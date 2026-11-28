//! Loading the kernel image (raw or gzip-compressed arm64 `Image`) into guest RAM.
//!
//! Images go straight into guest RAM: a raw `Image` by `pread`, a gzip one inflated in chunks
//! (no whole-file or whole-image buffer). An inflated image is cached (`<cache>/kernel-*`, keyed
//! by the gzip file's path, size, mtime, inode and trailer CRC32/ISIZE, and accepted only at the
//! trailer's length), so later launches `pread` it like a raw one.
use sim_core::hostfs::{FileExt, MetadataExt};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use emu_hvf::GuestRam;

const ARM64_MAGIC: u32 = 0x644d_5241; // "ARM\x64" at offset 0x38
const HEADER: usize = 64;
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
/// Inflate output chunk: big enough to amortize calls, small enough to stay in cache.
const CHUNK: usize = 1 << 20;

/// What the arm64 boot header says about an image of `len` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub text_offset: u64,
    /// Bytes the kernel occupies from its load address (image plus BSS).
    pub image_size: u64,
}

impl Header {
    pub fn parse(image: &[u8], len: u64) -> Result<Header> {
        if image.len() < HEADER || len < HEADER as u64 {
            bail!("kernel image too small ({len} bytes)");
        }
        let word = |o: usize| u64::from_le_bytes(image[o..o + 8].try_into().unwrap_or_default());
        let magic = u32::from_le_bytes(image[0x38..0x3c].try_into().unwrap_or_default());
        if magic != ARM64_MAGIC {
            bail!("not an arm64 kernel Image (magic {magic:#x})");
        }
        // image_size 0 means "unknown" on old kernels: assume the file plus generous BSS.
        let image_size = match word(16) {
            0 => len + (16 << 20),
            n => n.max(len),
        };
        Ok(Header { text_offset: word(8), image_size })
    }
}

/// Where the image came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Raw,
    /// A gzip image inflated by an earlier launch.
    Cached,
    /// A gzip image inflated now.
    Inflated,
}

#[derive(Debug)]
pub struct Loaded {
    /// Guest address of the image (load base + text_offset): the boot entry point.
    pub entry: u64,
    /// Bytes of image written at `entry`.
    pub len: u64,
    pub header: Header,
    pub source: Source,
}

/// Loads the kernel at `path` into `mem` at `base + text_offset`; the image (with its BSS) must
/// end at or before `limit`. With `cache`, a gzip image is inflated once and reused.
pub fn load_kernel(path: &Path, mem: &GuestRam, base: u64, limit: u64, cache: Option<&Path>) -> Result<Loaded> {
    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let meta = file.metadata().with_context(|| format!("reading {}", path.display()))?;
    let mut magic = [0u8; 2];
    let gzip = file.read_exact_at(&mut magic, 0).is_ok() && magic == GZIP_MAGIC;
    if !gzip {
        return load_raw(&file, meta.len(), mem, base, limit, Source::Raw).with_context(|| format!("loading {}", path.display()));
    }
    // The gzip trailer (CRC32 and ISIZE of the inflated bytes) ties a cache entry to the
    // content, not only to the file's metadata.
    let mut trailer = [0u8; 8];
    let entry = cache
        .filter(|_| meta.len() >= 18 && file.read_exact_at(&mut trailer, meta.len() - 8).is_ok())
        .map(|dir| (dir, CacheKey::new(path, &meta, trailer)));
    if let Some((dir, key)) = &entry
        && let Ok(f) = File::open(dir.join(key.name()))
        && let Ok(m) = f.metadata()
        // A short entry (a write cut by power loss) would read as zeros past its end.
        && m.len() == key.isize
        && let Ok(loaded) = load_raw(&f, m.len(), mem, base, limit, Source::Cached)
    {
        return Ok(loaded);
    }
    let loaded = inflate(&file, mem, base, limit).with_context(|| format!("decompressing {}", path.display()))?;
    if let Some((dir, key)) = &entry
        // Only an image the trailer describes (not, e.g., the first member of a multi-member gzip).
        && loaded.len == key.isize
    {
        // Best effort: a launch that can't cache still boots.
        let _ = key.store(dir, mem, &loaded);
    }
    Ok(loaded)
}

