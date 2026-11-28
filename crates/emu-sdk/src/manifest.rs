//! Google's SDK repository manifests (`repository2-3.xml`, `sys-img2-3.xml`, `addons_list-5.xml`).
//!
//! Only what provisioning needs is read: each `remotePackage`'s path, revision, channel and
//! archives (host filters, size, sha1, URL).

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use roxmltree::Node;

use crate::{Error, Result};

/// The machine an archive must run on, in the manifests' terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Host {
    pub os: &'static str,
    pub arch: &'static str,
}

impl Host {
    /// Apple silicon, the only host simee boots guests on (and so the renderer's host).
    pub const MACOS_ARM64: Host = Host { os: "macosx", arch: "aarch64" };

    /// The hosts the repository publishes archives for.
    pub const KNOWN: [Host; 5] = [
        Host { os: "macosx", arch: "aarch64" },
        Host { os: "macosx", arch: "x64" },
        Host { os: "linux", arch: "x64" },
        Host { os: "linux", arch: "aarch64" },
        Host { os: "windows", arch: "x64" },
    ];

    /// The machine this binary runs on, for host tools such as platform-tools.
    pub fn current() -> Host {
        let os = if cfg!(target_os = "macos") {
            "macosx"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else {
            "linux"
        };
        Host { os, arch: if cfg!(target_arch = "aarch64") { "aarch64" } else { "x64" } }
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.arch)
    }
}

/// `major.minor.micro`, optionally a preview (`-rcN`), which sorts before its release.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Revision {
    pub major: u32,
    pub minor: u32,
    pub micro: u32,
    pub preview: Option<u32>,
}

impl Revision {
    fn key(&self) -> (u32, u32, u32, u32) {
        (self.major, self.minor, self.micro, self.preview.unwrap_or(u32::MAX))
    }
}

impl Ord for Revision {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

impl PartialOrd for Revision {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Parses what [`Revision`]'s `Display` prints (`37`, `37.3.10`, `38.0.0-rc2`).
impl std::str::FromStr for Revision {
    type Err = Error;

    fn from_str(s: &str) -> Result<Revision> {
        let bad = || Error::Manifest(format!("bad revision '{s}'"));
        let (version, preview) = match s.split_once("-rc") {
            Some((v, p)) => (v, Some(p.parse().map_err(|_| bad())?)),
            None => (s, None),
        };
        let parts = version.split('.').map(|p| p.parse::<u32>().map_err(|_| bad())).collect::<Result<Vec<_>>>()?;
        match parts.as_slice() {
            [major] => Ok(Revision { major: *major, minor: 0, micro: 0, preview }),
            [major, minor] => Ok(Revision { major: *major, minor: *minor, micro: 0, preview }),
            [major, minor, micro] => Ok(Revision { major: *major, minor: *minor, micro: *micro, preview }),
            _ => Err(bad()),
        }
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.major)?;
        if self.minor != 0 || self.micro != 0 || self.preview.is_some() {
            write!(f, ".{}.{}", self.minor, self.micro)?;
        }
        if let Some(p) = self.preview {
            write!(f, "-rc{p}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Archive {
    /// As written in the manifest: usually relative to the manifest's directory.
    pub url: String,
    pub size: u64,
    pub sha1: String,
    pub host_os: Option<String>,
    pub host_arch: Option<String>,
}

impl Archive {
    pub fn runs_on(&self, host: Host) -> bool {
        self.host_os.as_deref().is_none_or(|o| o == host.os) && self.host_arch.as_deref().is_none_or(|a| a == host.arch)
    }
}

#[derive(Clone, Debug)]
pub struct RemotePackage {
    /// `platform-tools`, `system-images;android-36;google_apis;arm64-v8a`, ...
    pub path: String,
    pub revision: Revision,
    /// Channel name (`stable`, `beta`, `dev`, `canary`); a package without a channelRef is stable.
    pub channel: String,
    /// `type-details/api-level` for platform-specific packages.
    pub api_level: Option<String>,
    /// A system image's `type-details/tag/id`s, which can differ from the tag in its path
    /// (`system-images;android-34;google-xr;arm64-v8a` is tagged `android-xr`).
    pub tags: Vec<String>,
    pub archives: Vec<Archive>,
    /// The id of the license the package is under (`uses-license`).
    pub license: Option<String>,
}

/// A license the repository asks users to accept before installing a package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct License {
    pub id: String,
    pub text: String,
}

/// A package version picked for a host: everything needed to fetch, verify and install it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    pub path: String,
    pub revision: Revision,
    pub api_level: Option<String>,
    /// System image tags (see [`RemotePackage::tags`]).
    pub tags: Vec<String>,
    /// Absolute URL.
    pub url: String,
    pub size: u64,
    pub sha1: String,
}

impl Download {
    /// Install location relative to the SDK root: the package path with `;` as separators.
    pub fn install_dir(&self) -> PathBuf {
        install_dir(&self.path)
    }

