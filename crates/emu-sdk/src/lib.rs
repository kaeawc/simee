//! Provisions an Android SDK without Java, sdkmanager or avdmanager.
//!
//! Packages (platform-tools, system images) are resolved from Google's repository manifests,
//! downloaded with macOS's own `/usr/bin/curl` (resumable, into `<sdk>/.downloads`), checked
//! against the manifest's size and sha1, unpacked with `/usr/bin/unzip` into a temporary
//! directory under the SDK and renamed into the standard layout. Shelling out to the system's
//! curl and unzip keeps a TLS stack and a zip implementation out of the binary. The host
//! renderer is unpacked out of the `emulator` package the same way, into simee's own directory
//! (see [`renderer`]). Other hosts (Linux, for preparing images and AVDs) use the `curl` and
//! `unzip` on their `PATH`.

pub mod dhu;
mod manifest;
pub mod renderer;

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

pub use manifest::{
    Archive, Download, Host, License, Manifest, RemotePackage, Revision, SysImagePath, install_dir, parse_site_list, system_image_path,
};

/// Google's SDK repository.
pub const REPO_URL: &str = "https://dl.google.com/android/repository/";

/// The repository to provision from: `$SDK_TEST_BASE_URL` (the variable sdkmanager honours, for
/// mirrors and offline use; `file://` works), else [`REPO_URL`].
pub fn repository_base() -> String {
    match std::env::var("SDK_TEST_BASE_URL") {
        Ok(base) if !base.is_empty() => {
            if base.ends_with('/') {
                base
            } else {
                base + "/"
            }
        }
        _ => REPO_URL.to_owned(),
    }
}

/// The main manifest (platform-tools, platforms, build-tools, emulator...).
pub fn repository_url() -> String {
    format!("{}repository2-3.xml", repository_base())
}

/// The list of system-image manifests, one per tag family.
pub fn sites_url() -> String {
    format!("{}addons_list-5.xml", repository_base())
}

/// The system-image manifest that carries `tag`. `default` lives under `sys-img/android/`, 16 KB
/// page-size variants (`google_apis_ps16k`) share their base tag's manifest, and every Android XR
/// image (`google-xr`, `android-xr-*`, `ai-glasses`, or the `android-xr` tag they carry) is
/// under `sys-img/google_xr/`.
pub fn sys_img_url(tag: &str) -> String {
    let dir = match tag {
        "default" => "android",
        t if sim_core::form_factor::is_xr_tag(t) => "google_xr",
        t => t.strip_suffix("_ps16k").unwrap_or(t),
    };
    format!("{}sys-img/{dir}/sys-img2-3.xml", repository_base())
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("bad manifest: {0}")]
    Manifest(String),
    #[error("no package '{0}' in {1}")]
    UnknownPackage(String, String),
    #[error("no stable archive of '{0}' for {1}")]
    NoArchive(String, String),
    #[error("{0}")]
    Command(String),
    #[error("{path}: {what} mismatch (expected {expected}, got {got})")]
    Verify { path: PathBuf, what: &'static str, expected: String, got: String },
    #[error("{0}: expected exactly one top-level directory in the archive, found {1} entries")]
    Layout(PathBuf, usize),
    #[error("package license '{0}' not accepted")]
    License(String),
    #[error("{0}: no emulator/lib64/{1} in the archive")]
    NoRenderer(PathBuf, &'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |e| Error::Io(path.to_path_buf(), e)
}

/// Lowercase hex sha1 of `data`.
pub fn sha1_hex(data: &[u8]) -> String {
    sha1_smol::Sha1::from(data).digest().to_string()
}

/// Lowercase hex sha1 of a file, streamed.
pub fn sha1_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = sha1_smol::Sha1::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(h.digest().to_string());
        }
        h.update(&buf[..n]);
    }
}