/// Gzip trailer: CRC32 then ISIZE (the inflated length mod 2^32), little endian.
fn trailer_isize(trailer: [u8; 8]) -> u64 {
    u64::from(u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]))
}

fn place(header: &[u8], len: u64, base: u64, limit: u64) -> Result<(Header, u64)> {
    let h = Header::parse(header, len)?;
    let entry = base.checked_add(h.text_offset).context("kernel text_offset out of range")?;
    if entry.checked_add(h.image_size).is_none_or(|end| end > limit) {
        bail!("kernel image ({} bytes) overlaps the initrd area", h.image_size);
    }
    Ok((h, entry))
}

fn load_raw(file: &File, len: u64, mem: &GuestRam, base: u64, limit: u64, source: Source) -> Result<Loaded> {
    let mut head = [0u8; HEADER];
    if len >= HEADER as u64 {
        file.read_exact_at(&mut head, 0)?;
    }
    let (header, entry) = place(&head, len, base, limit)?;
    mem.read_from_file(entry, len as usize, file, 0)?;
    Ok(Loaded { entry, len, header, source })
}

/// Fills `buf` from `r` (short only at end of stream).
fn fill(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Inflates a gzip image into guest RAM chunk by chunk (the gzip CRC is checked at the end).
fn inflate(file: &File, mem: &GuestRam, base: u64, limit: u64) -> Result<Loaded> {
    let mut gz = flate2::read::GzDecoder::new(std::io::BufReader::with_capacity(256 << 10, file));
    let mut buf = vec![0u8; CHUNK];
    let first = fill(&mut gz, &mut buf)?;
    // The total length is known only at the end: bound each chunk by `limit` as it lands.
    let (_, entry) = place(&buf[..first], first as u64, base, limit)?;
    let head: [u8; HEADER] = buf[..HEADER].try_into().unwrap_or([0; HEADER]);
    let mut len = 0u64;
    let mut n = first;
    while n > 0 {
        if entry + len + n as u64 > limit {
            bail!("kernel image overlaps the initrd area");
        }
        mem.write(entry + len, &buf[..n])?;
        len += n as u64;
        n = fill(&mut gz, &mut buf)?;
    }
    let (header, entry) = place(&head, len, base, limit)?;
    Ok(Loaded { entry, len, header, source: Source::Inflated })
}

/// FNV-1a: a stable hash for cache file names (std's hasher may change between releases).
fn fnv(bytes: impl IntoIterator<Item = u8>, mut h: u64) -> u64 {
    for b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
    }
    h
}

const FNV_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// Names the inflated image of one version of one gzip file.
struct CacheKey {
    /// Which file (its path).
    file: u64,
    /// Which version of it (size, mtime, inode, device, and the gzip trailer's CRC32 and ISIZE).
    stamp: u64,
    /// The inflated length the trailer declares: a valid entry is exactly this long.
    isize: u64,
}

/// A temp file this old has no live writer (a write takes well under a second and bumps the
/// mtime as it goes): it was left by a launch killed mid-write.
const STALE_TEMP: std::time::Duration = std::time::Duration::from_secs(60);

impl CacheKey {
    fn new(path: &Path, meta: &std::fs::Metadata, trailer: [u8; 8]) -> CacheKey {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let stamp = [meta.len(), meta.mtime() as u64, meta.mtime_nsec() as u64, meta.ino(), meta.dev(), u64::from_le_bytes(trailer)];
        CacheKey {
            file: fnv(path.as_os_str().as_encoded_bytes().iter().copied(), FNV_SEED),
            stamp: fnv(stamp.iter().flat_map(|v| v.to_le_bytes()), FNV_SEED),
            isize: trailer_isize(trailer),
        }
    }