    /// The archive's file name (last URL segment).
    pub fn file_name(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or(&self.url)
    }
}

pub fn install_dir(package_path: &str) -> PathBuf {
    package_path.split(';').collect()
}

#[derive(Clone, Debug)]
pub struct Manifest {
    /// Directory URL the manifest was fetched from, ending in `/`; archive URLs are relative to it.
    pub base_url: String,
    pub packages: Vec<RemotePackage>,
    pub licenses: Vec<License>,
}

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name(name))
}

fn text<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    child(n, name).and_then(|c| c.text()).map(str::trim)
}

fn num<T: std::str::FromStr>(n: Node<'_, '_>, name: &str) -> Option<T> {
    text(n, name)?.parse().ok()
}

fn parse_doc(xml: &str) -> Result<roxmltree::Document<'_>> {
    roxmltree::Document::parse(xml).map_err(|e| Error::Manifest(e.to_string()))
}

impl Manifest {
    pub fn parse(xml: &str, base_url: &str) -> Result<Manifest> {
        let doc = parse_doc(xml)?;
        let root = doc.root_element();
        let channels: BTreeMap<&str, &str> =
            root.children().filter(|c| c.has_tag_name("channel")).filter_map(|c| Some((c.attribute("id")?, c.text()?.trim()))).collect();
        let licenses = root
            .children()
            .filter(|c| c.has_tag_name("license"))
            .filter_map(|l| Some(License { id: l.attribute("id")?.to_string(), text: l.text().unwrap_or_default().trim().to_string() }))
            .collect();
        let mut packages = Vec::new();
        for pkg in root.children().filter(|c| c.has_tag_name("remotePackage")) {
            let path = pkg.attribute("path").ok_or_else(|| Error::Manifest("remotePackage without a path".into()))?;
            let revision = child(pkg, "revision")
                .map(|r| Revision {
                    major: num(r, "major").unwrap_or(0),
                    minor: num(r, "minor").unwrap_or(0),
                    micro: num(r, "micro").unwrap_or(0),
                    preview: num(r, "preview"),
                })
                .unwrap_or_default();
            let channel = match child(pkg, "channelRef").and_then(|c| c.attribute("ref")) {
                Some(id) => channels.get(id).copied().unwrap_or(id),
                None => "stable",
            };
            let archives = child(pkg, "archives")
                .into_iter()
                .flat_map(|a| a.children().filter(|c| c.has_tag_name("archive")))
                .filter_map(|a| {
                    let complete = child(a, "complete")?;
                    Some(Archive {
                        url: text(complete, "url")?.to_string(),
                        size: num(complete, "size")?,
                        sha1: text(complete, "checksum")?.to_ascii_lowercase(),
                        host_os: text(a, "host-os").map(String::from),
                        host_arch: text(a, "host-arch").map(String::from),
                    })
                })
                .collect();
            let details = child(pkg, "type-details");
            let tags = details
                .into_iter()
                .flat_map(|d| d.children().filter(|c| c.has_tag_name("tag")))
                .filter_map(|t| text(t, "id"))
                .map(String::from)
                .collect();
            packages.push(RemotePackage {
                path: path.to_string(),
                revision,
                channel: channel.to_string(),
                api_level: details.and_then(|d| text(d, "api-level")).map(String::from),
                tags,
                archives,
                license: child(pkg, "uses-license").and_then(|l| l.attribute("ref")).map(String::from),
            });
        }
        Ok(Manifest { base_url: base_url.to_string(), packages, licenses })
    }

