//! Android Auto's Desktop Head Unit (DHU), the sanctioned head unit for testing Android Auto.
//!
//! It is SDK package `extras;google;auto`, under the `android-sdk-license` terms, which don't
//! allow redistribution: simee never bundles it. It is fetched from Google's repository only when
//! the user asks (`emu -provision-dhu`) and has accepted the license, into the SDK's own
//! `extras/google/auto` (the directory Android Studio's SDK Manager uses, so either tool's
//! install serves both).
use std::path::{Path, PathBuf};

use crate::{Download, Error, Host, License, Manifest, Result, Revision, Sdk};

/// The repository package.
pub const PACKAGE: &str = "extras;google;auto";
/// The license the package is under, and the one `-accept-licenses` accepts.
pub const SDK_LICENSE: &str = "android-sdk-license";
/// What simee records inside an install it made: the revision and channel it picked.
const MARKER: &str = ".simee-dhu";
/// The program the package holds.
pub const EXECUTABLE: &str = "desktop-head-unit";

/// Which releases to pick from: the stable channel (r2.0) or, as well, the beta (r2.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Channel {
    #[default]
    Stable,
    Beta,
}

impl Channel {
    fn name(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        }
    }

    /// The manifest channels it draws from; the newest revision among them wins.
    pub fn channels(self) -> &'static [&'static str] {
        match self {
            Channel::Stable => &["stable"],
            Channel::Beta => &["stable", "beta"],
        }
    }
}

/// `<sdk>/extras/google/auto`.
pub fn dir(sdk_root: &Path) -> PathBuf {
    sdk_root.join(crate::install_dir(PACKAGE))
}

/// The installed program, when the SDK has one.
pub fn executable(sdk_root: &Path) -> Option<PathBuf> {
    let p = dir(sdk_root).join(EXECUTABLE);
    p.is_file().then_some(p)
}

/// An installed DHU: what could be read of its revision and of the channel it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub revision: Option<Revision>,
    /// Known only for installs simee made; the SDK Manager's records don't say.
    pub channel: Option<Channel>,
}

/// The DHU in `sdk_root`, when its program is there. The revision comes from simee's marker,
/// else `source.properties` or `package.xml` (an SDK Manager install).
pub fn installed(sdk_root: &Path) -> Option<Installed> {
    executable(sdk_root)?;
    let dir = dir(sdk_root);
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let marked = |key: &str| {
        let text = read(MARKER)?;
        text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(str::to_owned))
    };
    let revision = marked("revision")
        .and_then(|r| r.parse().ok())
        .or_else(|| {
            read("source.properties").and_then(|t| t.lines().find_map(|l| l.trim().strip_prefix("Pkg.Revision=")?.trim().parse().ok()))
        })
        .or_else(|| read("package.xml").and_then(|t| package_xml_revision(&t)));
    let channel = marked("channel").and_then(|c| match c.as_str() {
        "stable" => Some(Channel::Stable),
        "beta" => Some(Channel::Beta),
        _ => None,
    });
    Some(Installed { revision, channel })
}

fn package_xml_revision(xml: &str) -> Option<Revision> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let rev = doc.descendants().find(|n| n.has_tag_name("revision"))?;
    let part = |name: &str| rev.children().find(|n| n.has_tag_name(name)).and_then(|n| n.text()?.trim().parse::<u32>().ok());
    Some(Revision { major: part("major")?, minor: part("minor").unwrap_or(0), micro: part("micro").unwrap_or(0), preview: part("preview") })
}

impl Plan {
    /// Whether the DHU installed in `sdk_root` is the one this plan would install: the same
    /// revision and, for an install simee made, the same channel. Anything else (an upgrade,
    /// `-dhu-beta`, or an install whose revision can't be read) wants an install.
    pub fn is_installed(&self, sdk_root: &Path) -> bool {
        installed(sdk_root).is_some_and(|i| i.revision == Some(self.download.revision) && i.channel.is_none_or(|c| c == self.channel))
    }
}

/// What a request for the DHU resolved to.
#[derive(Clone, Debug)]
pub struct Plan {
    pub download: Download,
    pub channel: Channel,
    pub license: Option<License>,
}

/// The DHU archive for `host` in `channel`. A host without one fails naming the hosts that have.
pub fn resolve(manifest: &Manifest, host: Host, channel: Channel) -> Result<Plan> {
    let channels = channel.channels();
    let download = match manifest.resolve_in(PACKAGE, host, channels) {
        Err(Error::NoArchive(..)) => {
            let have: Vec<String> = manifest.hosts_of(PACKAGE, channels).iter().map(Host::to_string).collect();
            return Err(Error::Command(format!(
                "Google publishes no Android Auto Desktop Head Unit for {host}; it has one for {}",
                have.join(", ")
            )));
        }
        r => r?,
    };
    Ok(Plan { download, channel, license: manifest.license_of(PACKAGE).cloned() })
}

impl Sdk {
    /// Installs the DHU `plan` resolved to: verified, unpacked beside a temporary name, the
    /// program made executable (the docs say to, and zip may not keep the bit), then moved into
    /// place whole. `accepted` names the license ids the user accepted; the package's must be one.
    pub fn install_dhu(&self, plan: &Plan, accepted: &[&str]) -> Result<PathBuf> {
        if let Some(l) = &plan.license
            && !accepted.contains(&l.id.as_str())
        {
            return Err(Error::License(l.id.clone()));
        }
        self.install_with(&plan.download, true, |top| {
            make_executable(&top.join(EXECUTABLE))?;
            let marker = top.join(MARKER);
            let text = format!("revision={}\nchannel={}\n", plan.download.revision, plan.channel.name());
            std::fs::write(&marker, text).map_err(|e| Error::Io(marker, e))
        })
    }
}

/// Sets the executable bits (the docs say to; zip may not keep them). Windows has none: the file
/// only has to exist.
fn make_executable(path: &Path) -> Result<()> {
    let io = |e| Error::Io(path.to_path_buf(), e);
    // The archive is untrusted: a symlink here would send the chmod to its target.
    let meta = std::fs::symlink_metadata(path).map_err(io)?;
    if !meta.is_file() {
        return Err(Error::Command(format!("{} in the archive is not a regular file", path.display())));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o755)).map_err(io)?;
    }
    let _ = meta;
    Ok(())
}
