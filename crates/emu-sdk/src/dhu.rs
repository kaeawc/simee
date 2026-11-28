//! Android Auto's Desktop Head Unit (DHU), the sanctioned head unit for testing Android Auto.
//!
//! It is SDK package `extras;google;auto`, under the `android-sdk-license` terms, which don't
//! allow redistribution: simee never bundles it. It is fetched from Google's repository only when
//! the user asks (`emu -provision-dhu`) and has accepted the license, into the SDK's own
//! `extras/google/auto` (the directory Android Studio's SDK Manager uses, so either tool's
//! install serves both).
use std::path::{Path, PathBuf};

use crate::{Download, Error, Host, License, Manifest, Result, Sdk};

/// The repository package.
pub const PACKAGE: &str = "extras;google;auto";
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

/// What a request for the DHU resolved to.
#[derive(Clone, Debug)]
pub struct Plan {
    pub download: Download,
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
    Ok(Plan { download, license: manifest.license_of(PACKAGE).cloned() })
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
        self.install_with(&plan.download, true, |top| make_executable(&top.join(EXECUTABLE)))
    }
}

/// Sets the executable bits (the docs say to; zip may not keep them). Windows has none: the file
/// only has to exist.
fn make_executable(path: &Path) -> Result<()> {
    let io = |e| Error::Io(path.to_path_buf(), e);
    let meta = std::fs::metadata(path).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o755)).map_err(io)?;
    }
    let _ = meta;
    Ok(())
}