/// Human-readable byte count (`16.6 MB`, `1.74 GB`).
pub fn format_size(bytes: u64) -> String {
    const MB: f64 = 1e6;
    let b = bytes as f64;
    if b >= 1e9 { format!("{:.2} GB", b / 1e9) } else { format!("{:.1} MB", b / MB) }
}

/// Moves the unpacked directory `src` to `dest`, replacing whatever is there. The old directory
/// is set aside first and put back if the move fails, so `dest` is never left half-replaced.
fn replace_dir(src: &Path, dest: &Path, aside_dir: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(io(parent))?;
    }
    if dest.symlink_metadata().is_err() {
        return std::fs::rename(src, dest).map_err(io(dest));
    }
    // Under the downloads directory, not beside `dest`: a leftover must not look like a package.
    std::fs::create_dir_all(aside_dir).map_err(io(aside_dir))?;
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let aside = aside_dir.join(format!("{name}.old-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&aside);
    std::fs::rename(dest, &aside).map_err(io(dest))?;
    if let Err(e) = std::fs::rename(src, dest) {
        let _ = std::fs::rename(&aside, dest);
        return Err(io(dest)(e));
    }
    if let Err(e) = std::fs::remove_dir_all(&aside) {
        eprintln!("warning: could not remove the replaced {} ({e}); delete it by hand", aside.display());
    }
    Ok(())
}

/// Whether an archive member name is absolute or climbs out with `..`.
fn unsafe_member(name: &str) -> bool {
    let b = name.as_bytes();
    name.starts_with(['/', '\\']) || (b.len() >= 2 && b[1] == b':') || name.split(['/', '\\']).any(|c| c == "..")
}

/// Links the directory `target` (relative to `link`'s parent, or absolute) at `link`. On Windows
/// that is a directory symlink, which needs Developer Mode or an elevated process.
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    return std::os::windows::fs::symlink_dir(target, link);
}

/// A directory removed on drop.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A system tool: macOS's own copy in `/usr/bin` (never a `PATH` lookup there), else `name` as
/// found on the `PATH`.
fn host_tool(name: &str) -> PathBuf {
    if cfg!(target_os = "macos") { Path::new("/usr/bin").join(name) } else { PathBuf::from(name) }
}

/// An SDK root being provisioned.
#[derive(Clone, Debug)]
pub struct Sdk {
    root: PathBuf,
    /// Cached manifests older than this are fetched again.
    pub manifest_max_age: Duration,
    /// Silence curl's progress meter and our own progress lines.
    pub quiet: bool,
    pub curl: PathBuf,
    pub unzip: PathBuf,
}

impl Sdk {
    /// Opens (creating if needed) the SDK at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Result<Sdk> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(io(&root))?;
        Ok(Sdk { root, manifest_max_age: Duration::from_secs(24 * 3600), quiet: false, curl: host_tool("curl"), unzip: host_tool("unzip") })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Download cache: manifests and in-flight archives (`<file>.part`).
    pub fn downloads(&self) -> PathBuf {
        self.root.join(".downloads")
    }

