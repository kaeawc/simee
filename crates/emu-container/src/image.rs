//! Container images: official ones only, each pinned by SHA-256.
//!
//! The only source is Redroid's own image on Docker Hub (`redroid/redroid`), by the digest of the
//! per-architecture manifest recorded here. Nothing is downloaded unless the caller opts in
//! ([`Fetch::Allowed`]); without it a missing image is an error that says how to opt in. Whatever
//! is on disk, from a pull or from an earlier run, is verified before it is unpacked: the manifest's
//! hash must equal the pin, and the layer's hash and size must equal the ones the pin records.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// An image's pin for one architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchPin {
    /// The host architecture (`std::env::consts::ARCH`'s spelling).
    pub arch: &'static str,
    /// The digest of this architecture's manifest.
    pub manifest: &'static str,
    /// The digest and size of the image's single layer (compressed tar).
    pub layer: &'static str,
    pub layer_size: u64,
}

/// An official image at one tag, pinned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pinned {
    pub android: u32,
    pub repository: &'static str,
    pub tag: &'static str,
    /// The tag's multi-architecture index, for people checking a pin against the registry.
    pub index: &'static str,
    /// What the image runs: Redroid's `/init` and the two arguments its entrypoint passes.
    pub entrypoint: &'static [&'static str],
    pub archs: &'static [ArchPin],
}

/// Redroid 15, 64-bit only, the build of 2025-06-27: the version with a recorded clean boot
/// (docs/research/container-backend.md). Digests as `registry-1.docker.io` served them on
/// 2026-11-28 (`curl` with an anonymous pull token; the index's own hash matches its digest).
/// Android 16 aborts in guest GPU mode on arm64, 14 locks up on some hosts, 13 doesn't finish
/// booting: none of them is pinned.
pub const REDROID_15: Pinned = Pinned {
    android: 15,
    repository: "redroid/redroid",
    tag: "15.0.0_64only-250627",
    index: "sha256:b51bde9cef80f7bd7581148192f2b2f4d41f23c6344cfe88eceeb8ddd67490ee",
    entrypoint: &["/init", "qemu=1", "androidboot.hardware=redroid"],
    archs: &[
        ArchPin {
            arch: "x86_64",
            manifest: "sha256:58e548d78c4854f032912757dc0782d7a86b9d602ffc10b2f09fb9e86731adf3",
            layer: "sha256:7719e5442aee2fded6f7a609799881db5fa95318388ec0d935ed372574e3509e",
            layer_size: 729_788_703,
        },
        ArchPin {
            arch: "aarch64",
            manifest: "sha256:dc2024a999dd0acb1112a23cddc9537ec8cb60a0dbe5a87ce9fbd75a2f5e94d2",
            layer: "sha256:698f1d07c642a52dec5d3367772fc438cbefdd253b2442da74d653dc135fefbc",
            layer_size: 693_612_895,
        },
    ],
};

/// Every image the backend accepts.
pub const PINNED: &[Pinned] = &[REDROID_15];

/// The pin for an Android version (its major number) and host architecture.
pub fn pinned(android: u32, arch: &str) -> Option<(&'static Pinned, &'static ArchPin)> {
    let image = PINNED.iter().find(|p| p.android == android)?;
    Some((image, image.archs.iter().find(|a| a.arch == arch)?))
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no pinned container image for Android {android} on {arch} (pinned: {})", PINNED.iter().map(|p| p.android.to_string()).collect::<Vec<_>>().join(", "))]
    NoPin { android: u32, arch: String },
    #[error("the container image isn't on disk at {0} and downloading is off: pass -container-pull to fetch {1} (about {2} MB)")]
    NotFetched(PathBuf, String, u64),
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("{what} is {got} but the pin says {want}: refusing to use an image that isn't the official one")]
    Digest { what: String, got: String, want: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Whether the caller allows a download. It is never on by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    Denied,
    Allowed,
}

/// `sha256:<hex>` of everything `r` yields.
pub fn sha256_reader(mut r: impl Read) -> std::io::Result<String> {
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("sha256:{}", hash.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.to_path_buf();
    move |e| Error::Io(path, e)
}

pub fn sha256_file(path: &Path) -> Result<String> {
    sha256_reader(std::fs::File::open(path).map_err(io_err(path))?).map_err(io_err(path))
}

/// Where an OCI layout keeps the blob for `digest` (`blobs/sha256/<hex>`); `None` when the digest
/// isn't `sha256:` plus 64 hex digits, so a crafted digest can't name a path outside the layout.
fn blob_path(layout: &Path, digest: &str) -> Option<PathBuf> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| layout.join("blobs/sha256").join(hex))
}

