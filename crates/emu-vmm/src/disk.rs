//! Disk images backing virtio-blk devices.
//!
//! - `Persistent`: a raw file the guest reads and writes in place. Sparse on APFS, so an empty
//!   10 GiB userdata costs nothing until written, and guest discards punch holes back into it.
//! - `Ephemeral`: a read-only base (a system image in the SDK) with guest writes kept in
//!   memory for the life of the process — the stock emulator's behaviour for system and vendor
//!   images unless `-writable-system` is given.
use sim_core::hostfs::FileExt;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use emu_hvf::GuestRam;

const CHUNK: u64 = 4096;
/// Largest buffer of zeroes written at once where a range can't be punched.
const ZERO_BUF: u64 = 1 << 20;

/// How a disk image is attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskMode {
    /// Guest writes go to the file.
    Persistent,
    /// Guest writes are kept in memory and discarded at exit.
    Ephemeral,
}

/// A disk in a virtio-blk slot of the board.
#[derive(Clone, Debug)]
pub struct DiskSpec {
    pub slot: u32,
    pub path: PathBuf,
    pub mode: DiskMode,
    pub serial: String,
}

pub struct Disk {
    /// fsync on guest FLUSH (see `set_fsync`).
    fsync: bool,
    file: File,
    size: u64,
    /// Granularity `F_PUNCHHOLE` accepts: the host filesystem's block size.
    block: u64,
    /// Cleared once the host filesystem reports it can't punch holes.
    punch: bool,
    overlay: Option<Overlay>,
}

/// An ephemeral disk's in-memory changes on top of its read-only base.
#[derive(Default)]
struct Overlay {
    /// Chunks the guest wrote, keyed by chunk-aligned offset. They take precedence over `zeroed`.
    chunks: BTreeMap<u64, Box<[u8]>>,
    /// Chunk-aligned ranges the guest discarded or zeroed, `start -> end`, disjoint and
    /// non-adjacent: a whole-disk trim costs one entry, not one per chunk.
    zeroed: BTreeMap<u64, u64>,
}

impl Overlay {
    /// End of the zeroed range containing `off`, if any.
    fn zeroed_at(&self, off: u64) -> Option<u64> {
        self.zeroed.range(..=off).next_back().map(|(_, &end)| end).filter(|&end| end > off)
    }

    /// Start of the first zeroed range at or after `off`.
    fn next_zeroed(&self, off: u64) -> Option<u64> {
        self.zeroed.range(off..).next().map(|(&start, _)| start)
    }

    fn touches(&self, offset: u64, len: u64) -> bool {
        self.chunks.range(offset / CHUNK * CHUNK..offset + len).next().is_some()
            || self.zeroed_at(offset).is_some()
            || self.zeroed.range(offset..offset + len).next().is_some()
    }

    /// Marks chunk-aligned `[start, end)` as zeroes, dropping any chunks written there.
    fn add_zeroed(&mut self, start: u64, end: u64) {
        let mut tail = self.chunks.split_off(&start);
        let mut rest = tail.split_off(&end);
        self.chunks.append(&mut rest);
        let (mut s, mut e) = (start, end);
        if let Some((&ps, &pe)) = self.zeroed.range(..s).next_back()
            && pe >= s
        {
            s = ps;
            e = e.max(pe);
        }
        let merged: Vec<u64> = self.zeroed.range(s..=e).map(|(&k, _)| k).collect();
        for k in merged {
            if let Some(ke) = self.zeroed.remove(&k) {
                e = e.max(ke);
            }
        }
        self.zeroed.insert(s, e);
    }

    fn is_empty(&self) -> bool {
        self.chunks.is_empty() && self.zeroed.is_empty()
    }

    /// The writable copy of the chunk at `chunk`, created from the base (or zeroes) on first
    /// use. `overwrite`: the caller replaces all of it, so a new chunk skips reading the base.
    fn chunk_mut(&mut self, base: &File, chunk: u64, overwrite: bool) -> io::Result<&mut [u8]> {
        let from_zeroes = overwrite || self.zeroed_at(chunk).is_some();
        Ok(match self.chunks.entry(chunk) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let mut data = vec![0u8; CHUNK as usize].into_boxed_slice();
                if !from_zeroes {
                    read_at(base, &mut data, chunk)?;
                }
                e.insert(data)
            }
        })
    }
}