    fn prefix(&self) -> String {
        format!("kernel-{:016x}-", self.file)
    }

    fn name(&self) -> String {
        format!("{}{:016x}", self.prefix(), self.stamp)
    }

    /// Writes the image just loaded at `loaded.entry` to the cache (temp file, synced, then
    /// renamed, so readers never see a partial one, even after a power loss) and prunes the
    /// same file's other versions and abandoned temp files.
    fn store(&self, dir: &Path, mem: &GuestRam, loaded: &Loaded) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let name = self.name();
        let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
        let written = (|| {
            let f = File::create(&tmp)?;
            mem.write_to_file(loaded.entry, loaded.len as usize, &f, 0)?;
            f.sync_data()?;
            std::fs::rename(&tmp, dir.join(&name))
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        self.prune(dir)?;
        Ok(dir.join(name))
    }

    /// Removes the same file's other versions, and temp files of any version (this one's
    /// included) old enough that no live launch is writing them. A fresh temp file belongs to
    /// another launch still writing it: kept.
    fn prune(&self, dir: &Path) -> std::io::Result<()> {
        let (prefix, current) = (self.prefix(), format!("{:016x}", self.stamp));
        for e in std::fs::read_dir(dir)?.flatten() {
            let n = e.file_name();
            let Some(rest) = n.to_str().and_then(|n| n.strip_prefix(&prefix)) else { continue };
            let stale = if rest.ends_with(".tmp") {
                e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > STALE_TEMP))
            } else {
                rest != current
            };
            if stale {
                let _ = std::fs::remove_file(e.path());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::layout::{INITRD_ADDR, KERNEL_ADDR, RAM_BASE};

    /// An arm64 Image of `len` bytes (patterned, so misplaced chunks show).
    fn fake_image(len: usize, text_offset: u64, image_size: u64) -> Vec<u8> {
        let mut img: Vec<u8> = (0..len).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        img[8..16].copy_from_slice(&text_offset.to_le_bytes());
        img[16..24].copy_from_slice(&image_size.to_le_bytes());
        img[0x38..0x3c].copy_from_slice(&ARM64_MAGIC.to_le_bytes());
        img
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(data).expect("gz");
        gz.finish().expect("gz")
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("emu-boot-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("dir");
        d
    }

    fn ram() -> GuestRam {
        GuestRam::new(RAM_BASE, 256 << 20).expect("ram")
    }

    fn guest(mem: &GuestRam, addr: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        mem.read(addr, &mut out).expect("read");
        out
    }

    fn cache_entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn header_parse() {
        let h = Header::parse(&fake_image(128, 0x8_0000, 0x20_0000), 128).expect("header");
        assert_eq!(h, Header { text_offset: 0x8_0000, image_size: 0x20_0000 });
        // Unknown size: the image plus 16 MiB of BSS.
        assert_eq!(Header::parse(&fake_image(128, 0, 0), 128).expect("header").image_size, 128 + (16 << 20));
        assert!(Header::parse(&[0u8; 128], 128).is_err());
        assert!(Header::parse(&fake_image(128, 0, 0)[..32], 32).is_err());
    }

    #[test]
    fn loads_raw_straight_into_ram() {
        let d = scratch("raw");
        let img = fake_image(3 << 20 | 123, 0x1000, 0);
        std::fs::write(d.join("Image"), &img).expect("write");
        let mem = ram();
        let cache = d.join("cache");
        let k = load_kernel(&d.join("Image"), &mem, KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("load");
        assert_eq!((k.entry, k.len, k.source), (KERNEL_ADDR + 0x1000, img.len() as u64, Source::Raw));
        assert_eq!(guest(&mem, k.entry, img.len()), img);
        // Raw images are never cached.
        assert!(cache_entries(&cache).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn inflates_in_chunks_then_reuses_the_cache() {
        let d = scratch("gz");
        // Not a multiple of the chunk size: the last chunk is short.
        let img = fake_image(5 * CHUNK + 4321, 0, 0);
        let kernel = d.join("kernel-ranchu");
        std::fs::write(&kernel, gzip(&img)).expect("write");
        let cache = d.join("cache");

        let mem = ram();
        let k = load_kernel(&kernel, &mem, KERNEL_ADDR, INITRD_ADDR, None).expect("no cache");
        assert_eq!((k.len, k.source), (img.len() as u64, Source::Inflated));
        assert_eq!(guest(&mem, KERNEL_ADDR, img.len()), img);

        let mem = ram();
        let k = load_kernel(&kernel, &mem, KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("miss");
        assert_eq!(k.source, Source::Inflated);
        assert_eq!(guest(&mem, KERNEL_ADDR, img.len()), img);
        let entries = cache_entries(&cache);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(!entries[0].ends_with(".tmp"));

        let mem = ram();
        let k = load_kernel(&kernel, &mem, KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("hit");
        assert_eq!((k.len, k.source), (img.len() as u64, Source::Cached));
        assert_eq!(guest(&mem, KERNEL_ADDR, img.len()), img);
        assert_eq!(k.header, Header { text_offset: 0, image_size: img.len() as u64 + (16 << 20) });

        // A new kernel at the same path (another size): inflated again, the old entry replaced.
        let img2 = fake_image(2 * CHUNK + 7, 0, 0);
        std::fs::write(&kernel, gzip(&img2)).expect("rewrite");
        let mem = ram();
        let k = load_kernel(&kernel, &mem, KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("changed");
        assert_eq!((k.len, k.source), (img2.len() as u64, Source::Inflated));
        assert_eq!(guest(&mem, KERNEL_ADDR, img2.len()), img2);
        let after = cache_entries(&cache);
        assert_eq!(after.len(), 1, "{after:?}");
        assert_ne!(after, entries);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_bad_cache_entry_falls_back_to_inflating() {
        let d = scratch("badcache");
        let img = fake_image(CHUNK + 99, 0, 0);
        let kernel = d.join("kernel-ranchu");
        std::fs::write(&kernel, gzip(&img)).expect("write");
        let cache = d.join("cache");
        load_kernel(&kernel, &ram(), KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("miss");
        let entry = cache.join(&cache_entries(&cache)[0]);
        std::fs::write(&entry, b"garbage, not an Image").expect("corrupt");
        let mem = ram();
        let k = load_kernel(&kernel, &mem, KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("fallback");
        assert_eq!(k.source, Source::Inflated);
        assert_eq!(guest(&mem, KERNEL_ADDR, img.len()), img);
        // And the entry is rewritten.
        assert_eq!(std::fs::read(&entry).expect("entry"), img);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A cache entry cut short (a rename that outran its data on power loss) still has a valid
    /// header, but reads past its end as zeros: it's inflated again, not booted.
    #[test]
    fn a_short_cache_entry_is_not_booted() {
        let d = scratch("short");
        let img = fake_image(3 * CHUNK + 5, 0, 0);
        let kernel = d.join("kernel-ranchu");
        std::fs::write(&kernel, gzip(&img)).expect("write");
        let cache = d.join("cache");
        load_kernel(&kernel, &ram(), KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("miss");
        let entry = cache.join(&cache_entries(&cache)[0]);
        std::fs::write(&entry, &img[..CHUNK]).expect("truncate");
        let mem = ram();
        let k = load_kernel(&kernel, &mem, KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("fallback");
        assert_eq!((k.len, k.source), (img.len() as u64, Source::Inflated));
        assert_eq!(guest(&mem, KERNEL_ADDR, img.len()), img);
        assert_eq!(std::fs::read(&entry).expect("entry"), img);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A kernel rewritten in place with the same size, inode and mtime (`cp -p` or
    /// `rsync --inplace -t` over it) is a new version: its trailer differs.
    #[test]
    fn a_kernel_rewritten_in_place_is_inflated_again() {
        let d = scratch("inplace");
        let kernel = d.join("kernel-ranchu");
        let old = fake_image(2 * CHUNK + 11, 0, 0);
        let mut new = old.clone();
        new[CHUNK..CHUNK + 4].copy_from_slice(b"new!");
        // Stored (uncompressed) gzip: equal input lengths give equal file sizes.
        let stored = |data: &[u8]| {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::none());
            gz.write_all(data).expect("gz");
            gz.finish().expect("gz")
        };
        std::fs::write(&kernel, stored(&old)).expect("write");
        let cache = d.join("cache");
        load_kernel(&kernel, &ram(), KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("miss");
        let before = std::fs::metadata(&kernel).expect("meta");
        {
            let f = std::fs::OpenOptions::new().write(true).open(&kernel).expect("open in place");
            f.write_all_at(&stored(&new), 0).expect("rewrite");
            f.set_modified(before.modified().expect("mtime")).expect("keep mtime");
        }
        let after = std::fs::metadata(&kernel).expect("meta");
        assert_eq!(
            (after.len(), after.ino(), after.mtime(), after.mtime_nsec()),
            (before.len(), before.ino(), before.mtime(), before.mtime_nsec())
        );
        let mem = ram();
        let k = load_kernel(&kernel, &mem, KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("changed");
        assert_eq!(k.source, Source::Inflated);
        assert_eq!(guest(&mem, KERNEL_ADDR, new.len()), new);
        assert_eq!(cache_entries(&cache).len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Storing prunes temp files no live launch is writing (any version, this one's included),
    /// keeps fresh ones (another launch mid-write) and never touches other kernels' files.
    #[test]
    fn prune_removes_abandoned_temp_files_only() {
        let d = scratch("prune");
        let kernel = d.join("kernel-ranchu");
        std::fs::write(&kernel, gzip(&fake_image(CHUNK + 3, 0, 0))).expect("write");
        let cache = d.join("cache");
        load_kernel(&kernel, &ram(), KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("miss");
        let name = cache_entries(&cache).remove(0);
        let prefix = &name[..name.len() - 16];
        let old = std::time::SystemTime::now() - 2 * STALE_TEMP;
        let place = |file: &str, mtime: Option<std::time::SystemTime>| {
            let f = File::create(cache.join(file)).expect("create");
            if let Some(t) = mtime {
                f.set_modified(t).expect("mtime");
            }
        };
        let abandoned_current = format!("{name}.999999.tmp");
        let abandoned_other = format!("{prefix}00000000deadbeef.888888.tmp");
        let live_current = format!("{name}.777777.tmp");
        let live_other = format!("{prefix}00000000feedface.666666.tmp");
        let other_kernel = "kernel-0000000000000000-0000000000000001.555555.tmp".to_string();
        place(&abandoned_current, Some(old));
        place(&abandoned_other, Some(old));
        place(&live_current, None);
        place(&live_other, None);
        place(&other_kernel, Some(old));
        // The next miss stores, and prunes.
        std::fs::remove_file(cache.join(&name)).expect("drop entry");
        load_kernel(&kernel, &ram(), KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("miss again");
        let mut want = vec![name.clone(), live_current, live_other, other_kernel];
        want.sort();
        assert_eq!(cache_entries(&cache), want);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rejects_bad_images() {
        let d = scratch("bad");
        let (mem, limit) = (ram(), KERNEL_ADDR + (8 << 20));
        let load = |name: &str, bytes: &[u8]| {
            std::fs::write(d.join(name), bytes).expect("write");
            load_kernel(&d.join(name), &mem, KERNEL_ADDR, limit, None)
        };
        assert!(load("magic", &[0u8; 4096]).is_err());
        assert!(load("tiny", &fake_image(64, 0, 0)[..40]).is_err());
        // Declared size past the limit, raw and gzip.
        assert!(load("big-raw", &fake_image(4096, 0, 9 << 20)).is_err());
        assert!(load("big-gz", &gzip(&fake_image(4096, 0, 9 << 20))).is_err());
        // Undeclared size whose inflated bytes run past the limit.
        assert!(load("long-gz", &gzip(&fake_image(9 << 20, 0, 0))).is_err());
        // A truncated gzip stream fails its checks instead of booting half a kernel.
        let gz = gzip(&fake_image(CHUNK * 2, 0, 0));
        assert!(load("cut-gz", &gz[..gz.len() - 8]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A real gzip kernel for the bench: `EMU_BENCH_KERNEL`, else the last gzip
    /// `kernel-ranchu` (by path) in the SDK's arm64 system images.
    fn bench_kernel() -> Option<std::path::PathBuf> {
        if let Some(p) = std::env::var_os("EMU_BENCH_KERNEL") {
            return Some(p.into());
        }
        let sdk = std::env::var_os("ANDROID_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Android/sdk")))?;
        let gzip = |k: &std::path::PathBuf| {
            let mut m = [0u8; 2];
            std::fs::File::open(k).and_then(|mut f| f.read_exact(&mut m)).is_ok() && m == [0x1f, 0x8b]
        };
        let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(sdk.join("system-images"))
            .ok()?
            .flatten()
            .flat_map(|api| std::fs::read_dir(api.path()).into_iter().flatten().flatten())
            .map(|tag| tag.path().join("arm64-v8a/kernel-ranchu"))
            .filter(gzip)
            .collect();
        found.sort();
        found.pop()
    }

    /// Launch cost of a real gzip kernel into fresh guest RAM, interleaved: the old path (read
    /// the file, inflate into a Vec, copy), a cache miss (chunked inflate, then the cache write)
    /// and a cache hit (`pread`).
    ///     cargo test --release -p simee-emu-vmm kernel_load -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn kernel_load() {
        use std::time::Instant;
        let Some(path) = bench_kernel() else {
            println!("no gzip kernel-ranchu found (set EMU_BENCH_KERNEL)");
            return;
        };
        let cache = scratch("bench-cache");
        let mut times = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for _ in 0..7 {
            let mem = ram();
            let start = Instant::now();
            let raw = std::fs::read(&path).expect("read");
            let mut image = Vec::with_capacity(raw.len() * 3);
            flate2::read::GzDecoder::new(raw.as_slice()).read_to_end(&mut image).expect("inflate");
            mem.write(KERNEL_ADDR, &image).expect("write");
            times[0].push(start.elapsed().as_secs_f64() * 1e3);

            let mem = ram();
            let start = Instant::now();
            load_kernel(&path, &mem, KERNEL_ADDR, INITRD_ADDR, None).expect("inflate");
            times[1].push(start.elapsed().as_secs_f64() * 1e3);

            let _ = std::fs::remove_dir_all(&cache);
            let mem = ram();
            let start = Instant::now();
            let k = load_kernel(&path, &mem, KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("miss");
            times[2].push(start.elapsed().as_secs_f64() * 1e3);
            assert_eq!(k.source, Source::Inflated);

            let mem = ram();
            let start = Instant::now();
            let k = load_kernel(&path, &mem, KERNEL_ADDR, INITRD_ADDR, Some(&cache)).expect("hit");
            times[3].push(start.elapsed().as_secs_f64() * 1e3);
            assert_eq!(k.source, Source::Cached);
        }
        println!("kernel {}", path.display());
        for (name, t) in ["old (read+inflate+copy)", "chunked inflate", "cache miss (+store)", "cache hit"].iter().zip(times.iter_mut()) {
            t.sort_by(f64::total_cmp);
            println!("  {name:<24} median {:>6.1} ms (min {:.1}, max {:.1})", t[3], t[0], t[6]);
        }
        let _ = std::fs::remove_dir_all(&cache);
    }
}
