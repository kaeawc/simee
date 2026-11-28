//! Copy-on-write file clones (#203): the one way emu-avd copies a disk image.
//!
//! Every disk an AVD is made from (a golden's user data, cache and key, `-clone-avd`'s copies of
//! a stopped AVD's, `boot.prop`) goes through [`clone_file`]: `clonefile(2)` on APFS and a
//! `FICLONE` reflink on Linux file systems that have them (Btrfs, XFS, bcachefs), so a new device
//! takes milliseconds and no disk until either side writes. Elsewhere (HFS+, ext4, another
//! volume: `ENOTSUP`, `EOPNOTSUPP`, `EXDEV`) it copies only the source's data extents, so a
//! sparse image (a user data is mostly holes) stays sparse.
//!
//! Metadata, whichever way the file was made: the source's permission bits and extended
//! attributes (what `clonefile` copies, and what the `std::fs::copy` this replaces copied on
//! macOS). An attribute the destination refuses (a `security.*` one without privilege) is
//! skipped, and so is an attribute list that keeps growing while it is read. Ownership and
//! timestamps are the new file's own.
//!
//! `clonefile` also carries ACLs and BSD file flags, but the sparse fallback does not: the
//! `fcopyfile` the old `std::fs::copy` used on macOS copied them, this does not. Nothing needs
//! them on an AVD's disk images (plain files only the emulator opens), so they are dropped.

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Error, Result};

/// What a data copy is reported as: it keeps holes where the platform can find them.
const COPY_KIND: CloneKind = if cfg!(any(target_vendor = "apple", target_os = "linux")) { CloneKind::Sparse } else { CloneKind::Copy };

/// How a file was copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloneKind {
    /// A copy-on-write clone (`clonefile` on APFS, `FICLONE` on Linux): no data copied, blocks
    /// shared until either side writes.
    Reflink,
    /// Only the data extents copied: the file stays as sparse as the original.
    Sparse,
    /// The whole file copied, holes written out: on platforms with neither clones nor
    /// `SEEK_DATA` (neither macOS nor Linux). `golden` ranks it the slowest.
    Copy,
}

impl CloneKind {
    pub fn label(self) -> &'static str {
        match self {
            CloneKind::Reflink => "reflink",
            CloneKind::Sparse => "sparse copy",
            CloneKind::Copy => "copy",
        }
    }
}

/// Copies `from` to the new file `to`: a copy-on-write clone where the file system has them,
/// else a copy that keeps the file's holes.
///
/// `to` must not exist. A failed clone leaves no half-written file behind, and never removes a
/// `to` this call did not create (one another process made in the meantime).
pub fn clone_file(from: &Path, to: &Path) -> Result<CloneKind> {
    clone_with(from, to, reflink, copy_data, copy_metadata)
}

/// [`clone_file`] for a file that must never be seen half-written, even if the process is killed
/// mid-copy (`encryptionkey.img`: the guest boots with whatever is at the final name). The copy is
/// made under a name unique to this call beside `to` (`.<name>.partial-<pid>-<n>`, created with
/// `O_EXCL` like every clone target), flushed to disk, then renamed into place.
///
/// The name is unique because `-read-only` boots skip the instance lock: two of them can run this
/// for one AVD at once, and a shared temporary name would let one rename the other's half-written
/// copy into place (#431). Each call now installs only its own whole file; of two racing calls the
/// later rename wins, and both files are complete.
///
/// `to` must not exist, and a symlink there, dangling or not, counts as existing: it is refused
/// with an error that says so, never written through (what `std::fs::copy` did) nor replaced by
/// the rename. A leftover of the old fixed `.partial` name is removed first; a killed run's
/// unique partial is not (nothing knows it is dead), it is just an unused sparse file.
pub fn clone_file_atomic(from: &Path, to: &Path) -> Result<CloneKind> {
    clone_atomic_with(from, to, reflink, copy_data, copy_metadata)
}