    fn say(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{msg}");
        }
    }

    /// Whether a package is installed at `dir` (relative to the SDK root), as the SDK tools see it.
    pub fn is_installed(&self, dir: &Path) -> bool {
        let d = self.root.join(dir);
        d.join("source.properties").is_file() || d.join("package.xml").is_file()
    }

    fn run(&self, cmd: &mut Command) -> Result<std::process::ExitStatus> {
        cmd.status().map_err(|e| self.tool_error(Path::new(cmd.get_program()), e))
    }

    /// Why a host tool couldn't run; a missing one says what to install.
    fn tool_error(&self, program: &Path, e: std::io::Error) -> Error {
        if e.kind() == std::io::ErrorKind::NotFound {
            let name = program.file_name().unwrap_or(program.as_os_str()).to_string_lossy();
            Error::Command(format!("`{}` not found: install {name} (e.g. `apt install {name}`)", program.display()))
        } else {
            Error::Command(format!("{}: {e}", program.display()))
        }
    }

    /// Fetches (or reads from the daily cache) and parses a manifest.
    pub fn manifest(&self, url: &str) -> Result<Manifest> {
        let base = &url[..url.rfind('/').map_or(0, |i| i + 1)];
        Manifest::parse(&self.fetch_cached(url)?, base)
    }

    /// A manifest's text, refreshed when the cached copy is older than `manifest_max_age`. A
    /// failed refresh falls back to a stale cached copy.
    fn fetch_cached(&self, url: &str) -> Result<String> {
        self.fetch_cached_as(url, "-sSfL")
    }

    /// [`Sdk::fetch_cached`] for a URL that may well not exist: curl stays silent about it.
    fn probe_cached(&self, url: &str) -> Result<String> {
        self.fetch_cached_as(url, "-sfL")
    }

    fn fetch_cached_as(&self, url: &str, curl_flags: &str) -> Result<String> {
        let dl = self.downloads();
        std::fs::create_dir_all(&dl).map_err(io(&dl))?;
        let name: String = url.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
        let cache = dl.join(name);
        let age = std::fs::metadata(&cache).and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
        if age.is_none_or(|a| a > self.manifest_max_age) {
            let part = cache.with_extension("part");
            let status = self.run(Command::new(&self.curl).args([curl_flags, "--retry", "3", "-o"]).arg(&part).arg(url))?;
            if status.success() {
                std::fs::rename(&part, &cache).map_err(io(&cache))?;
            } else if cache.is_file() {
                self.say(&format!("warning: could not refresh {url} ({status}); using the cached copy"));
            } else {
                return Err(Error::Command(format!("fetching {url} failed ({status})")));
            }
        }
        std::fs::read_to_string(&cache).map_err(io(&cache))
    }

    /// A manifest at a URL that may not exist (a tag's guessed site).
    fn probe_manifest(&self, url: &str) -> Result<Manifest> {
        let base = &url[..url.rfind('/').map_or(0, |i| i + 1)];
        Manifest::parse(&self.probe_cached(url)?, base)
    }

    /// Every system-image manifest for `tag`, or for all tags (from `addons_list-5.xml`).
    pub fn system_image_manifests(&self, tag: Option<&str>) -> Result<Vec<Manifest>> {
        // A tag's own site when it has one; variants live on another tag's site
        // (`android-wear-signed` on `android-wear`'s), so then every site.
        if let Some(t) = tag
            && let Ok(m) = self.probe_manifest(&sys_img_url(t))
        {
            return Ok(vec![m]);
        }
        self.all_system_image_manifests()
    }

    /// Every system image site the repository's site list names.
    fn all_system_image_manifests(&self) -> Result<Vec<Manifest>> {
        let urls = parse_site_list(&self.fetch_cached(&sites_url())?, &repository_base())?;
        // A listed site that fails to load is skipped, not fatal: the others may have the image.
        Ok(urls.iter().filter_map(|u| self.manifest(u).ok()).collect())
    }

    /// The download for system image `api`/`tag`/`abi` (see
    /// [`Manifest::resolve_system_image`]), from the tag's own site, else from whichever site
    /// lists it.
    pub fn find_system_image(&self, api: &str, tag: &str, abi: &str, host: Host) -> Result<Download> {
        let guess = self.probe_manifest(&sys_img_url(tag)).and_then(|m| m.resolve_system_image(api, tag, abi, host));
        match guess {
            Ok(d) => Ok(d),
            Err(first) => {
                self.all_system_image_manifests()?.iter().find_map(|m| m.resolve_system_image(api, tag, abi, host).ok()).ok_or(first)
            }
        }
    }

    /// Downloads `d` (resuming a partial download), verifies its size and sha1, and unpacks it
    /// into `d.install_dir()`. Returns the installed directory. Replaces any incomplete install
    /// already there; callers skip installed packages with [`Sdk::is_installed`].
    pub fn install(&self, d: &Download) -> Result<PathBuf> {
        self.install_with(d, false, |_| Ok(()))
    }

    /// [`Sdk::install`], calling `prepare` on the unpacked package before it moves into place
    /// (a failure there leaves the existing install alone). With `allow_flat`, an archive whose
    /// files sit at the top, not under one directory, is the package itself.
    pub(crate) fn install_with(&self, d: &Download, allow_flat: bool, prepare: impl FnOnce(&Path) -> Result<()>) -> Result<PathBuf> {
        let part = self.fetch_verified(d)?;
        // Disk images stream out of the archive into sparse files: most of a system image is
        // zeros (8.1 of TV 34's 8.6 GB), which unzip would write out in full. Everything else
        // unzips as usual.
        let images: Vec<String> = self.archive_members(&part)?.into_iter().filter(|m| m.ends_with(".img")).collect();
        let tmp = self.unpack(d, false, |dest| {
            let mut args: Vec<OsString> = vec!["-q".into(), part.clone().into(), "-d".into(), dest.into()];
            if !images.is_empty() {
                args.push("-x".into());
                args.extend(images.iter().map(OsString::from));
            }
            args
        })?;
        for member in &images {
            let out = tmp.0.join(member);
            let (len, data) = self.unzip_sparse(&part, member, &out)?;
            if len > data {
                self.say(&format!("{member}: {} written, {} of zeros left as holes", format_size(data), format_size(len - data)));
            }
        }
        let entries: Vec<PathBuf> = std::fs::read_dir(&tmp.0)
            .map_err(io(&tmp.0))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().is_some_and(|n| n != "__MACOSX"))
            .collect();
        let top = match entries.as_slice() {
            [one] if one.is_dir() => one.clone(),
            [_, ..] if allow_flat => tmp.0.clone(),
            _ => return Err(Error::Layout(part, entries.len())),
        };
        prepare(&top)?;
        if !top.join("source.properties").is_file() {
            self.say(&format!("warning: {} has no source.properties", d.file_name()));
        }
        let dest = self.root.join(d.install_dir());
        replace_dir(&top, &dest, &self.downloads())?;
        // The unpacked package is the cache; a system image archive is close to 2 GB.
        let _ = std::fs::remove_file(&part);
        Ok(dest)
    }

    /// Downloads the `emulator` package `d` and unpacks only the renderer
    /// ([`renderer::INCLUDE`] minus [`renderer::EXCLUDE`]) into
    /// `<sdk>/simee/renderer/<revision>` with a [`renderer::MARKER`], points
    /// [`renderer::CURRENT`] at it and removes older revisions. The archive is deleted.
    pub fn install_renderer(&self, d: &Download) -> Result<renderer::Installed> {
        let part = self.fetch_verified(d)?;
        // LICENSE or NOTICE.txt may be missing from some revision; the backend may not.
        let tmp = self.unpack(d, true, |dest| renderer::unzip_args(&part, dest))?;
        let top = tmp.0.join("emulator");
        if !top.join("lib64").join(renderer::BACKEND).is_file() {
            let _ = std::fs::remove_file(&part);
            return Err(Error::NoRenderer(part, renderer::BACKEND));
        }
        let marker = renderer::Marker { revision: d.revision, sha1: d.sha1.clone(), url: d.url.clone() };
        let m = top.join(renderer::MARKER);
        std::fs::write(&m, marker.to_string()).map_err(io(&m))?;

        let dest = renderer::dir(&self.root, d.revision);
        replace_dir(&top, &dest, &self.downloads())?;
        let _ = std::fs::remove_file(&part);

        // Repoint `current` atomically, then drop superseded revisions (a running emu keeps the
        // libraries it has already mapped).
        let root = renderer::root(&self.root);
        let current = root.join(renderer::CURRENT);
        let link = root.join(format!(".{}.tmp", renderer::CURRENT));
        let _ = std::fs::remove_file(&link);
        symlink_dir(Path::new(&d.revision.to_string()), &link).map_err(io(&link))?;
        // Windows won't rename a link over another, so there `current` is briefly absent.
        #[cfg(windows)]
        let _ = std::fs::remove_dir(&current);
        std::fs::rename(&link, &current).map_err(io(&current))?;
        for old in renderer::installed(&self.root).into_iter().filter(|i| i.dir != dest) {
            self.say(&format!("removing renderer r{}", old.marker.revision));
            let _ = std::fs::remove_dir_all(&old.dir);
        }
        Ok(renderer::Installed { dir: dest, marker })
    }

    /// Downloads `d` into the download cache (resuming a partial download) and verifies its size
    /// and sha1. Returns the archive's path; a bad archive is deleted.
    fn fetch_verified(&self, d: &Download) -> Result<PathBuf> {
        let dl = self.downloads();
        std::fs::create_dir_all(&dl).map_err(io(&dl))?;
        let part = dl.join(format!("{}.part", d.file_name()));
        self.fetch_archive(d, &part)?;

        let got = std::fs::metadata(&part).map_err(io(&part))?.len();
        if got != d.size {
            let _ = std::fs::remove_file(&part);
            return Err(Error::Verify { path: part, what: "size", expected: d.size.to_string(), got: got.to_string() });
        }
        self.say(&format!("verifying {}", d.file_name()));
        let sha = sha1_file(&part).map_err(io(&part))?;
        if sha != d.sha1 {
            let _ = std::fs::remove_file(&part);
            return Err(Error::Verify { path: part, what: "sha1", expected: d.sha1.clone(), got: sha });
        }
        Ok(part)
    }

    /// The archive's member paths (`unzip -Z1`), directories excluded.
    fn archive_members(&self, archive: &Path) -> Result<Vec<String>> {
        let out = Command::new(&self.unzip).arg("-Z1").arg(archive).output().map_err(|e| self.tool_error(&self.unzip, e))?;
        if !out.status.success() {
            return Err(Error::Command(format!("listing {} failed ({})", archive.display(), out.status)));
        }
        let names: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
        if let Some(bad) = names.iter().find(|n| unsafe_member(n)) {
            return Err(Error::Command(format!("{} holds the member '{bad}', which would leave the install directory", archive.display())));
        }
        Ok(names.into_iter().filter(|l| !l.ends_with('/')).collect())
    }

    /// Unzips one member to `out` (`unzip -p`), writing only its non-zero 4 KiB blocks: zero runs
    /// stay holes, which read back as zeros. Returns the file's length and the bytes written.
    fn unzip_sparse(&self, archive: &Path, member: &str, out: &Path) -> Result<(u64, u64)> {
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir).map_err(io(dir))?;
        }
        let mut child = Command::new(&self.unzip)
            .arg("-p")
            .arg(archive)
            .arg(member)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| self.tool_error(&self.unzip, e))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Command("unzip gave no output pipe".into()))?;
        let file = std::fs::File::create(out).map_err(io(out))?;
        let written = write_sparse(stdout, &file).map_err(io(out));
        let status = child.wait().map_err(io(out))?;
        let (len, data) = written?;
        if !status.success() {
            return Err(Error::Command(format!("unzip of {member} from {} failed ({status})", archive.display())));
        }
        Ok((len, data))
    }

    /// Runs unzip with the arguments `args` builds for a fresh temporary directory under the
    /// download cache, and returns that directory. Exit status 11 (a pattern matched nothing) is
    /// accepted when `allow_unmatched`; the caller then checks for the files it needs.
    fn unpack(&self, d: &Download, allow_unmatched: bool, args: impl FnOnce(&Path) -> Vec<OsString>) -> Result<TempDir> {
        self.say(&format!("unpacking {}", d.file_name()));
        let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|t| t.subsec_nanos()).unwrap_or(0);
        let tmp = TempDir(self.downloads().join(format!("unpack-{}-{nanos}", std::process::id())));
        std::fs::create_dir_all(&tmp.0).map_err(io(&tmp.0))?;
        let status = self.run(Command::new(&self.unzip).args(args(&tmp.0)))?;
        if !status.success() && !(allow_unmatched && status.code() == Some(11)) {
            return Err(Error::Command(format!("unzip of {} failed ({status})", d.file_name())));
        }
        Ok(tmp)
    }

    fn fetch_archive(&self, d: &Download, part: &Path) -> Result<()> {
        let have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        if have > d.size {
            std::fs::remove_file(part).map_err(io(part))?;
        } else if have == d.size {
            return Ok(()); // complete; verified by the caller
        }
        self.say(&format!("downloading {} ({})", d.url, format_size(d.size)));
        for attempt in 0..2 {
            let mut cmd = Command::new(&self.curl);
            cmd.args(["-fL", "--retry", "3", "-C", "-", "-o"]).arg(part).arg(&d.url);
            if self.quiet {
                cmd.arg("-sS");
            }
            let status = self.run(&mut cmd)?;
            if status.success() {
                return Ok(());
            }
            // 33: the server can't resume (no byte ranges); start over once.
            if attempt == 0 && status.code() == Some(33) {
                let _ = std::fs::remove_file(part);
                continue;
            }
            return Err(Error::Command(format!("downloading {} failed ({status}); rerun to resume", d.url)));
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing;
#[cfg(test)]
mod tests;

/// Copies `src` into the empty file `dst`, writing only blocks that aren't all zeros (4 KiB, the
/// APFS and ext4 block size), and deallocates the zero runs: they read back as zeros without
/// taking space. Returns the length and how many bytes were written.
///
/// ext4 leaves skipped ranges as holes by itself; APFS allocates them (zero-filled) whether the
/// file grows by a write past its end or by truncating, so on macOS each zero run is punched out
/// explicitly. A filesystem without holes keeps the file dense.
pub fn write_sparse(mut src: impl std::io::Read, dst: &std::fs::File) -> std::io::Result<(u64, u64)> {
    use sim_core::hostfs::FileExt;
    const BLOCK: usize = 4096;
    let mut buf = vec![0u8; 1 << 20];
    let (mut offset, mut written) = (0u64, 0u64);
    // Where the current zero run started (file offset), across chunks.
    let mut zeros: Option<u64> = None;
    // Best effort: the range already reads as zeros either way. Linux filesystems leave skipped
    // ranges as holes by themselves, and each punch is a journal transaction there, so only APFS.
    let punch = |start: u64, end: u64| {
        if cfg!(target_os = "macos") {
            let _ = emu_hvf::host::punch_hole(dst, start, end - start);
        }
    };
    loop {
        let mut n = 0;
        while n < buf.len() {
            match src.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        dst.set_len(offset + n as u64)?;
        // Each run of non-zero blocks goes out in one write.
        let mut run: Option<usize> = None;
        for (i, block) in buf[..n].chunks(BLOCK).enumerate() {
            let at = offset + (i * BLOCK) as u64;
            // A short final block is written: holes cover whole blocks.
            let zero = block.len() == BLOCK && block.iter().all(|&b| b == 0);
            match (zero, run) {
                (false, None) => {
                    run = Some(i * BLOCK);
                    if let Some(z) = zeros.take() {
                        punch(z, at);
                    }
                }
                (true, Some(start)) => {
                    dst.write_all_at(&buf[start..i * BLOCK], offset + start as u64)?;
                    written += (i * BLOCK - start) as u64;
                    run = None;
                    zeros = Some(at);
                }
                (true, None) => {
                    zeros.get_or_insert(at);
                }
                (false, Some(_)) => {}
            }
        }
        if let Some(start) = run {
            dst.write_all_at(&buf[start..n], offset + start as u64)?;
            written += (n - start) as u64;
        }
        offset += n as u64;
        if n < buf.len() {
            break;
        }
    }
    if let Some(z) = zeros {
        punch(z, offset);
    }
    Ok((offset, written))
}