    fn absolute(&self, url: &str) -> String {
        if url.contains("://") { url.to_string() } else { format!("{}{url}", self.base_url) }
    }

    fn candidates(&self, host: Host) -> impl Iterator<Item = Download> + '_ {
        self.candidates_in(host, &["stable"])
    }

    fn candidates_in<'a>(&'a self, host: Host, channels: &'a [&str]) -> impl Iterator<Item = Download> + 'a {
        self.packages.iter().filter(|p| channels.contains(&p.channel.as_str())).flat_map(move |p| {
            p.archives.iter().filter(move |a| a.runs_on(host)).map(move |a| Download {
                path: p.path.clone(),
                revision: p.revision,
                api_level: p.api_level.clone(),
                tags: p.tags.clone(),
                url: self.absolute(&a.url),
                size: a.size,
                sha1: a.sha1.clone(),
            })
        })
    }

    /// The newest stable version of `path` with an archive for `host`.
    pub fn resolve(&self, path: &str, host: Host) -> Result<Download> {
        if !self.packages.iter().any(|p| p.path == path) {
            return Err(Error::UnknownPackage(path.to_string(), self.base_url.clone()));
        }
        self.candidates(host)
            .filter(|d| d.path == path)
            .max_by_key(|d| d.revision)
            .ok_or_else(|| Error::NoArchive(path.to_string(), host.to_string()))
    }

    /// [`Manifest::resolve`] over the channels in `channels` (`stable`, `beta`...): the newest
    /// version in any of them.
    pub fn resolve_in(&self, path: &str, host: Host, channels: &[&str]) -> Result<Download> {
        if !self.packages.iter().any(|p| p.path == path) {
            return Err(Error::UnknownPackage(path.to_string(), self.base_url.clone()));
        }
        self.candidates_in(host, channels)
            .filter(|d| d.path == path)
            .max_by_key(|d| d.revision)
            .ok_or_else(|| Error::NoArchive(path.to_string(), host.to_string()))
    }

    /// The license package `path` is under, with the text the repository shows for it.
    pub fn license_of(&self, path: &str) -> Option<&License> {
        let id = self.packages.iter().filter(|p| p.path == path).find_map(|p| p.license.as_deref())?;
        self.licenses.iter().find(|l| l.id == id)
    }

    /// Every host that has an archive for some version of `path` in `channels`.
    pub fn hosts_of(&self, path: &str, channels: &[&str]) -> Vec<Host> {
        let mut hosts: Vec<Host> = Vec::new();
        for p in self.packages.iter().filter(|p| p.path == path && channels.contains(&p.channel.as_str())) {
            for a in &p.archives {
                for host in Host::KNOWN.iter().copied().filter(|h| a.runs_on(*h)) {
                    if !hosts.contains(&host) {
                        hosts.push(host);
                    }
                }
            }
        }
        hosts
    }

    /// The system image for `api`, `tag` and `abi`. When `api` is a bare level (`34`) that the
    /// site publishes only at extension levels (Automotive's `34-ext9`), the newest of those.
    pub fn resolve_system_image(&self, api: &str, tag: &str, abi: &str, host: Host) -> Result<Download> {
        let path = system_image_path(api, tag, abi);
        match self.resolve(&path, host) {
            Err(Error::UnknownPackage(..)) if !api.contains("-ext") => {
                let prefix = format!("{}-ext", api.strip_prefix("android-").unwrap_or(api));
                let ext = |d: &Download| {
                    let s = SysImagePath::parse(&d.path).filter(|s| s.tag == tag && s.abi == abi)?;
                    s.api().strip_prefix(&prefix)?.parse::<u32>().ok()
                };
                let newest = self.latest(host).into_iter().filter_map(|d| Some((ext(&d)?, d))).max_by_key(|(n, _)| *n);
                newest.map(|(_, d)| d).ok_or_else(|| Error::UnknownPackage(path, self.base_url.clone()))
            }
            r => r,
        }
    }

    /// The newest stable version of every package that has an archive for `host`, by path.
    pub fn latest(&self, host: Host) -> Vec<Download> {
        let mut best: BTreeMap<String, Download> = BTreeMap::new();
        for d in self.candidates(host) {
            match best.get(&d.path) {
                Some(have) if have.revision >= d.revision => {}
                _ => {
                    best.insert(d.path.clone(), d);
                }
            }
        }
        best.into_values().collect()
    }

    /// System images for `abi` (and `tag`, if given) with a stable archive for `host`, newest per
    /// package, ordered by API level. `tag` is the tag in the package path; when no path carries it,
    /// the packages' own tags (`-tag android-xr` finds the `google-xr` images).
    pub fn system_images(&self, host: Host, tag: Option<&str>, abi: &str) -> Vec<Download> {
        let images: Vec<Download> =
            self.latest(host).into_iter().filter(|d| SysImagePath::parse(&d.path).is_some_and(|s| s.abi == abi)).collect();
        let in_path = |d: &Download, t: &str| SysImagePath::parse(&d.path).is_some_and(|s| s.tag == t);
        let mut out: Vec<Download> = match tag {
            None => images,
            Some(t) if images.iter().any(|d| in_path(d, t)) => images.into_iter().filter(|d| in_path(d, t)).collect(),
            Some(t) => images.into_iter().filter(|d| d.tags.iter().any(|x| x == t)).collect(),
        };
        let key = |d: &Download| SysImagePath::parse(&d.path).map(|s| s.api_key());
        out.sort_by(|a, b| key(a).cmp(&key(b)).then_with(|| a.path.cmp(&b.path)));
        out
    }
}