fn mem_err(e: emu_hvf::MemError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, e)
}

impl Disk {
    pub fn persistent(path: &Path) -> io::Result<Disk> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len();
        let block = fs_block_size(&file).filter(|b| b.is_power_of_two() && *b >= 512).unwrap_or(CHUNK);
        Ok(Disk { file, size, block, punch: true, overlay: None, fsync: false })
    }

    pub fn ephemeral(path: &Path) -> io::Result<Disk> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Disk { file, size, block: CHUNK, punch: false, overlay: Some(Overlay::default()), fsync: false })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    fn check(&self, offset: u64, len: u64) -> io::Result<()> {
        match offset.checked_add(len) {
            Some(end) if end <= self.size => Ok(()),
            _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "access beyond end of disk")),
        }
    }

    /// Reads `len` bytes at disk `offset` into guest memory at `gpa`.
    pub fn read_to_guest(&self, mem: &GuestRam, offset: u64, gpa: u64, len: u64) -> io::Result<()> {
        self.read_vectored_to_guest(mem, offset, &[(gpa, len as usize)])
    }

    /// Reads the disk from `offset` into the guest ranges `segs` (`(gpa, len)`, back to back:
    /// a virtio-blk request's data buffers). Where no ephemeral change covers the request that
    /// is one `preadv` straight into guest memory, whatever the number of segments.
    pub fn read_vectored_to_guest(&self, mem: &GuestRam, offset: u64, segs: &[(u64, usize)]) -> io::Result<()> {
        let total: u64 = segs.iter().map(|&(_, len)| len as u64).sum();
        self.check(offset, total)?;
        match &self.overlay {
            Some(ov) if !ov.is_empty() && ov.touches(offset, total) => {
                let mut off = offset;
                for &(gpa, len) in segs {
                    self.read_overlaid(ov, mem, off, gpa, len as u64)?;
                    off += len as u64;
                }
                Ok(())
            }
            _ => mem.read_vectored_from_file(segs, &self.file, offset),
        }
    }

    /// One range of an ephemeral disk whose overlay covers part of it: written chunks from
    /// memory, zeroed ranges as zeroes, the rest from the base.
    fn read_overlaid(&self, ov: &Overlay, mem: &GuestRam, offset: u64, gpa: u64, len: u64) -> io::Result<()> {
        let end = offset + len;
        let mut off = offset;
        while off < end {
            let chunk = off / CHUNK * CHUNK;
            let dst = gpa + (off - offset);
            if let Some(data) = ov.chunks.get(&chunk) {
                let n = (chunk + CHUNK).min(end) - off;
                let i = (off - chunk) as usize;
                mem.write(dst, &data[i..i + n as usize]).map_err(mem_err)?;
                off += n;
                continue;
            }
            // A run of base or zeroed data, up to the next written chunk.
            let stop = ov.chunks.range(chunk + CHUNK..).next().map_or(end, |(&k, _)| k.min(end));
            let n = if let Some(zend) = ov.zeroed_at(off) {
                let n = zend.min(stop) - off;
                mem.zero(dst, n as usize).map_err(mem_err)?;
                n
            } else {
                let n = ov.next_zeroed(off).map_or(stop, |s| s.min(stop)) - off;
                mem.read_from_file(dst, n as usize, &self.file, off)?;
                n
            };
            off += n;
        }
        Ok(())
    }

    /// Writes `len` bytes from guest memory at `gpa` to disk `offset`.
    pub fn write_from_guest(&mut self, mem: &GuestRam, offset: u64, gpa: u64, len: u64) -> io::Result<()> {
        self.write_vectored_from_guest(mem, offset, &[(gpa, len as usize)])
    }

    /// Writes the guest ranges `segs` (`(gpa, len)`, back to back) to the disk from `offset`:
    /// one `pwritev` for a persistent image; an ephemeral disk copies them into its chunks,
    /// reading a chunk's base only when the write leaves part of it as it was.
    pub fn write_vectored_from_guest(&mut self, mem: &GuestRam, offset: u64, segs: &[(u64, usize)]) -> io::Result<()> {
        let total: u64 = segs.iter().map(|&(_, len)| len as u64).sum();
        self.check(offset, total)?;
        let Some(overlay) = self.overlay.as_mut() else {
            return mem.write_vectored_to_file(segs, &self.file, offset);
        };
        // Check every guest range first: a chunk created for a whole-chunk write starts as
        // zeroes, so a copy that failed halfway would leave zeroes where the base was.
        for &(gpa, len) in segs {
            mem.host_range(gpa, len).ok_or_else(|| mem_err(emu_hvf::MemError { addr: gpa, len: len as u64 }))?;
        }
        let mut off = offset;
        for &(gpa, len) in segs {
            let mut pos = 0u64;
            while pos < len as u64 {
                let at = off + pos;
                let chunk = at / CHUNK * CHUNK;
                let in_chunk = (at - chunk) as usize;
                let n = (CHUNK - in_chunk as u64).min(len as u64 - pos) as usize;
                let data = overlay.chunk_mut(&self.file, chunk, n == CHUNK as usize)?;
                mem.read(gpa + pos, &mut data[in_chunk..in_chunk + n]).map_err(mem_err)?;
                pos += n as u64;
            }
            off += len as u64;
        }
        Ok(())
    }

    /// Makes `len` bytes at `offset` read back as zeroes and releases their storage where
    /// possible (virtio-blk DISCARD and WRITE_ZEROES). A persistent image gets a hole punched
    /// over the block-aligned interior and explicit zeroes at unaligned edges; an ephemeral disk
    /// records the range in memory and never touches its base.
    pub fn zero_range(&mut self, offset: u64, len: u64) -> io::Result<()> {
        self.check(offset, len)?;
        let end = offset + len;
        if let Some(ov) = self.overlay.as_mut() {
            let first = offset.next_multiple_of(CHUNK);
            let last = end / CHUNK * CHUNK;
            if first < last {
                zero_overlay_bytes(ov, &self.file, offset, first)?;
                zero_overlay_bytes(ov, &self.file, last, end)?;
                ov.add_zeroed(first, last);
            } else {
                zero_overlay_bytes(ov, &self.file, offset, end)?;
            }
            return Ok(());
        }
        let first = offset.next_multiple_of(self.block);
        let last = end / self.block * self.block;
        if !self.punch || first >= last {
            return write_zeroes(&self.file, offset, end);
        }
        write_zeroes(&self.file, offset, first)?;
        write_zeroes(&self.file, last, end)?;
        match emu_hvf::host::punch_hole(&self.file, first, last - first) {
            Ok(()) => Ok(()),
            Err(e) => {
                if no_holes(&e) {
                    self.punch = false;
                }
                write_zeroes(&self.file, first, last)
            }
        }
    }

    /// Whether guest flushes reach the drive (`-disk-cache fsync`). Off by default: writes are
    /// already in the host page cache, which survives an emulator crash; only a host crash or
    /// power loss can lose them.
    pub fn set_fsync(&mut self, on: bool) {
        self.fsync = on;
    }

    pub fn flush(&self) -> io::Result<()> {
        if self.overlay.is_some() || !self.fsync {
            return Ok(());
        }
        // Plain fsync hands dirty pages to the drive. std's sync_data/sync_all use F_FULLFSYNC on
        // macOS, which also drains the drive's cache: it made guest FLUSHes (ext4 commits,
        // fsync-heavy first boot) the dominant cost of booting. Data written before a flush has
        // already reached the host page cache, so an emulator crash loses nothing either way.
        #[cfg(unix)]
        return rustix::fs::fsync(&self.file).map_err(io::Error::from);
        // Windows' FlushFileBuffers is what sync_data calls; there is no F_FULLFSYNC to avoid.
        #[cfg(windows)]
        return self.file.sync_data();
    }
}