#[derive(Deserialize)]
struct Descriptor {
    digest: String,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct Manifest {
    layers: Vec<Descriptor>,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path).map_err(io_err(path))?;
    serde_json::from_slice(&bytes).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))
}

/// Checks an OCI image layout (what `skopeo copy … oci:<dir>` writes) against a pin and returns
/// the layer blob to unpack. The layout's `index.json` must list the pinned manifest; the manifest
/// blob must hash to the pin; it must have exactly the pinned layer, of the pinned size; and the
/// layer blob must hash to the pin. A mismatch anywhere is an error and nothing is unpacked.
pub fn verify_layout(layout: &Path, pin: &ArchPin) -> Result<PathBuf> {
    let index: Index = read_json(&layout.join("index.json"))?;
    if !index.manifests.iter().any(|m| m.digest == pin.manifest) {
        let found = index.manifests.iter().map(|m| m.digest.as_str()).collect::<Vec<_>>().join(", ");
        return Err(Error::Digest {
            what: "the layout's manifest".into(),
            got: if found.is_empty() { "none".into() } else { found },
            want: pin.manifest.into(),
        });
    }
    let manifest_path = blob_path(layout, pin.manifest).ok_or_else(|| Error::Invalid(format!("malformed pin {}", pin.manifest)))?;
    let got = sha256_file(&manifest_path)?;
    if got != pin.manifest {
        return Err(Error::Digest { what: format!("the manifest blob {}", manifest_path.display()), got, want: pin.manifest.into() });
    }
    let manifest: Manifest = read_json(&manifest_path)?;
    let [layer] = manifest.layers.as_slice() else {
        return Err(Error::Invalid(format!("the manifest has {} layers; the pinned image has one", manifest.layers.len())));
    };
    if layer.digest != pin.layer || layer.size != pin.layer_size {
        return Err(Error::Digest {
            what: "the manifest's layer".into(),
            got: format!("{} ({} bytes)", layer.digest, layer.size),
            want: format!("{} ({} bytes)", pin.layer, pin.layer_size),
        });
    }
    let blob = blob_path(layout, pin.layer).ok_or_else(|| Error::Invalid(format!("malformed pin {}", pin.layer)))?;
    let size = std::fs::metadata(&blob).map_err(io_err(&blob))?.len();
    if size != pin.layer_size {
        return Err(Error::Invalid(format!("{} is {size} bytes; the pin says {}", blob.display(), pin.layer_size)));
    }
    let got = sha256_file(&blob)?;
    if got != pin.layer {
        return Err(Error::Digest { what: format!("the layer blob {}", blob.display()), got, want: pin.layer.into() });
    }
    Ok(blob)
}

/// The command that downloads a pinned image into an OCI layout: `skopeo copy` of the manifest by
/// its digest, so the registry can only return the bytes the pin names. Returned, not run: the
/// caller runs it after [`Fetch::Allowed`].
pub fn pull_command(image: &Pinned, pin: &ArchPin, layout: &Path) -> Vec<String> {
    vec![
        "skopeo".into(),
        "copy".into(),
        "--quiet".into(),
        format!("docker://docker.io/{}@{}", image.repository, pin.manifest),
        format!("oci:{}:{}", layout.display(), image.tag),
    ]
}

/// The command that unpacks the verified layer into `rootfs`, keeping numeric owners and modes.
/// It runs inside the container's user namespace, where the ids map as the container will see them.
pub fn unpack_command(layer: &Path, rootfs: &Path) -> Vec<String> {
    vec![
        "tar".into(),
        "--extract".into(),
        "--gzip".into(),
        "--numeric-owner".into(),
        "--same-owner".into(),
        "--same-permissions".into(),
        "--xattrs".into(),
        "--file".into(),
        layer.display().to_string(),
        "--directory".into(),
        rootfs.display().to_string(),
    ]
}

/// Where an image's files live under `cache` (`<cache>/redroid-15-x86_64/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub layout: PathBuf,
    pub rootfs: PathBuf,
    /// Written after a successful unpack; holds the layer digest that was unpacked.
    pub unpacked: PathBuf,
}