/// [`clone_file_atomic`] with the clone steps given, so tests can stall or fail a copy after its
/// partial file exists.
fn clone_atomic_with(from: &Path, to: &Path, reflink: Reflink, copy: CopyData, meta: CopyMeta) -> Result<CloneKind> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    if let Ok(meta) = to.symlink_metadata() {
        return Err(if meta.is_symlink() {
            Error::Config(format!(
                "{} is a symlink (to {}); refusing to write through or replace it: remove it or point it at a real file",
                to.display(),
                std::fs::read_link(to).map_or_else(|_| "?".into(), |t| t.display().to_string())
            ))
        } else {
            Error::Io(to.to_path_buf(), io::ErrorKind::AlreadyExists.into())
        });
    }
    let name = to.file_name().ok_or_else(|| Error::Config(format!("{} has no file name", to.display())))?;
    let name = name.to_string_lossy();
    // The fixed name an earlier version used: remove its leftover (a link is removed, not followed).
    let _ = std::fs::remove_file(to.with_file_name(format!(".{name}.partial")));
    let tmp = to.with_file_name(format!(".{name}.partial-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
    // `clone_with` refuses an existing `tmp` and creates it exclusively; a failure removes it.
    let kind = clone_with(from, &tmp, reflink, copy, meta)?;
    let installed = File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| io(&tmp, e))
        .and_then(|()| std::fs::rename(&tmp, to).map_err(|e| io(to, e)));
    if let Err(e) = installed {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // Make the rename itself durable; best effort (a directory can't always be opened or synced).
    if let Some(dir) = to.parent().filter(|d| !d.as_os_str().is_empty())
        && let Ok(d) = File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(kind)
}

/// The clone attempt: makes `to` from `src`, or fails leaving no file this attempt made at `to`.
type Reflink = fn(&File, &Path) -> io::Result<()>;
/// The data copy into the empty `dst` (`src` is `from`, `dst` is `to`).
type CopyData = fn(&File, &File, &Path, &Path) -> Result<()>;

/// The mode and attribute copy onto the new file.
type CopyMeta = fn(&File, &File) -> io::Result<()>;

/// [`clone_file`] with the clone attempt, the data copy and the metadata copy given, so tests can
/// make them fail as an unsupported file system, a full disk or a refused `chmod` do.
fn clone_with(from: &Path, to: &Path, reflink: Reflink, copy: CopyData, meta: CopyMeta) -> Result<CloneKind> {
    if to.symlink_metadata().is_ok() {
        return Err(Error::Io(to.to_path_buf(), io::ErrorKind::AlreadyExists.into()));
    }
    clone_new_file(from, to, reflink, copy, meta)
}

/// A file this call made at `to`, removed again unless [`Created::keep`] is called. It exists
/// only once the clone or `create_new` succeeded, so a `to` somebody else made (an `AlreadyExists`
/// from the race after the check above) is never ours to remove.
struct Created<'a>(Option<&'a Path>);

impl<'a> Created<'a> {
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for Created<'_> {
    fn drop(&mut self) {
        if let Some(to) = self.0 {
            let _ = std::fs::remove_file(to);
        }
    }
}

fn clone_new_file(from: &Path, to: &Path, reflink: Reflink, copy: CopyData, meta: CopyMeta) -> Result<CloneKind> {
    let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
    let src = File::open(from).map_err(|e| io(from, e))?;
    // Any failure falls back to the copy: a cause other than the file system (a missing
    // directory, no space) fails the copy too, with its own error. A failed clone leaves no file.
    let (created, dst, kind) = match reflink(&src, to) {
        Ok(()) => {
            let created = Created(Some(to));
            if cfg!(target_vendor = "apple") {
                created.keep();
                return Ok(CloneKind::Reflink);
            }
            let dst = File::options().write(true).open(to).map_err(|e| io(to, e))?;
            (created, dst, CloneKind::Reflink)
        }
        Err(_) => {
            let dst = File::options().write(true).create_new(true).open(to).map_err(|e| io(to, e))?;
            let created = Created(Some(to));
            copy(&src, &dst, from, to)?;
            (created, dst, COPY_KIND)
        }
    };
    meta(&src, &dst).map_err(|e| io(to, e))?;
    created.keep();
    Ok(kind)
}

/// `clonefile(2)`: `to` shares `src`'s blocks and carries its mode, ACL and extended attributes.
#[cfg(target_vendor = "apple")]
fn reflink(src: &File, to: &Path) -> io::Result<()> {
    rustix::fs::fclonefileat(src, rustix::fs::CWD, to, rustix::fs::CloneFlags::empty()).map_err(io::Error::from)
}