/// The file system's block size, which holes are punched in multiples of.
#[cfg(unix)]
fn fs_block_size(file: &File) -> Option<u64> {
    rustix::fs::fstatvfs(file).ok().map(|s| s.f_frsize)
}

/// Windows has no `fstatvfs` this crate reads: the default chunk stands in.
#[cfg(windows)]
fn fs_block_size(_file: &File) -> Option<u64> {
    None
}

/// Whether `e` says the file system can't punch holes.
fn no_holes(e: &io::Error) -> bool {
    #[cfg(unix)]
    return [Some(libc::ENOTSUP), Some(libc::EOPNOTSUPP)].contains(&e.raw_os_error());
    #[cfg(windows)]
    return e.kind() == io::ErrorKind::Unsupported;
}

/// Zeroes `[start, end)` of an ephemeral disk chunk by chunk (the partial chunks of a range).
fn zero_overlay_bytes(ov: &mut Overlay, base: &File, start: u64, end: u64) -> io::Result<()> {
    let mut off = start;
    while off < end {
        let chunk = off / CHUNK * CHUNK;
        let n = (chunk + CHUNK).min(end) - off;
        if ov.chunks.contains_key(&chunk) || ov.zeroed_at(chunk).is_none() {
            let i = (off - chunk) as usize;
            ov.chunk_mut(base, chunk, false)?[i..i + n as usize].fill(0);
        }
        off += n;
    }
    Ok(())
}