/// System-image manifest URLs listed in `addons_list-5.xml`, made absolute against `base_url`.
pub fn parse_site_list(xml: &str, base_url: &str) -> Result<Vec<String>> {
    let doc = parse_doc(xml)?;
    Ok(doc
        .root_element()
        .children()
        .filter(|c| c.has_tag_name("site"))
        .filter(|s| s.attributes().any(|a| a.name() == "type" && a.value().ends_with("sysImgSiteType")))
        .filter_map(|s| text(s, "url"))
        .map(|u| if u.contains("://") { u.to_string() } else { format!("{base_url}{u}") })
        .collect())
}

/// The parts of a `system-images;<platform>;<tag>;<abi>` package path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SysImagePath<'a> {
    /// `android-36`, `android-36.1`, `android-CANARY`, ...
    pub platform: &'a str,
    pub tag: &'a str,
    pub abi: &'a str,
}

impl<'a> SysImagePath<'a> {
    pub fn parse(path: &'a str) -> Option<SysImagePath<'a>> {
        match path.split(';').collect::<Vec<_>>().as_slice() {
            ["system-images", platform, tag, abi] => Some(SysImagePath { platform, tag, abi }),
            _ => None,
        }
    }

    /// The platform without its `android-` prefix: `36`, `36.1`, `36-ext19`, `CANARY`.
    pub fn api(&self) -> &'a str {
        self.platform.strip_prefix("android-").unwrap_or(self.platform)
    }

    /// Numeric order: `36` < `36-ext19` < `36.1` < `37.0`; non-numeric platforms sort last.
    pub fn api_key(&self) -> (u32, u32, String) {
        let api = self.api();
        let split = |s: &'a str| s.find(|c: char| !c.is_ascii_digit()).map_or((s, ""), |i| s.split_at(i));
        let (major, rest) = split(api);
        let Ok(major) = major.parse() else { return (u32::MAX, 0, api.to_string()) };
        let (minor, rest) = match rest.strip_prefix('.') {
            Some(r) => {
                let (m, rest) = split(r);
                (m.parse().unwrap_or(0), rest)
            }
            None => (0, rest),
        };
        (major, minor, rest.to_string())
    }
}

/// `system-images;android-<api>;<tag>;<abi>`; `api` may be `36`, `36.1`, `CANARY` or already
/// carry the `android-` prefix.
pub fn system_image_path(api: &str, tag: &str, abi: &str) -> String {
    let platform = if api.starts_with("android-") { api.to_string() } else { format!("android-{api}") };
    format!("system-images;{platform};{tag};{abi}")
}