/// `FICLONE` into a new file, removed again if the file system can't.
#[cfg(target_os = "linux")]
fn reflink(src: &File, to: &Path) -> io::Result<()> {
    let dst = File::options().write(true).create_new(true).open(to)?;
    let cloned = rustix::fs::ioctl_ficlone(&dst, src);
    drop(dst);
    if cloned.is_err() {
        let _ = std::fs::remove_file(to);
    }
    cloned.map_err(io::Error::from)
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
fn reflink(_: &File, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Copies `src`'s data extents into the empty `dst`, leaving its holes as holes. A file system
/// that can't report extents has its file copied whole.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn copy_data(src: &File, dst: &File, from: &Path, to: &Path) -> Result<()> {
    copy_extents(src, dst, from, to, &|f, whence| rustix::fs::seek(f, whence))
}

/// [`copy_data`] with `seek` given, so a test can make `SEEK_DATA`/`SEEK_HOLE` fail as a file
/// system without them does.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn copy_extents(
    src: &File,
    dst: &File,
    from: &Path,
    to: &Path,
    seek: &dyn Fn(&File, rustix::fs::SeekFrom) -> rustix::io::Result<u64>,
) -> Result<()> {
    use rustix::fs::SeekFrom;
    use std::os::unix::fs::FileExt;
    let io = |p: &Path, e| Error::Io(p.to_path_buf(), e);
    let len = src.metadata().map_err(|e| io(from, e))?.len();
    dst.set_len(len).map_err(|e| io(to, e))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    while pos < len {
        let (data, end) = match seek(src, SeekFrom::Data(pos)) {
            Ok(d) => (d, seek(src, SeekFrom::Hole(d)).map_or(len, |h| h.min(len))),
            // No data from `pos` on: the rest is a hole.
            Err(rustix::io::Errno::NXIO) => break,
            Err(_) => (pos, len),
        };
        let mut at = data;
        while at < end {
            let n = ((end - at) as usize).min(buf.len());
            src.read_exact_at(&mut buf[..n], at).map_err(|e| io(from, e))?;
            dst.write_all_at(&buf[..n], at).map_err(|e| io(to, e))?;
            at += n as u64;
        }
        pos = end;
    }
    Ok(())
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
fn copy_data(src: &File, dst: &File, from: &Path, _: &Path) -> Result<()> {
    io::copy(&mut &*src, &mut &*dst).map(drop).map_err(|e| Error::Io(from.to_path_buf(), e))
}

/// The source's permission bits and, best effort, its extended attributes.
fn copy_metadata(src: &File, dst: &File) -> io::Result<()> {
    dst.set_permissions(src.metadata()?.permissions())?;
    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    {
        use rustix::fs::{XattrFlags, fgetxattr, flistxattr, fsetxattr};
        let names = xattr_names(&mut |buf| flistxattr(src, buf), &mut || rustix::fs::flistxattr_query(src))?;
        let mut value = Vec::new();
        for name in names.split(|&b| b == 0).filter(|s| !s.is_empty()) {
            let Ok(size) = rustix::fs::fgetxattr_query(src, name) else { continue };
            value.resize(size, 0);
            if let Ok(got) = fgetxattr(src, name, &mut value[..]) {
                let _ = fsetxattr(dst, name, &value[..got], XattrFlags::empty());
            }
        }
    }
    Ok(())
}

/// The NUL-separated names of the source's extended attributes, best effort: a file system
/// without them, or a list that keeps changing, gives none.
#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn xattr_names(
    list: &mut dyn FnMut(&mut [u8]) -> rustix::io::Result<usize>,
    query: &mut dyn FnMut() -> rustix::io::Result<usize>,
) -> io::Result<Vec<u8>> {
    let mut names = vec![0u8; 4096];
    // The list can grow between the size query and the read (ERANGE again): ask again a few
    // times, then go without rather than fail the clone for a best-effort copy.
    for _ in 0..4 {
        match list(&mut names[..]) {
            Ok(n) => {
                names.truncate(n);
                return Ok(names);
            }
            Err(rustix::io::Errno::RANGE) => match query() {
                // Room for a little growth since the query.
                Ok(size) => names = vec![0u8; size + 256],
                Err(_) => break,
            },
            // No extended attributes here (a file system without them).
            Err(_) => break,
        }
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("emu-avd-clone-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn unsupported(_: &File, _: &Path) -> io::Result<()> {
        Err(rustix::io::Errno::NOTSUP.into())
    }

    fn cross_volume(_: &File, _: &Path) -> io::Result<()> {
        Err(rustix::io::Errno::XDEV.into())
    }

    /// A 256 MiB image that is almost all hole, with data at its start, middle and end.
    fn sparse_image(path: &Path) {
        let f = File::create(path).expect("src");
        f.set_len(256 << 20).expect("len");
        f.write_all_at(b"head", 0).expect("write");
        f.write_all_at(b"middle", 100 << 20).expect("write");
        f.write_all_at(b"tail", (256 << 20) - 4).expect("write");
    }

    fn write_at(path: &Path, at: u64, b: &[u8]) {
        File::options().write(true).open(path).expect("open").write_all_at(b, at).expect("write");
    }

    fn read_at(path: &Path, at: u64, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        File::open(path).expect("open").read_exact_at(&mut v, at).expect("read");
        v
    }

    /// Writes to either side after a clone never show on the other, whichever way it was made.
    fn assert_independent(src: &Path, dst: &Path) {
        write_at(dst, 0, b"DST!");
        assert_eq!(read_at(src, 0, 4), b"head", "the source is unchanged by a write to its clone");
        write_at(src, 100 << 20, b"SOURCE");
        assert_eq!(read_at(dst, 100 << 20, 6), b"middle", "the clone is unchanged by a write to its source");
        assert_eq!(read_at(dst, 0, 4), b"DST!");
        assert_eq!(read_at(src, 100 << 20, 6), b"SOURCE");
    }

    fn fs_type(dir: &Path) -> String {
        #[cfg(target_vendor = "apple")]
        {
            let s = rustix::fs::statfs(dir).expect("statfs");
            s.f_fstypename.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect()
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            let _ = dir;
            String::new()
        }
    }

    #[test]
    fn a_clone_is_independent_of_its_source_after_writes() {
        let dir = tmp("independent");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        sparse_image(&src);
        let kind = clone_file(&src, &dst).expect("clone");
        if fs_type(&dir) == "apfs" {
            assert_eq!(kind, CloneKind::Reflink, "APFS clones with clonefile");
        }
        assert_eq!(std::fs::read(&src).expect("src"), std::fs::read(&dst).expect("dst"));
        assert_independent(&src, &dst);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unsupported_clone_falls_back_to_a_sparse_copy_with_mode_and_xattrs() {
        for (name, attempt) in [("enotsup", unsupported as fn(&File, &Path) -> io::Result<()>), ("exdev", cross_volume)] {
            let dir = tmp(name);
            let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
            sparse_image(&src);
            std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).expect("chmod");
            let has_xattr = rustix::fs::setxattr(src.as_path(), "user.simee.test", b"kept", rustix::fs::XattrFlags::empty()).is_ok();
            assert_eq!(clone_with(&src, &dst, attempt, copy_data, copy_metadata).expect("copy"), CloneKind::Sparse, "{name}");
            let meta = std::fs::metadata(&dst).expect("meta");
            assert_eq!(meta.len(), 256 << 20, "{name}");
            assert_eq!(std::fs::read(&src).expect("src"), std::fs::read(&dst).expect("dst"), "{name}");
            assert!(meta.blocks() * 512 < 8 << 20, "{name}: the copy took {} bytes of a 256 MiB hole", meta.blocks() * 512);
            assert_eq!(meta.permissions().mode() & 0o7777, 0o640, "{name}: mode kept");
            if has_xattr {
                let mut v = [0u8; 16];
                let n = rustix::fs::getxattr(dst.as_path(), "user.simee.test", &mut v[..]).expect("xattr kept");
                assert_eq!(&v[..n], b"kept", "{name}");
            }
            assert_independent(&src, &dst);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_clone_never_overwrites_and_a_failed_one_leaves_nothing() {
        let dir = tmp("refuse");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        std::fs::write(&src, b"data").expect("src");
        std::fs::write(&dst, b"mine").expect("dst");
        assert!(clone_file(&src, &dst).is_err());
        assert!(clone_with(&src, &dst, unsupported, copy_data, copy_metadata).is_err());
        assert_eq!(std::fs::read(&dst).expect("dst"), b"mine", "an existing file is never replaced");
        let missing = dir.join("out.img");
        assert!(clone_file(&dir.join("missing.img"), &missing).is_err());
        assert!(clone_with(&src, &dir.join("no-such-dir/out.img"), unsupported, copy_data, copy_metadata).is_err());
        assert!(!missing.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Another process makes `to` after our existence check, then the clone fails with EEXIST.
    fn raced(_: &File, to: &Path) -> io::Result<()> {
        std::fs::write(to, b"theirs")?;
        Err(rustix::io::Errno::EXIST.into())
    }

    /// A copy that dies mid-way (a full disk): some bytes written, then an error.
    fn dies_mid_copy(_: &File, dst: &File, _: &Path, to: &Path) -> Result<()> {
        dst.write_all_at(b"partial", 0).expect("write");
        Err(Error::Io(to.to_path_buf(), rustix::io::Errno::NOSPC.into()))
    }

    #[test]
    fn a_file_another_process_created_first_is_never_removed() {
        let dir = tmp("raced");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        std::fs::write(&src, b"data").expect("src");
        assert!(clone_with(&src, &dst, raced, copy_data, copy_metadata).is_err());
        assert_eq!(std::fs::read(&dst).expect("theirs survives"), b"theirs");
        let _ = std::fs::remove_dir_all(dir);
    }

    fn no_reflink(_: &File, _: &Path) -> io::Result<()> {
        panic!("the clone was attempted")
    }

    fn no_copy(_: &File, _: &File, _: &Path, _: &Path) -> Result<()> {
        panic!("the copy was attempted")
    }

    fn no_meta(_: &File, _: &File) -> io::Result<()> {
        panic!("the metadata copy was attempted")
    }

    /// A metadata copy the destination refuses (a read-only file system, a lost race with `chmod`).
    fn meta_refused(_: &File, _: &File) -> io::Result<()> {
        Err(rustix::io::Errno::PERM.into())
    }

    /// An existing destination stops `clone_with` at its first check: nothing is attempted, so
    /// nothing can fail halfway through and remove or rewrite it. (The race that gets past the check
    /// is `a_file_another_process_created_first_is_never_removed`.)
    #[test]
    fn an_existing_destination_is_refused_before_anything_is_attempted() {
        let dir = tmp("preexisting");
        let src = dir.join("src.img");
        std::fs::write(&src, b"data").expect("src");
        let file = dir.join("file.img");
        std::fs::write(&file, b"mine").expect("file");
        let dangling = dir.join("dangling.img");
        std::os::unix::fs::symlink(dir.join("nowhere"), &dangling).expect("symlink");
        let directory = dir.join("dir.img");
        std::fs::create_dir(&directory).expect("dir");
        for dst in [&file, &dangling, &directory] {
            let err = clone_with(&src, dst, no_reflink, no_copy, no_meta).expect_err("refused");
            assert!(matches!(err, Error::Io(_, ref e) if e.kind() == io::ErrorKind::AlreadyExists), "{}: {err}", dst.display());
            assert!(clone_file(&src, dst).is_err(), "{}", dst.display());
        }
        assert_eq!(std::fs::read(&file).expect("file"), b"mine");
        assert!(dangling.symlink_metadata().expect("link").is_symlink() && !dir.join("nowhere").exists());
        assert!(directory.is_dir());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failure_mid_copy_removes_only_the_partial_copy() {
        let dir = tmp("mid-copy");
        let (src, dst, other) = (dir.join("src.img"), dir.join("dst.img"), dir.join("other.img"));
        std::fs::write(&src, b"data").expect("src");
        std::fs::write(&other, b"neighbour").expect("other");
        let err = clone_with(&src, &dst, unsupported, dies_mid_copy, no_meta).expect_err("fails");
        assert!(matches!(err, Error::Io(_, ref e) if e.raw_os_error() == Some(rustix::io::Errno::NOSPC.raw_os_error())), "{err}");
        assert!(dst.symlink_metadata().is_err(), "the half-written copy is removed");
        assert_eq!(std::fs::read(&src).expect("src"), b"data");
        assert_eq!(std::fs::read(&other).expect("other"), b"neighbour");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failed_metadata_copy_after_a_data_copy_removes_the_copy() {
        let dir = tmp("meta-after-copy");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        std::fs::write(&src, b"data").expect("src");
        assert!(clone_with(&src, &dst, unsupported, copy_data, meta_refused).is_err());
        assert!(dst.symlink_metadata().is_err(), "a copy without its mode is not left to boot");
        assert_eq!(std::fs::read(&src).expect("src"), b"data");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// What `FICLONE` leaves when it succeeds: a new `to` with `src`'s content.
    #[cfg(not(target_vendor = "apple"))]
    fn cloned(src: &File, to: &Path) -> io::Result<()> {
        let mut data = Vec::new();
        io::Read::read_to_end(&mut &*src, &mut data)?;
        std::fs::write(to, data)
    }

    /// `FICLONE` succeeded, but what it made can't be reopened for its metadata (here a link to
    /// nowhere stands in for a file that went away or was replaced).
    #[cfg(not(target_vendor = "apple"))]
    fn cloned_then_unopenable(_: &File, to: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(to.with_file_name("nowhere"), to)
    }

    // The clone-success paths below run only where `clone_new_file` reopens the destination: not
    // on macOS, where `clonefile` is the whole job.
    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn a_clone_that_cannot_be_reopened_is_removed() {
        let dir = tmp("reopen");
        let (src, dst, other) = (dir.join("src.img"), dir.join("dst.img"), dir.join("other.img"));
        std::fs::write(&src, b"data").expect("src");
        std::fs::write(&other, b"neighbour").expect("other");
        let err = clone_with(&src, &dst, cloned_then_unopenable, no_copy, no_meta).expect_err("reopen fails");
        assert!(matches!(err, Error::Io(ref p, _) if *p == dst), "{err}");
        assert!(dst.symlink_metadata().is_err(), "the clone is removed, link and all");
        assert_eq!(std::fs::read(&src).expect("src"), b"data");
        assert_eq!(std::fs::read(&other).expect("other"), b"neighbour");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn a_clone_whose_metadata_copy_fails_is_removed() {
        let dir = tmp("meta-after-clone");
        let (src, dst, other) = (dir.join("src.img"), dir.join("dst.img"), dir.join("other.img"));
        std::fs::write(&src, b"data").expect("src");
        std::fs::write(&other, b"neighbour").expect("other");
        let err = clone_with(&src, &dst, cloned, no_copy, meta_refused).expect_err("metadata fails");
        assert!(matches!(err, Error::Io(ref p, _) if *p == dst), "{err}");
        assert!(dst.symlink_metadata().is_err(), "a clone without its mode is removed");
        assert_eq!(std::fs::read(&src).expect("src"), b"data");
        assert_eq!(std::fs::read(&other).expect("other"), b"neighbour");
        // The same clone with its metadata copied is kept, and reported as a reflink.
        assert_eq!(clone_with(&src, &dst, cloned, no_copy, copy_metadata).expect("clone"), CloneKind::Reflink);
        assert_eq!(std::fs::read(&dst).expect("dst"), b"data");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Where the real-file-system tests run: `SIMEE_REFLINK_DIR` (a directory on Btrfs, XFS with
    /// reflinks or bcachefs) when set, else the temp directory.
    #[cfg(target_os = "linux")]
    fn reflink_dir(name: &str) -> PathBuf {
        match std::env::var_os("SIMEE_REFLINK_DIR") {
            Some(d) => {
                let dir = PathBuf::from(d).join(format!("emu-avd-clone-{name}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).expect("mkdir");
                dir
            }
            None => tmp(name),
        }
    }

    /// Whether `dir`'s file system does `FICLONE`.
    #[cfg(target_os = "linux")]
    fn supports_ficlone(dir: &Path) -> bool {
        let (a, b) = (dir.join("probe-a"), dir.join("probe-b"));
        std::fs::write(&a, vec![1u8; 8192]).expect("probe");
        let ok =
            File::open(&a).and_then(|s| File::create(&b).and_then(|d| rustix::fs::ioctl_ficlone(&d, &s).map_err(io::Error::from))).is_ok();
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        ok
    }

    /// The real `FICLONE` where the file system has it, and the real fallback where not: set
    /// `SIMEE_REFLINK_DIR` to a Btrfs or XFS directory to run the first half.
    #[cfg(target_os = "linux")]
    #[test]
    fn real_ficlone_clones_where_supported_and_cleans_up_on_every_failure() {
        let dir = reflink_dir("real-ficlone");
        let reflinks = supports_ficlone(&dir);
        if !reflinks {
            eprintln!("{} has no FICLONE: checking the fallback only (SIMEE_REFLINK_DIR picks a reflink file system)", dir.display());
        }
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        sparse_image(&src);
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).expect("chmod");
        // A real clone, mode kept, independent after writes.
        let kind = clone_file(&src, &dst).expect("clone");
        assert_eq!(kind, if reflinks { CloneKind::Reflink } else { CloneKind::Sparse });
        assert_eq!(std::fs::metadata(&dst).expect("meta").permissions().mode() & 0o7777, 0o640);
        assert_eq!(std::fs::read(&src).expect("src"), std::fs::read(&dst).expect("dst"));
        assert_independent(&src, &dst);
        // A pre-existing destination: refused up front, and the real `reflink` (a racing creator)
        // doesn't touch it either.
        let s = File::open(&src).expect("src");
        assert!(reflink(&s, &dst).is_err());
        assert!(clone_file(&src, &dst).is_err());
        assert_eq!(read_at(&dst, 0, 4), b"DST!", "the existing destination is untouched");
        std::fs::remove_file(&dst).expect("rm");
        // The metadata copy fails after the real clone or copy: the destination goes.
        assert!(clone_with(&src, &dst, reflink, copy_data, meta_refused).is_err());
        assert!(dst.symlink_metadata().is_err(), "no destination after a failed metadata copy");
        // A failed real clone (a missing directory) leaves nothing and says why.
        let gone = dir.join("no-such-dir/out.img");
        assert!(clone_file(&src, &gone).is_err() && gone.symlink_metadata().is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A real `EXDEV`: `FICLONE` across two file systems falls back to the sparse copy.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_cross_volume_clone_falls_back_to_a_sparse_copy() {
        let (a, b) = (tmp("exdev-a"), PathBuf::from("/dev/shm").join(format!("emu-avd-clone-exdev-{}", std::process::id())));
        if std::fs::create_dir_all(&b).is_err()
            || MetadataExt::dev(&std::fs::metadata(&a).expect("a")) == MetadataExt::dev(&std::fs::metadata(&b).expect("b"))
        {
            eprintln!("no second file system at /dev/shm: skipped");
            let _ = std::fs::remove_dir_all(a);
            let _ = std::fs::remove_dir_all(b);
            return;
        }
        let (src, dst) = (a.join("src.img"), b.join("dst.img"));
        sparse_image(&src);
        assert_eq!(clone_file(&src, &dst).expect("copy"), CloneKind::Sparse);
        assert_eq!(std::fs::read(&src).expect("src"), std::fs::read(&dst).expect("dst"));
        let _ = std::fs::remove_dir_all(a);
        let _ = std::fs::remove_dir_all(b);
    }

    #[test]
    fn an_atomic_clone_lands_whole_and_replaces_a_killed_runs_leftover() {
        let dir = tmp("atomic");
        let (src, dst) = (dir.join("src.img"), dir.join("key.img"));
        sparse_image(&src);
        let leftover = dir.join(".key.img.partial");
        std::fs::write(&leftover, b"half a key from a killed run").expect("leftover");
        clone_file_atomic(&src, &dst).expect("clone");
        assert!(!leftover.exists(), "the old fixed temporary name is cleared");
        assert!(partials(&dir).is_empty(), "no temporary name is left behind");
        assert_eq!(std::fs::read(&src).expect("src"), std::fs::read(&dst).expect("dst"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The temporary files an atomic clone of `key.img` left in `dir`.
    fn partials(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".key.img.partial"))
            .collect()
    }

    /// A data copy that writes half of `src`, then fails: the partial file exists by then.
    fn copy_half_then_fail(src: &File, dst: &File, _: &Path, to: &Path) -> Result<()> {
        let mut half = vec![0; 8];
        src.read_exact_at(&mut half, 0).expect("read");
        dst.write_all_at(&half, 0).expect("write");
        assert!(to.exists(), "the partial file is on disk when the copy fails");
        Err(Error::Io(to.to_path_buf(), io::Error::other("disk full")))
    }

    fn unsupported_reflink(_: &File, _: &Path) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    #[test]
    fn a_failed_atomic_clone_leaves_neither_the_key_nor_a_partial() {
        let dir = tmp("atomic-fail");
        let dst = dir.join("key.img");
        assert!(clone_file_atomic(&dir.join("missing.img"), &dst).is_err());
        assert!(dst.symlink_metadata().is_err());
        assert!(partials(&dir).is_empty());
        // Failing after the partial file was written: still no key, and the partial is removed.
        let src = dir.join("src.img");
        std::fs::write(&src, b"0123456789abcdef").expect("src");
        let err = clone_atomic_with(&src, &dst, unsupported_reflink, copy_half_then_fail, copy_metadata).expect_err("disk full");
        assert!(err.to_string().contains("disk full"), "{err}");
        assert!(dst.symlink_metadata().is_err(), "a half-written key is never installed");
        assert!(partials(&dir).is_empty(), "{:?}", partials(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A data copy that writes `src` in two halves with a pause between them, so a racing clone
    /// runs while this one's partial file holds half a key.
    fn copy_slowly(src: &File, dst: &File, _: &Path, _: &Path) -> Result<()> {
        let len = src.metadata().expect("meta").len();
        let mut all = vec![0; len as usize];
        src.read_exact_at(&mut all, 0).expect("read");
        dst.set_len(len).expect("len");
        let (first, second) = all.split_at(all.len() / 2);
        dst.write_all_at(first, 0).expect("write");
        // The 'A' writer is quick and the 'B' writer slow: with a shared temporary name, A's
        // rename would install B's half-written file.
        std::thread::sleep(std::time::Duration::from_millis(if all[0] == b'A' { 30 } else { 300 }));
        dst.write_all_at(second, first.len() as u64).expect("write");
        Ok(())
    }

    #[test]
    fn concurrent_atomic_clones_never_install_a_partial_key() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tmp("atomic-race");
        let dst = dir.join("key.img");
        let len = 4096;
        let (a, b) = (dir.join("a.img"), dir.join("b.img"));
        std::fs::write(&a, vec![b'A'; len]).expect("a");
        std::fs::write(&b, vec![b'B'; len]).expect("b");
        let done = AtomicBool::new(false);
        let seen = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|s| {
            // Whatever is at the final name, whenever it is read, is one whole key.
            s.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    if let Ok(bytes) = std::fs::read(&dst) {
                        seen.lock().expect("seen").push(bytes);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
            let writers: Vec<_> = [&a, &b]
                .into_iter()
                .enumerate()
                .map(|(i, src)| {
                    let dst = &dst;
                    s.spawn(move || {
                        // B starts while A's partial file exists.
                        std::thread::sleep(std::time::Duration::from_millis(10 * i as u64));
                        clone_atomic_with(src, dst, unsupported_reflink, copy_slowly, copy_metadata)
                    })
                })
                .collect();
            for w in writers {
                // The loser of the rename race may see the winner's file: either way no half key.
                let _ = w.join().expect("writer");
            }
            done.store(true, Ordering::Relaxed);
        });
        let installed = std::fs::read(&dst).expect("a key is installed");
        assert!(installed == vec![b'A'; len] || installed == vec![b'B'; len], "the installed key is one whole key");
        for bytes in seen.into_inner().expect("seen") {
            assert!(bytes == vec![b'A'; len] || bytes == vec![b'B'; len], "a partial key was visible: {} bytes", bytes.len());
        }
        assert!(partials(&dir).is_empty(), "no temporary file is left: {:?}", partials(&dir));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_atomic_clone_refuses_a_symlink_even_a_dangling_one() {
        let dir = tmp("atomic-symlink");
        let (src, dst, target) = (dir.join("src.img"), dir.join("key.img"), dir.join("elsewhere.img"));
        std::fs::write(&src, b"key").expect("src");
        std::os::unix::fs::symlink(&target, &dst).expect("symlink");
        let msg = clone_file_atomic(&src, &dst).expect_err("refused").to_string();
        assert!(msg.contains("symlink") && msg.contains("elsewhere.img") && msg.contains("remove it"), "{msg}");
        assert!(!target.exists(), "nothing is written through the link");
        assert_eq!(std::fs::read_link(&dst).expect("still a link"), target, "the link is not replaced");
        assert!(partials(&dir).is_empty());
        // A link to a real file is refused as well, and that file keeps its content.
        std::fs::write(&target, b"theirs").expect("target");
        assert!(clone_file_atomic(&src, &dst).is_err());
        assert_eq!(std::fs::read(&target).expect("target"), b"theirs");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_atomic_clone_refuses_an_existing_file() {
        let dir = tmp("atomic-exists");
        let (src, dst) = (dir.join("src.img"), dir.join("key.img"));
        std::fs::write(&src, b"new").expect("src");
        std::fs::write(&dst, b"mine").expect("dst");
        assert!(matches!(clone_file_atomic(&src, &dst), Err(Error::Io(_, e)) if e.kind() == io::ErrorKind::AlreadyExists));
        assert_eq!(std::fs::read(&dst).expect("dst"), b"mine");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A sparse file whose last extent is a hole: data at the start and at 1 MiB, then nothing.
    fn trailing_hole_image(path: &Path, len: u64) {
        let f = File::create(path).expect("src");
        f.set_len(len).expect("len");
        f.write_all_at(b"head", 0).expect("write");
        f.write_all_at(b"mid", 1 << 20).expect("write");
    }

    #[test]
    fn a_trailing_hole_keeps_the_size_and_stays_a_hole() {
        let dir = tmp("trailing-hole");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        trailing_hole_image(&src, 256 << 20);
        let s = File::open(&src).expect("src");
        let d = File::create_new(&dst).expect("dst");
        copy_data(&s, &d, &src, &dst).expect("copy");
        let meta = std::fs::metadata(&dst).expect("meta");
        assert_eq!(meta.len(), 256 << 20, "the size includes the trailing hole");
        assert!(meta.blocks() * 512 < 8 << 20, "the hole took {} bytes", meta.blocks() * 512);
        assert_eq!(read_at(&dst, 0, 4), b"head");
        assert_eq!(read_at(&dst, 1 << 20, 3), b"mid");
        assert_eq!(read_at(&dst, (256 << 20) - 8, 8), [0u8; 8]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_system_without_seek_data_has_the_whole_file_copied() {
        let dir = tmp("no-seek-data");
        let (src, dst) = (dir.join("src.img"), dir.join("dst.img"));
        trailing_hole_image(&src, 3 << 20);
        let s = File::open(&src).expect("src");
        let d = File::create_new(&dst).expect("dst");
        let no_seek = |_: &File, _: rustix::fs::SeekFrom| Err(rustix::io::Errno::INVAL);
        copy_extents(&s, &d, &src, &dst, &no_seek).expect("whole copy");
        assert_eq!(std::fs::metadata(&dst).expect("meta").len(), 3 << 20);
        assert_eq!(std::fs::read(&src).expect("src"), std::fs::read(&dst).expect("dst"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_growing_xattr_list_is_retried_then_skipped_not_fatal() {
        let list = |calls: &mut u32, buf: &mut [u8], settle_at: u32| {
            *calls += 1;
            if *calls < settle_at {
                return Err(rustix::io::Errno::RANGE);
            }
            buf[..7].copy_from_slice(b"user.a\0");
            Ok(7)
        };
        // The list grew again between the size query and the read, once.
        let mut calls = 0;
        let names = xattr_names(&mut |b| list(&mut calls, b, 3), &mut || Ok(16)).expect("retried");
        assert_eq!(names, b"user.a\0");
        // It never settles: no attributes, and the clone goes on.
        let mut calls = 0;
        let names = xattr_names(&mut |b| list(&mut calls, b, u32::MAX), &mut || Ok(16)).expect("skipped");
        assert!(names.is_empty());
        // A failing size query is skipped too.
        let mut calls = 0;
        let names = xattr_names(&mut |b| list(&mut calls, b, u32::MAX), &mut || Err(rustix::io::Errno::RANGE)).expect("skipped");
        assert!(names.is_empty());
    }
}