/// Writes zeroes over `[start, end)` of `file` in bounded pieces.
fn write_zeroes(file: &File, start: u64, end: u64) -> io::Result<()> {
    if start >= end {
        return Ok(());
    }
    let buf = vec![0u8; (end - start).min(ZERO_BUF) as usize];
    let mut off = start;
    while off < end {
        let n = (end - off).min(ZERO_BUF) as usize;
        file.write_all_at(&buf[..n], off)?;
        off += n as u64;
    }
    Ok(())
}

fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => break, // past EOF reads as zeroes
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Creates a sparse raw image of `size` bytes if `path` doesn't exist.
pub fn ensure_sparse(path: &Path, size: u64) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let f = OpenOptions::new().write(true).create_new(true).open(path)?;
    f.set_len(size)
}

/// Copies `src` to `dst` if `dst` doesn't exist, as an APFS clone when possible (instant, no
/// extra space until either side is written).
pub fn ensure_copy(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.exists() {
        return Ok(());
    }
    std::fs::copy(src, dst).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    const RAM: u64 = 0x4000_0000;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("emu-disk-{}-{name}", std::process::id()))
    }

    fn read_all(d: &Disk, mem: &GuestRam, len: u64) -> Vec<u8> {
        d.read_to_guest(mem, 0, RAM + 0x10000, len).expect("read");
        let mut got = vec![0u8; len as usize];
        mem.read(RAM + 0x10000, &mut got).expect("r");
        got
    }

    #[test]
    fn ephemeral_keeps_writes_in_memory() {
        let path = tmp("eph");
        std::fs::write(&path, vec![0xaa; 3 * CHUNK as usize]).expect("write");
        let mem = GuestRam::new(RAM, 0x10_0000).expect("ram");
        let mut d = Disk::ephemeral(&path).expect("open");
        mem.write(RAM, &[1, 2, 3, 4]).expect("w");
        // Straddles chunks 0 and 1.
        d.write_from_guest(&mem, CHUNK - 2, RAM, 4).expect("write");
        let got = read_all(&d, &mem, 3 * CHUNK);
        assert_eq!(&got[CHUNK as usize - 3..CHUNK as usize + 3], &[0xaa, 1, 2, 3, 4, 0xaa]);
        assert!(got[2 * CHUNK as usize..].iter().all(|&b| b == 0xaa));
        assert_eq!(std::fs::read(&path).expect("read")[CHUNK as usize - 2], 0xaa, "base untouched");
        assert!(d.read_to_guest(&mem, 3 * CHUNK - 1, RAM, 2).is_err(), "past the end");
        let _ = std::fs::remove_file(path);
    }

    /// Scattered guest pages, as a virtio-blk request carries them, read and written as one
    /// request on both kinds of disk, with an ephemeral overlay covering part of the range.
    #[test]
    fn vectored_requests_match_per_segment_io() {
        const LEN: u64 = 16 * CHUNK;
        let base: Vec<u8> = (0..LEN).map(|i| (i * 13 % 251) as u8).collect();
        let mem = GuestRam::new(RAM, 0x20_0000).expect("ram");
        // Unaligned sizes so segments straddle chunks; every other page so none merge but two do.
        let segs = [(RAM + 0x1_0000, 4096), (RAM + 0x1_2000, 100), (RAM + 0x1_2000 + 100, 3996), (RAM + 0x1_6000, 9000)];
        let total: usize = segs.iter().map(|s| s.1).sum();
        for kind in ["persistent", "ephemeral"] {
            let path = tmp(&format!("vec-{kind}"));
            std::fs::write(&path, &base).expect("write");
            let mut d = if kind == "persistent" { Disk::persistent(&path) } else { Disk::ephemeral(&path) }.expect("open");
            let mut model = base.clone();
            // A small single write first, so the ephemeral overlay touches the vectored range.
            mem.write(RAM, &[0xc3; 10]).expect("w");
            d.write_from_guest(&mem, 3 * CHUNK + 5, RAM, 10).expect("small write");
            model[3 * CHUNK as usize + 5..3 * CHUNK as usize + 15].fill(0xc3);
            // Vectored write from offset CHUNK + 300: a full chunk and partial ones.
            let src: Vec<u8> = (0..total).map(|i| (i % 7 + 1) as u8).collect();
            let mut at = 0;
            for &(gpa, len) in &segs {
                mem.write(gpa, &src[at..at + len]).expect("w");
                at += len;
            }
            d.write_vectored_from_guest(&mem, CHUNK + 300, &segs).expect("writev");
            model[CHUNK as usize + 300..CHUNK as usize + 300 + total].copy_from_slice(&src);
            // Read back vectored from a different offset into the same pages.
            for off in [0, CHUNK + 300, 2 * CHUNK + 1] {
                mem.zero(RAM + 0x1_0000, 0x1_0000).expect("zero");
                d.read_vectored_to_guest(&mem, off, &segs).expect("readv");
                let mut at = off as usize;
                for &(gpa, len) in &segs {
                    let mut got = vec![0u8; len];
                    mem.read(gpa, &mut got).expect("r");
                    assert_eq!(got, model[at..at + len], "{kind} read at {off}, segment {gpa:#x}");
                    at += len;
                }
            }
            assert!(d.read_vectored_to_guest(&mem, LEN - 4096, &segs).is_err(), "{kind}: past the end");
            let _ = std::fs::remove_file(path);
        }
    }

    /// A 128 KiB request in 32 scattered 4 KiB pages: one call per page (what virtio-blk did
    /// before vectored requests) against one vectored call, in alternating rounds in one process
    /// so host noise hits both alike. Prints the median round of each.
    ///     cargo test --release -p simee-emu-vmm disk_vectored_vs_per_segment -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn disk_vectored_vs_per_segment() {
        use std::time::Instant;
        const FILE: u64 = 64 << 20;
        const REQ: u64 = 32 * CHUNK;
        const PER_ROUND: u64 = 2000;
        let path = tmp("vec-bench");
        std::fs::write(&path, (0..FILE).map(|i| (i >> 9) as u8).collect::<Vec<u8>>()).expect("image");
        let mem = GuestRam::new(RAM, 32 << 20).expect("ram");
        let segs: Vec<(u64, usize)> = (0..32).map(|i| (RAM + 0x10_0000 + i * 2 * CHUNK, CHUNK as usize)).collect();
        let median = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        for kind in ["persistent", "ephemeral"] {
            for write in [false, true] {
                let (mut per, mut vec) = (Vec::new(), Vec::new());
                for round in 0..14 {
                    // Ephemeral writes start from an empty overlay each round, so every chunk is new.
                    let mut d = if kind == "persistent" { Disk::persistent(&path) } else { Disk::ephemeral(&path) }.expect("open");
                    let vectored = round % 2 == 1;
                    let start = Instant::now();
                    for n in 0..PER_ROUND {
                        let off = n * 7919 % (FILE / REQ) * REQ;
                        match (write, vectored) {
                            (false, false) => {
                                for (i, &(gpa, len)) in segs.iter().enumerate() {
                                    d.read_to_guest(&mem, off + i as u64 * CHUNK, gpa, len as u64).expect("read");
                                }
                            }
                            (false, true) => d.read_vectored_to_guest(&mem, off, &segs).expect("readv"),
                            (true, false) => {
                                for (i, &(gpa, len)) in segs.iter().enumerate() {
                                    d.write_from_guest(&mem, off + i as u64 * CHUNK, gpa, len as u64).expect("write");
                                }
                            }
                            (true, true) => d.write_vectored_from_guest(&mem, off, &segs).expect("writev"),
                        }
                    }
                    let us = start.elapsed().as_secs_f64() * 1e6 / PER_ROUND as f64;
                    if round >= 2 {
                        if vectored { vec.push(us) } else { per.push(us) }
                    }
                }
                let (p, v) = (median(&mut per), median(&mut vec));
                let op = if write { "write" } else { "read" };
                println!("disk {kind:<10} {op:<5} 32x4KiB: per-segment {p:>6.1} us/req, vectored {v:>6.1} us/req ({:.2}x)", p / v);
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ephemeral_whole_chunk_writes_keep_the_base_where_a_copy_fails() {
        let path = tmp("eph-full");
        std::fs::write(&path, vec![0xaa; 4 * CHUNK as usize]).expect("write");
        let mem = GuestRam::new(RAM, 0x10_0000).expect("ram");
        let mut d = Disk::ephemeral(&path).expect("open");
        mem.write(RAM, &[5; CHUNK as usize]).expect("w");
        // The second range lies outside guest RAM: nothing is applied, not even the first chunk.
        assert!(d.write_vectored_from_guest(&mem, 0, &[(RAM, CHUNK as usize), (RAM + 0x10_0000 - 8, 16)]).is_err());
        assert!(d.overlay.as_ref().expect("overlay").is_empty());
        // A whole-chunk write next to partial ones.
        d.write_vectored_from_guest(&mem, CHUNK - 8, &[(RAM, 8), (RAM, CHUNK as usize), (RAM, 8)]).expect("write");
        let got = read_all(&d, &mem, 4 * CHUNK);
        assert!(got[..CHUNK as usize - 8].iter().all(|&b| b == 0xaa));
        assert!(got[CHUNK as usize - 8..2 * CHUNK as usize + 8].iter().all(|&b| b == 5));
        assert!(got[2 * CHUNK as usize + 8..].iter().all(|&b| b == 0xaa));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persistent_writes_through() {
        let path = tmp("pers");
        ensure_sparse(&path, 2 * CHUNK).expect("create");
        let mem = GuestRam::new(RAM, 0x10_0000).expect("ram");
        let mut d = Disk::persistent(&path).expect("open");
        mem.write(RAM, b"data").expect("w");
        d.write_from_guest(&mem, 512, RAM, 4).expect("write");
        d.flush().expect("flush");
        assert_eq!(&std::fs::read(&path).expect("read")[512..516], b"data");
        let _ = std::fs::remove_file(path);
    }

    // Counts `st_blocks`, and punching holes isn't implemented on Windows.
    #[cfg(unix)]
    #[test]
    fn persistent_zero_range_punches_holes() {
        const LEN: u64 = 4 << 20;
        let path = tmp("punch");
        std::fs::write(&path, vec![0x55u8; LEN as usize]).expect("write");
        File::open(&path).and_then(|f| f.sync_all()).expect("sync");
        let before = std::fs::metadata(&path).expect("meta").blocks();
        let mut d = Disk::persistent(&path).expect("open");
        // Unaligned at both ends: 1000 bytes in, ending 3000 bytes past the 3 MiB mark.
        let (start, end) = (1000, (3 << 20) + 3000);
        d.zero_range(start, end - start).expect("zero");
        d.flush().expect("flush");
        let data = std::fs::read(&path).expect("read");
        assert!(data[..start as usize].iter().all(|&b| b == 0x55), "head preserved");
        assert!(data[start as usize..end as usize].iter().all(|&b| b == 0), "range zeroed");
        assert!(data[end as usize..].iter().all(|&b| b == 0x55), "tail preserved");
        assert_eq!(data.len() as u64, LEN, "length unchanged");
        let after = std::fs::metadata(&path).expect("meta").blocks();
        // st_blocks counts 512-byte units; all but the two partial edge blocks are released.
        assert!(before - after >= ((3 << 20) - 2 * 4096) / 512, "blocks {before} -> {after}");
        assert!(d.zero_range(LEN - 10, 11).is_err(), "past the end");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persistent_zero_range_within_one_block() {
        let path = tmp("punch-small");
        std::fs::write(&path, vec![0x55u8; 2 * CHUNK as usize]).expect("write");
        let mut d = Disk::persistent(&path).expect("open");
        d.zero_range(CHUNK - 10, 20).expect("zero");
        let data = std::fs::read(&path).expect("read");
        let z = (CHUNK - 10) as usize;
        assert_eq!(data[z - 1], 0x55);
        assert!(data[z..z + 20].iter().all(|&b| b == 0));
        assert_eq!(data[z + 20], 0x55);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ephemeral_zero_range_shadows_base() {
        let path = tmp("eph-zero");
        std::fs::write(&path, vec![0xaa; 4 * CHUNK as usize]).expect("write");
        let mem = GuestRam::new(RAM, 0x10_0000).expect("ram");
        let mut d = Disk::ephemeral(&path).expect("open");
        mem.write(RAM, &[7; 8]).expect("w");
        d.write_from_guest(&mem, CHUNK + 100, RAM, 8).expect("write");
        // Tail of chunk 0, all of chunks 1-2, head of chunk 3.
        let (start, end) = (CHUNK - 10, 3 * CHUNK + 20);
        d.zero_range(start, end - start).expect("zero");
        let got = read_all(&d, &mem, 4 * CHUNK);
        assert!(got[..start as usize].iter().all(|&b| b == 0xaa));
        assert!(got[start as usize..end as usize].iter().all(|&b| b == 0));
        assert!(got[end as usize..].iter().all(|&b| b == 0xaa));
        let ov = d.overlay.as_ref().expect("overlay");
        assert_eq!(ov.chunks.keys().copied().collect::<Vec<_>>(), [0, 3 * CHUNK], "full chunks cost nothing");
        assert_eq!(ov.zeroed.iter().map(|(&s, &e)| (s, e)).collect::<Vec<_>>(), [(CHUNK, 3 * CHUNK)]);

        // A write into a zeroed chunk starts from zeroes, not the base.
        d.write_from_guest(&mem, 2 * CHUNK + 5, RAM, 1).expect("write");
        let got = read_all(&d, &mem, 4 * CHUNK);
        assert_eq!(&got[2 * CHUNK as usize + 4..2 * CHUNK as usize + 7], &[0, 7, 0]);

        // Zeroing chunk 3 merges with the existing range; the newer chunk-2 write survives.
        d.zero_range(3 * CHUNK, CHUNK).expect("zero");
        let ov = d.overlay.as_ref().expect("overlay");
        assert_eq!(ov.zeroed.iter().map(|(&s, &e)| (s, e)).collect::<Vec<_>>(), [(CHUNK, 4 * CHUNK)]);
        assert_eq!(ov.chunks.keys().copied().collect::<Vec<_>>(), [0, 2 * CHUNK]);
        let got = read_all(&d, &mem, 4 * CHUNK);
        assert_eq!(got[2 * CHUNK as usize + 5], 7);
        assert!(got[3 * CHUNK as usize..].iter().all(|&b| b == 0));
        assert!(std::fs::read(&path).expect("read").iter().all(|&b| b == 0xaa), "base untouched");
        let _ = std::fs::remove_file(path);
    }
}
