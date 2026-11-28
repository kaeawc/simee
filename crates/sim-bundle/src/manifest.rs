//! `manifest.json`: what a bundle holds and how to put it back together.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sim_core::platform::Platform;

use crate::{Error, Result};

/// The format's major version. A reader refuses any other: a new major may change what an
/// existing field means. Additions that old readers can ignore keep the major.
pub const FORMAT: u32 = 1;

/// Disk images are cut into chunks of this size (the last one shorter). Fixed boundaries suit
/// disks, written in place: an offset never shifts, so content-defined chunking would find the
/// same boundaries, and a small chunk limits what one dirty block drags into a bundle.
pub const CHUNK_SIZE: u64 = 1 << 20;

/// The only chunk compression in format 1: each chunk is one LZ4 block (no frame or size header;
/// the manifest gives each chunk's length).
pub const COMPRESSION: &str = "lz4";

/// What `resume` means in format 1: the receiver boots the disks cold (no RAM state).
pub const COLD_BOOT: &str = "cold-boot";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    /// The simee that wrote it.
    pub simee_version: String,
    pub platform: Platform,
    pub device: Device,
    /// What the device runs on, matched by content on import (never by path).
    pub identity: Identity,
    pub captures: Captures,
    /// What the receiver does with it: [`COLD_BOOT`] in format 1.
    pub resume: String,
    /// What in the bundle may be a secret, and whether an export can leave it out.
    pub secrets: Vec<Secret>,
    /// What an import shares with the device it came from.
    pub guest_identity: GuestIdentity,
    /// What the export changed or left out to make the bundle host-independent (a `config.ini`
    /// key that held a host path), so the receiver can tell.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<String>,
    pub chunk_size: u64,
    pub compression: String,
    pub disks: Vec<Disk>,
    pub files: Vec<File>,
    /// A file tree's directories and symlinks, and its files' modes (an iOS data directory):
    /// what `disks` and `files` don't say. Empty for a bundle of disk images.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tree: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// The name it had where it was exported (an import names it anew).
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form_factor: Option<String>,
}

/// What a device needs on the receiving host: the system image (Android), or the runtime and
/// device type (iOS).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Identity {
    Android(AndroidImage),
    Ios(IosSimulator),
}

/// An iOS simulator's device type and runtime, by the service's identifiers. An import needs the
/// same runtime build; the version and name only say what to install when it is missing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IosSimulator {
    /// `com.apple.CoreSimulator.SimDeviceType.iPhone-17`.
    pub device_type: String,
    /// `com.apple.CoreSimulator.SimRuntime.iOS-26-1`.
    pub runtime: String,
    /// The runtime's build (`23B80`).
    pub build: String,
    /// The runtime's version (`26.1`).
    pub version: String,
    /// The runtime's name (`iOS 26.1`).
    pub runtime_name: String,
}

/// What a [tree](Manifest::tree) entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Dir,
    /// A regular file, stored as the disk (or file) at the same path.
    File,
    Symlink,
}

/// One directory, file or symlink of a file tree, at its path in the bundle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub kind: EntryKind,
    /// Permission bits (at most `0o7777`) of a directory or file; none for a symlink.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// A symlink's target, as the link holds it (relative, or a path inside the device).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// An Android system image by content: `source.properties`'s API level, tag, ABI and revision,
/// the BLAKE3 of the files that boot it, and `build.prop`'s fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndroidImage {
    pub api: String,
    pub tag: String,
    pub abi: String,
    pub revision: String,
    /// The SDK package that held it where it was exported (`system-images;android-36;google_apis;arm64-v8a`):
    /// what to install when no image matches. Never used to find the image.
    pub package: String,
    /// File name (`system.img`, `vendor.img`, `kernel-ranchu`, `ramdisk.img`) to its BLAKE3.
    pub files: BTreeMap<String, String>,
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Captures {
    pub disks: bool,
    pub ram: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secret {
    /// A path in the bundle, or what it is (`accounts`).
    pub item: String,
    pub what: String,
    pub excludable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestIdentity {
    /// `duplicated`: an import is the same device to the guest's apps and services as the one it
    /// came from, so two of them on one network look like one device.
    pub on_import: String,
    /// The identifiers that come along (`ro.serialno`, `android_id`).
    pub identifiers: Vec<String>,
}

/// A disk image: its chunks, in order, covering `size` bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    /// Where it goes, relative to the device directory (`simee/userdata.img`).
    pub path: String,
    pub size: u64,
    /// BLAKE3 of all `size` bytes.
    pub blake3: String,
    /// Reserved for base deltas (#171): the disk this one is a delta against. A format 1 reader
    /// that meets one refuses the bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Base>,
    pub extents: Vec<Extent>,
}

/// The disk a delta is against, checked by its hash before use (#171).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    pub kind: String,
    pub identity: String,
    pub blake3: String,
}