pub fn paths(cache: &Path, image: &Pinned, pin: &ArchPin) -> Paths {
    let dir = cache.join(format!("redroid-{}-{}", image.android, pin.arch));
    Paths { layout: dir.join("oci"), rootfs: dir.join("rootfs"), unpacked: dir.join("rootfs.unpacked") }
}

/// What [`ensure`] has to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The rootfs was unpacked from the pinned layer already.
    Ready,
    /// The layout is on disk: verify it and unpack.
    Unpack,
    /// Nothing on disk and a download is allowed: pull (then verify and unpack).
    Pull,
}

/// What to do given what is on disk. A missing layout with downloads denied is the opt-in error.
pub fn next_step(paths: &Paths, pin: &ArchPin, fetch: Fetch) -> Result<Step> {
    if std::fs::read_to_string(&paths.unpacked).is_ok_and(|s| s.trim() == pin.layer) && paths.rootfs.is_dir() {
        return Ok(Step::Ready);
    }
    if paths.layout.join("index.json").is_file() {
        return Ok(Step::Unpack);
    }
    match fetch {
        Fetch::Allowed => Ok(Step::Pull),
        Fetch::Denied => Err(Error::NotFetched(paths.layout.clone(), pin.manifest.to_string(), pin.layer_size / 1_000_000)),
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    use super::*;

    /// A tiny OCI layout whose pin is computed from its own bytes: `(pin, layout dir)`.
    /// The layer is not a real tar; verification never opens it.
    pub fn layout(dir: &Path, layer_bytes: &[u8]) -> ArchPin {
        let blobs = dir.join("blobs/sha256");
        std::fs::create_dir_all(&blobs).expect("mkdir");
        let put = |bytes: &[u8]| {
            let digest = sha256_reader(bytes).expect("hash");
            std::fs::write(blobs.join(digest.strip_prefix("sha256:").expect("prefix")), bytes).expect("write blob");
            digest
        };
        let layer = put(layer_bytes);
        let manifest = put(format!(r#"{{"schemaVersion":2,"layers":[{{"digest":"{layer}","size":{}}}]}}"#, layer_bytes.len()).as_bytes());
        std::fs::write(dir.join("index.json"), format!(r#"{{"schemaVersion":2,"manifests":[{{"digest":"{manifest}","size":10}}]}}"#))
            .expect("index");
        // The pin's strings must be 'static; leaking a few bytes in a test is fine.
        ArchPin {
            arch: "x86_64",
            manifest: Box::leak(manifest.into_boxed_str()),
            layer: Box::leak(layer.into_boxed_str()),
            layer_size: layer_bytes.len() as u64,
        }
    }

    /// The real layout skopeo wrote for hello-world (tests/fixtures/README.md), and the pin for it.
    pub const HELLO_LAYOUT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hello-world-oci");
    pub const HELLO: ArchPin = ArchPin {
        arch: "x86_64",
        manifest: "sha256:d1a8d0a4eeb63aff09f5f34d4d80505e0ba81905f36158cc3970d8e07179e59e",
        layer: "sha256:4f55086f7dd096d48b0e49be066971a8ed996521c2e190aa21b2435a847198b4",
        layer_size: 2415,
    };

    /// A [`Pinned`] around [`HELLO`], for running the whole pipeline on a real, tiny layout.
    pub fn hello_image() -> Pinned {
        Pinned {
            android: 0,
            repository: "library/hello-world",
            tag: "15.0.0_64only-250627",
            index: "",
            entrypoint: &["/init"],
            archs: &[HELLO],
        }
    }

    pub fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("mkdir");
        for e in std::fs::read_dir(from).expect("read_dir") {
            let e = e.expect("entry");
            let dest = to.join(e.file_name());
            if e.file_type().expect("type").is_dir() {
                copy_dir(&e.path(), &dest);
            } else {
                std::fs::copy(e.path(), dest).expect("copy");
            }
        }
    }

    pub fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("emu-container-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{HELLO, HELLO_LAYOUT, layout, tmp};
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(sha256_reader(&b""[..]).expect("hash"), "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_reader(&b"abc"[..]).expect("hash"), "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn the_pins_are_well_formed_and_cover_both_host_architectures() {
        for image in PINNED {
            assert!(blob_path(Path::new("/l"), image.index).is_some(), "{}", image.index);
            assert_eq!(image.entrypoint.first(), Some(&"/init"));
            for a in image.archs {
                assert!(blob_path(Path::new("/l"), a.manifest).is_some(), "{}", a.manifest);
                assert!(blob_path(Path::new("/l"), a.layer).is_some(), "{}", a.layer);
                assert!(a.layer_size > 100_000_000, "a Redroid layer is hundreds of MB");
            }
            assert_ne!(image.archs[0].manifest, image.archs[1].manifest);
        }
        assert_eq!(pinned(15, "x86_64").map(|(i, a)| (i.tag, a.arch)), Some(("15.0.0_64only-250627", "x86_64")));
        assert_eq!(pinned(15, "aarch64").map(|(_, a)| a.arch), Some("aarch64"));
        assert!(pinned(15, "riscv64").is_none());
        assert!(pinned(16, "x86_64").is_none(), "16 isn't pinned: it doesn't boot in guest GPU mode on arm64");
    }

    #[test]
    fn blob_paths_refuse_anything_but_a_sha256_digest() {
        let l = Path::new("/layout");
        let good = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(blob_path(l, &good), Some(PathBuf::from(format!("/layout/blobs/sha256/{}", "ab".repeat(32)))));
        for bad in ["sha256:../../etc/passwd", "sha256:abc", "sha512:aa", "", &format!("sha256:{}/x", "a".repeat(62))] {
            assert_eq!(blob_path(l, bad), None, "{bad}");
        }
    }

    #[test]
    fn a_layout_written_by_skopeo_verifies_against_its_pin() {
        let blob = verify_layout(Path::new(HELLO_LAYOUT), &HELLO).expect("verifies");
        assert!(blob.ends_with("blobs/sha256/4f55086f7dd096d48b0e49be066971a8ed996521c2e190aa21b2435a847198b4"));
        // The same layout under a pin for another image is refused.
        assert!(verify_layout(Path::new(HELLO_LAYOUT), &REDROID_15.archs[0]).is_err());
        let wrong_layer = ArchPin { layer: REDROID_15.archs[0].layer, ..HELLO };
        assert!(verify_layout(Path::new(HELLO_LAYOUT), &wrong_layer).is_err());
    }

    #[test]
    fn a_layout_that_matches_its_pin_verifies() {
        let dir = tmp("verify-ok");
        let pin = layout(&dir, b"layer bytes");
        let blob = verify_layout(&dir, &pin).expect("verifies");
        assert_eq!(std::fs::read(blob).expect("blob"), b"layer bytes");
    }

    #[test]
    fn a_tampered_layer_is_refused() {
        let dir = tmp("verify-layer");
        let pin = layout(&dir, b"layer bytes");
        let blob = blob_path(&dir, pin.layer).expect("path");
        std::fs::write(&blob, b"LAYER BYTES").expect("tamper"); // same size, different bytes
        let e = verify_layout(&dir, &pin).expect_err("tampered");
        assert!(matches!(e, Error::Digest { .. }), "{e}");
        assert!(e.to_string().contains("the layer blob"), "{e}");
        std::fs::write(&blob, b"short").expect("truncate");
        assert!(verify_layout(&dir, &pin).expect_err("short").to_string().contains("bytes"));
    }

    #[test]
    fn a_tampered_manifest_is_refused() {
        let dir = tmp("verify-manifest");
        let pin = layout(&dir, b"layer bytes");
        let manifest = blob_path(&dir, pin.manifest).expect("path");
        let text = std::fs::read_to_string(&manifest).expect("manifest");
        std::fs::write(&manifest, text.replace("\"size\"", "\"size\" ")).expect("tamper");
        let e = verify_layout(&dir, &pin).expect_err("tampered manifest");
        assert!(e.to_string().contains("the manifest blob"), "{e}");
    }

    #[test]
    fn a_layout_of_a_different_image_is_refused() {
        let dir = tmp("verify-other");
        let other = layout(&dir, b"some other image");
        let mine = ArchPin { manifest: REDROID_15.archs[0].manifest, ..other };
        let e = verify_layout(&dir, &mine).expect_err("other image");
        assert!(e.to_string().contains("the pin says"), "{e}");
        assert!(e.to_string().contains(REDROID_15.archs[0].manifest), "{e}");
        // A manifest naming the right layer with a different size is refused too.
        let wrong_size = ArchPin { layer_size: other.layer_size + 1, ..other };
        assert!(verify_layout(&dir, &wrong_size).is_err());
    }

    #[test]
    fn a_manifest_with_two_layers_is_refused() {
        let dir = tmp("verify-two");
        let pin = layout(&dir, b"layer bytes");
        let blobs = dir.join("blobs/sha256");
        let two = format!(r#"{{"layers":[{{"digest":"{0}","size":11}},{{"digest":"{0}","size":11}}]}}"#, pin.layer);
        let digest = sha256_reader(two.as_bytes()).expect("hash");
        std::fs::write(blobs.join(digest.strip_prefix("sha256:").expect("prefix")), &two).expect("write");
        std::fs::write(dir.join("index.json"), format!(r#"{{"manifests":[{{"digest":"{digest}"}}]}}"#)).expect("index");
        let pin = ArchPin { manifest: Box::leak(digest.into_boxed_str()), ..pin };
        assert!(verify_layout(&dir, &pin).expect_err("two layers").to_string().contains("2 layers"));
    }

    #[test]
    fn a_missing_or_broken_layout_is_an_error_not_a_panic() {
        let dir = tmp("verify-missing");
        let pin = REDROID_15.archs[0];
        assert!(matches!(verify_layout(&dir, &pin), Err(Error::Io(..))));
        std::fs::write(dir.join("index.json"), "not json").expect("write");
        assert!(matches!(verify_layout(&dir, &pin), Err(Error::Invalid(_))));
    }

    #[test]
    fn the_pull_command_names_the_manifest_by_digest() {
        let (image, pin) = pinned(15, "aarch64").expect("pin");
        let cmd = pull_command(image, pin, Path::new("/cache/redroid-15-aarch64/oci"));
        assert_eq!(
            cmd,
            [
                "skopeo",
                "copy",
                "--quiet",
                "docker://docker.io/redroid/redroid@sha256:dc2024a999dd0acb1112a23cddc9537ec8cb60a0dbe5a87ce9fbd75a2f5e94d2",
                "oci:/cache/redroid-15-aarch64/oci:15.0.0_64only-250627",
            ]
        );
        assert!(cmd.iter().all(|a| !a.contains(":latest") && !a.ends_with("-latest")), "never a moving tag");
    }

    #[test]
    fn the_unpack_command_keeps_owners_and_modes() {
        let cmd = unpack_command(Path::new("/l/blob"), Path::new("/r"));
        for flag in ["--numeric-owner", "--same-owner", "--same-permissions", "--xattrs"] {
            assert!(cmd.iter().any(|a| a == flag), "{flag}");
        }
        assert_eq!(cmd[cmd.len() - 2..], ["--directory", "/r"]);
        assert_eq!(cmd[cmd.len() - 4..cmd.len() - 2], ["--file", "/l/blob"]);
    }

    #[test]
    fn nothing_downloads_without_the_opt_in() {
        let dir = tmp("steps");
        let (image, pin) = pinned(15, "x86_64").expect("pin");
        let p = paths(&dir, image, pin);
        assert_eq!(p.layout, dir.join("redroid-15-x86_64/oci"));
        let e = next_step(&p, pin, Fetch::Denied).expect_err("denied");
        assert!(e.to_string().contains("-container-pull") && e.to_string().contains("729 MB"), "{e}");
        assert_eq!(next_step(&p, pin, Fetch::Allowed).expect("allowed"), Step::Pull);
    }

    #[test]
    fn a_layout_on_disk_is_unpacked_without_a_download_and_an_unpacked_rootfs_is_ready() {
        let dir = tmp("steps-disk");
        let (image, pin) = pinned(15, "x86_64").expect("pin");
        let p = paths(&dir, image, pin);
        std::fs::create_dir_all(&p.layout).expect("mkdir");
        std::fs::write(p.layout.join("index.json"), "{}").expect("index");
        assert_eq!(next_step(&p, pin, Fetch::Denied).expect("on disk"), Step::Unpack);
        std::fs::create_dir_all(&p.rootfs).expect("mkdir");
        std::fs::write(&p.unpacked, "sha256:something-else\n").expect("marker");
        assert_eq!(next_step(&p, pin, Fetch::Denied).expect("stale marker"), Step::Unpack, "a marker for another layer doesn't count");
        std::fs::write(&p.unpacked, format!("{}\n", pin.layer)).expect("marker");
        assert_eq!(next_step(&p, pin, Fetch::Denied).expect("ready"), Step::Ready);
    }
}