/// `len` bytes of a disk at `offset`: one stored chunk (`hash`), zeros (all-zero data and holes,
/// never stored; an import leaves a hole), or the same range of the base (`from_base`, #171).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extent {
    pub offset: u64,
    pub len: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub zero: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub from_base: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A small file (`config.ini`): its chunks, each [`Manifest::chunk_size`] but the last.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    pub path: String,
    pub size: u64,
    pub blake3: String,
    pub chunks: Vec<String>,
}

impl Extent {
    pub fn chunk(offset: u64, len: u64, hash: String) -> Extent {
        Extent { offset, len, hash: Some(hash), zero: false, from_base: false }
    }

    pub fn zero(offset: u64, len: u64) -> Extent {
        Extent { offset, len, hash: None, zero: true, from_base: false }
    }
}

/// Whether `path` is safe to join to a destination directory: relative, `/`-separated, with no
/// empty, `.` or `..` component, no backslash and no NUL.
pub fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', '\0'])
        && path.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Whether `h` is a BLAKE3 hash as the manifest writes it: 64 lowercase hex digits.
pub fn is_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Manifest {
    /// Parses and [validates](Manifest::validate) `manifest.json`. The format is checked before
    /// anything else, so a newer major is refused as such rather than as malformed.
    pub fn parse(bytes: &[u8]) -> Result<Manifest> {
        /// Only `format`: serde skips everything else without building it, so a hostile manifest
        /// costs no more than its own bytes before it is refused.
        #[derive(Deserialize)]
        struct FormatOnly {
            format: Option<u64>,
        }
        let only: FormatOnly = serde_json::from_slice(bytes).map_err(|e| Error::Manifest(e.to_string()))?;
        match only.format {
            Some(f) if f == u64::from(FORMAT) => {}
            Some(f) => return Err(Error::Format(f)),
            None => return Err(Error::Manifest("no `format`".into())),
        }
        let m: Manifest = serde_json::from_slice(bytes).map_err(|e| Error::Manifest(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    pub fn to_json(&self) -> Vec<u8> {
        let mut v = serde_json::to_vec_pretty(self).unwrap_or_default();
        v.push(b'\n');
        v
    }

    /// Checks everything an import relies on: the format, every path, that each disk's extents
    /// cover it exactly in order, every hash's shape, and each file's chunk count.
    pub fn validate(&self) -> Result<()> {
        let bad = |s: String| Err(Error::Manifest(s));
        if self.format != FORMAT {
            return Err(Error::Format(u64::from(self.format)));
        }
        if self.compression != COMPRESSION {
            return bad(format!("unknown compression `{}`", self.compression));
        }
        if !self.chunk_size.is_power_of_two() || !(4096..=64 << 20).contains(&self.chunk_size) {
            return bad(format!("chunk_size {} is not a power of two from 4 KiB to 64 MiB", self.chunk_size));
        }
        let mut seen = BTreeSet::new();
        for path in self.disks.iter().map(|d| &d.path).chain(self.files.iter().map(|f| &f.path)) {
            if !safe_path(path) {
                return Err(Error::Path(path.clone()));
            }
            if !seen.insert(path.as_str()) {
                return bad(format!("`{path}` appears twice"));
            }
        }
        for d in &self.disks {
            if !is_hash(&d.blake3) {
                return bad(format!("{}: bad blake3", d.path));
            }
            let mut at = 0;
            for e in &d.extents {
                let kinds = [e.hash.is_some(), e.zero, e.from_base].iter().filter(|k| **k).count();
                if kinds != 1 {
                    return bad(format!("{}: the extent at {} must be exactly one of hash, zero, from_base", d.path, e.offset));
                }
                if e.offset != at || e.len == 0 {
                    return bad(format!("{}: extent at {} (len {}) leaves a gap or overlaps at {at}", d.path, e.offset, e.len));
                }
                if let Some(h) = &e.hash
                    && (!is_hash(h) || e.len > self.chunk_size)
                {
                    return bad(format!("{}: the chunk at {} is malformed or longer than chunk_size", d.path, e.offset));
                }
                if e.from_base && d.base.is_none() {
                    return bad(format!("{}: from_base without a base", d.path));
                }
                at = e.offset.checked_add(e.len).ok_or_else(|| Error::Manifest(format!("{}: extents overflow", d.path)))?;
            }
            if at != d.size {
                return bad(format!("{}: extents cover {at} of {} bytes", d.path, d.size));
            }
        }
        for f in &self.files {
            if !is_hash(&f.blake3) || !f.chunks.iter().all(|h| is_hash(h)) {
                return bad(format!("{}: bad hash", f.path));
            }
            if f.chunks.len() as u64 != f.size.div_ceil(self.chunk_size) {
                return bad(format!("{}: {} chunks for {} bytes", f.path, f.chunks.len(), f.size));
            }
        }
        self.validate_tree(&seen)
    }

    /// The tree's checks: each entry's path is safe and appears once; a file entry is a disk or
    /// file of the bundle and no other entry is; a symlink has a target and no mode, and nothing
    /// lies under one (extracting through a link would write outside the destination).
    fn validate_tree(&self, stored: &BTreeSet<&str>) -> Result<()> {
        let bad = |s: String| Err(Error::Manifest(s));
        let mut paths = BTreeSet::new();
        let mut links = BTreeSet::new();
        for e in &self.tree {
            if !safe_path(&e.path) {
                return Err(Error::Path(e.path.clone()));
            }
            if !paths.insert(e.path.as_str()) {
                return bad(format!("tree: `{}` appears twice", e.path));
            }
            let is_stored = stored.contains(e.path.as_str());
            if (e.kind == EntryKind::File) != is_stored {
                let what = if is_stored { "stored" } else { "not stored" };
                return bad(format!("tree: `{}` is a {:?} entry but {what} in the bundle", e.path, e.kind));
            }
            match (e.kind, &e.target, e.mode) {
                (EntryKind::Symlink, Some(t), None) if !t.is_empty() && !t.contains('\0') => {
                    links.insert(e.path.as_str());
                }
                (EntryKind::Symlink, ..) => return bad(format!("tree: the symlink `{}` needs a target and no mode", e.path)),
                (_, Some(_), _) => return bad(format!("tree: `{}` has a target but isn't a symlink", e.path)),
                (_, None, Some(m)) if m > 0o7777 => return bad(format!("tree: `{}` has mode {m:o}", e.path)),
                _ => {}
            }
        }
        if !self.tree.is_empty()
            && let Some(p) = stored.iter().find(|p| !paths.contains(*p))
        {
            return bad(format!("tree: `{p}` is stored but not in the tree"));
        }
        if !links.is_empty() {
            for p in paths.iter().chain(stored.iter()) {
                let mut at = *p;
                while let Some((parent, _)) = at.rsplit_once('/') {
                    if links.contains(parent) {
                        return bad(format!("tree: `{p}` lies under the symlink `{parent}`"));
                    }
                    at = parent;
                }
            }
        }
        Ok(())
    }

    /// Every stored chunk's hash, once, in the order the manifest first names it.
    pub fn chunk_hashes(&self) -> Vec<&str> {
        let mut seen = BTreeSet::new();
        let disks = self.disks.iter().flat_map(|d| d.extents.iter().filter_map(|e| e.hash.as_deref()));
        let files = self.files.iter().flat_map(|f| f.chunks.iter().map(String::as_str));
        disks.chain(files).filter(|h| seen.insert(*h)).collect()
    }
}
