//! Native ext4 formatter.
//!
//! Writes an empty ext4 filesystem (root directory, `lost+found`, an internal jbd2 journal) the
//! way `mke2fs -t ext4 -b 4096` lays one out, so simee needs no external `mkfs.ext4`. Only
//! metadata is written: the image is truncated first and every block the formatter doesn't touch
//! (inode tables, the journal body, data) reads as zero from the sparse file, so a fresh 10 GiB
//! image takes well under a megabyte on disk.
//!
//! The feature set is deliberately conservative, accepted by e2fsprogs 1.42 (the SDK's
//! `mkfs.ext4`/`e2fsck`) through 1.47 (Android's `e2fsck`/`tune2fs`) and the guest kernel:
//!
//! - compat: `has_journal`, `ext_attr`, `dir_index`
//! - incompat: `filetype`, `extent`, `flex_bg`
//! - ro_compat: `sparse_super`, `large_file`, `huge_file`, `dir_nlink`, `extra_isize`
//!
//! No `uninit_bg`/`metadata_csum`: without group checksums every inode table is simply valid
//! (all zeroes = all free), so the kernel never starts its lazy inode-table zeroing thread,
//! which would otherwise write hundreds of megabytes of zeroes into the image. No
//! `resize_inode` (the image is never grown online) and no `64bit` (up to 16 TiB).

use hostfs::FileExt;
use std::fs::File;
use std::io::{self, Error, ErrorKind};

/// Positional writes and reads: std's unix trait there, `seek_write` and `seek_read` on Windows.
mod hostfs {
    #[cfg(unix)]
    pub use std::os::unix::fs::FileExt;

    #[cfg(windows)]
    pub trait FileExt {
        fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()>;
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()>;
    }

    #[cfg(windows)]
    impl FileExt for std::fs::File {
        fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
            use std::os::windows::fs::FileExt as _;
            while !buf.is_empty() {
                match self.seek_write(buf, offset) {
                    Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                    Ok(n) => (buf, offset) = (&buf[n..], offset + n as u64),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
            use std::os::windows::fs::FileExt as _;
            while !buf.is_empty() {
                match self.seek_read(buf, offset) {
                    Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                    Ok(n) => (buf, offset) = (&mut buf[n..], offset + n as u64),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }
}

pub const BLOCK_SIZE: u64 = 4096;
/// Smallest image accepted: 4096 blocks, enough for the minimum 1024-block journal.
pub const MIN_SIZE: u64 = 16 << 20;
/// Largest image without the `64bit` feature: 2^32 - 1 blocks.
pub const MAX_SIZE: u64 = (u32::MAX as u64) * BLOCK_SIZE;

pub const COMPAT_HAS_JOURNAL: u32 = 0x4;
pub const COMPAT_EXT_ATTR: u32 = 0x8;
pub const COMPAT_DIR_INDEX: u32 = 0x20;
pub const INCOMPAT_FILETYPE: u32 = 0x2;
pub const INCOMPAT_EXTENTS: u32 = 0x40;
pub const INCOMPAT_FLEX_BG: u32 = 0x200;
pub const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
pub const RO_COMPAT_LARGE_FILE: u32 = 0x2;
pub const RO_COMPAT_HUGE_FILE: u32 = 0x8;
pub const RO_COMPAT_DIR_NLINK: u32 = 0x20;
pub const RO_COMPAT_EXTRA_ISIZE: u32 = 0x40;

pub const FEATURE_COMPAT: u32 = COMPAT_HAS_JOURNAL | COMPAT_EXT_ATTR | COMPAT_DIR_INDEX;
pub const FEATURE_INCOMPAT: u32 = INCOMPAT_FILETYPE | INCOMPAT_EXTENTS | INCOMPAT_FLEX_BG;
pub const FEATURE_RO_COMPAT: u32 =
    RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE | RO_COMPAT_HUGE_FILE | RO_COMPAT_DIR_NLINK | RO_COMPAT_EXTRA_ISIZE;

const BS: u64 = BLOCK_SIZE;
const BLOCKS_PER_GROUP: u64 = 8 * BS;
const INODE_SIZE: u64 = 256;
const INODES_PER_BLOCK: u64 = BS / INODE_SIZE;
/// mke2fs's ext4 default inode ratio.
const BYTES_PER_INODE: u64 = 16384;
const LOG_GROUPS_PER_FLEX: u8 = 4;
const GROUPS_PER_FLEX: u64 = 1 << LOG_GROUPS_PER_FLEX;
const DESC_SIZE: u64 = 32;
const FIRST_INO: u32 = 11;
const ROOT_INO: u32 = 2;
const JOURNAL_INO: u32 = 8;
const LPF_INO: u32 = 11;
/// mke2fs grows lost+found to 16 KiB so e2fsck can reconnect files without allocating.
const LPF_BLOCKS: u64 = 4;
/// Covers every field of the kernel's `ext4_inode` up to `i_projid` (needed for project quota).
const EXTRA_ISIZE: u16 = 32;
/// Longest initialized extent.
const MAX_EXTENT_LEN: u64 = 32768;

const S_IFDIR: u16 = 0o040000;
const S_IFREG: u16 = 0o100000;
const EXT4_EXTENTS_FL: u32 = 0x80000;
const FT_DIR: u8 = 2;

const EXT4_MAGIC: u16 = 0xEF53;
const EXTENT_MAGIC: u16 = 0xF30A;
const JBD2_MAGIC: u32 = 0xC03B_3998;
const JBD2_SUPERBLOCK_V2: u32 = 4;

/// Formats `file` as an empty ext4 filesystem of `size_bytes` (rounded down to whole 4 KiB
/// blocks; the file itself is set to exactly `size_bytes`).
///
/// The file is truncated to zero length first, so any previous contents are discarded and the
/// result is sparse. `label` is the volume name (at most 16 bytes), `uuid` the filesystem UUID.
pub fn format(file: &File, size_bytes: u64, label: &str, uuid: [u8; 16]) -> io::Result<()> {
    if label.len() > 16 {
        return Err(Error::new(ErrorKind::InvalidInput, format!("ext4 label {label:?} is longer than 16 bytes")));
    }
    let mut layout = Layout::new(size_bytes)?;
    let root = layout.alloc(1)?;
    let lpf = layout.alloc(LPF_BLOCKS)?;
    let journal = layout.alloc(layout.journal_blocks)?;

    file.set_len(0)?;
    file.set_len(size_bytes)?;

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32);
    let w = |block: u64, data: &[u8]| file.write_all_at(data, block * BS);

    // Directories.
    let mut buf = vec![0u8; BS as usize];
    let mut off = dirent(&mut buf, 0, ROOT_INO, 12, b".");
    off = dirent(&mut buf, off, ROOT_INO, 12, b"..");
    dirent(&mut buf, off, LPF_INO, (BS - off as u64) as u16, b"lost+found");
    w(root[0].0, &buf)?;
    for (i, block) in blocks_of(&lpf).enumerate() {
        buf.fill(0);
        if i == 0 {
            let off = dirent(&mut buf, 0, LPF_INO, 12, b".");
            dirent(&mut buf, off, ROOT_INO, (BS - off as u64) as u16, b"..");
        } else {
            // An empty directory block: one unused entry spanning it.
            put16(&mut buf, 4, BS as u16);
        }
        w(block, &buf)?;
    }

    // Journal: a clean (s_start = 0) jbd2 superblock in its first block; the rest stays zero.
    buf.fill(0);
    let be = |buf: &mut [u8], at: usize, v: u32| buf[at..at + 4].copy_from_slice(&v.to_be_bytes());
    be(&mut buf, 0x00, JBD2_MAGIC);
    be(&mut buf, 0x04, JBD2_SUPERBLOCK_V2);
    be(&mut buf, 0x0C, BS as u32);
    be(&mut buf, 0x10, layout.journal_blocks as u32);
    be(&mut buf, 0x14, 1); // s_first
    be(&mut buf, 0x18, 1); // s_sequence
    buf[0x30..0x40].copy_from_slice(&uuid);
    be(&mut buf, 0x40, 1); // s_nr_users
    w(journal[0].0, &buf)?;

    // Inode table: the first block holds inodes 1-16 (reserved ones stay zero).
    let root_inode = inode(S_IFDIR | 0o755, 3, BS, &root, now)?;
    let lpf_inode = inode(S_IFDIR | 0o700, 2, LPF_BLOCKS * BS, &lpf, now)?;
    let journal_inode = inode(S_IFREG | 0o600, 1, layout.journal_blocks * BS, &journal, now)?;
    buf.fill(0);
    for (ino, data) in [(ROOT_INO, &root_inode), (JOURNAL_INO, &journal_inode), (LPF_INO, &lpf_inode)] {
        let at = (ino as usize - 1) * INODE_SIZE as usize;
        buf[at..at + INODE_SIZE as usize].copy_from_slice(data);
    }
    w(layout.meta[0][2], &buf)?;

    // Block bitmaps (all-zero ones are already zero on disk), counting free blocks per group.
    let mut free_blocks = Vec::with_capacity(layout.groups as usize);
    let mut cursor = 0;
    for g in 0..layout.groups {
        let (start, end) = (g * BLOCKS_PER_GROUP, ((g + 1) * BLOCKS_PER_GROUP).min(layout.blocks));
        buf.fill(0);
        while cursor < layout.used.len() && layout.used[cursor].0 + layout.used[cursor].1 <= start {
            cursor += 1;
        }
        let mut used = 0;
        for &(s, len) in layout.used[cursor..].iter().take_while(|r| r.0 < end) {
            let (a, b) = (s.max(start), (s + len).min(end));
            set_bits(&mut buf, a - start, b - start);
            used += b - a;
        }
        // Bits past the end of a short last group are set, as e2fsck expects.
        set_bits(&mut buf, end - start, BLOCKS_PER_GROUP);
        free_blocks.push(end - start - used);
        if buf.iter().any(|&b| b != 0) {
            w(layout.meta[g as usize][0], &buf)?;
        }
    }

    // Inode bitmaps: padding past inodes_per_group is set; group 0 has inodes 1-11 in use.
    for g in 0..layout.groups {
        buf.fill(0);
        set_bits(&mut buf, layout.inodes_per_group, 8 * BS);
        if g == 0 {
            set_bits(&mut buf, 0, FIRST_INO as u64);
        }
        if buf.iter().any(|&b| b != 0) {
            w(layout.meta[g as usize][1], &buf)?;
        }
    }

    // Group descriptors.
    let mut gdt = vec![0u8; (layout.gdt_blocks * BS) as usize];
    for g in 0..layout.groups as usize {
        let d = &mut gdt[g * DESC_SIZE as usize..][..DESC_SIZE as usize];
        let [bb, ib, it] = layout.meta[g];
        put32(d, 0x00, bb as u32);
        put32(d, 0x04, ib as u32);
        put32(d, 0x08, it as u32);
        put16(d, 0x0C, free_blocks[g] as u16);
        let (free_inodes, dirs) = if g == 0 { (layout.inodes_per_group - FIRST_INO as u64, 2) } else { (layout.inodes_per_group, 0) };
        put16(d, 0x0E, free_inodes as u16);
        put16(d, 0x10, dirs);
    }

    // Superblock, with backups (and GDT copies) in groups 0, 1 and powers of 3, 5 and 7.
    let mut sb = vec![0u8; 1024];
    let inodes = layout.inodes_per_group * layout.groups;
    put32(&mut sb, 0x00, inodes as u32);
    put32(&mut sb, 0x04, layout.blocks as u32);
    put32(&mut sb, 0x08, (layout.blocks / 20) as u32); // 5% reserved for root
    put32(&mut sb, 0x0C, free_blocks.iter().sum::<u64>() as u32);
    put32(&mut sb, 0x10, (inodes - FIRST_INO as u64) as u32);
    put32(&mut sb, 0x14, 0); // s_first_data_block
    put32(&mut sb, 0x18, 2); // log2(4096) - 10
    put32(&mut sb, 0x1C, 2); // s_log_cluster_size
    put32(&mut sb, 0x20, BLOCKS_PER_GROUP as u32);
    put32(&mut sb, 0x24, BLOCKS_PER_GROUP as u32);
    put32(&mut sb, 0x28, layout.inodes_per_group as u32);
    put32(&mut sb, 0x30, now); // s_wtime
    put16(&mut sb, 0x36, u16::MAX); // s_max_mnt_count = -1
    put16(&mut sb, 0x38, EXT4_MAGIC);
    put16(&mut sb, 0x3A, 1); // s_state: clean
    put16(&mut sb, 0x3C, 1); // s_errors: continue
    put32(&mut sb, 0x40, now); // s_lastcheck
    put32(&mut sb, 0x4C, 1); // s_rev_level: dynamic
    put32(&mut sb, 0x54, FIRST_INO);
    put16(&mut sb, 0x58, INODE_SIZE as u16);
    put32(&mut sb, 0x5C, FEATURE_COMPAT);
    put32(&mut sb, 0x60, FEATURE_INCOMPAT);
    put32(&mut sb, 0x64, FEATURE_RO_COMPAT);
    sb[0x68..0x78].copy_from_slice(&uuid);
    sb[0x78..0x78 + label.len()].copy_from_slice(label.as_bytes());
    put32(&mut sb, 0xE0, JOURNAL_INO);
    sb[0xEC..0xFC].copy_from_slice(&hash_seed(&uuid));
    sb[0xFC] = 1; // s_def_hash_version: half_md4
    sb[0xFD] = 1; // s_jnl_backup_type: s_jnl_blocks holds a copy of the journal inode's blocks
    put32(&mut sb, 0x100, 0x4 | 0x8); // s_default_mount_opts: user_xattr, acl
    put32(&mut sb, 0x108, now); // s_mkfs_time
    sb[0x10C..0x10C + 60].copy_from_slice(&journal_inode[0x28..0x28 + 60]);
    put32(&mut sb, 0x10C + 15 * 4, 0); // i_size_high
    put32(&mut sb, 0x10C + 16 * 4, (layout.journal_blocks * BS) as u32);
    put16(&mut sb, 0x15C, EXTRA_ISIZE); // s_min_extra_isize
    put16(&mut sb, 0x15E, EXTRA_ISIZE); // s_want_extra_isize
    put32(&mut sb, 0x160, 1); // s_flags: signed directory hash (as mke2fs on macOS arm64)
    sb[0x174] = LOG_GROUPS_PER_FLEX;
    for g in (0..layout.groups).filter(|&g| has_super(g)) {
        put16(&mut sb, 0x5A, g as u16); // s_block_group_nr
        let first = g * BLOCKS_PER_GROUP;
        file.write_all_at(&sb, first * BS + if g == 0 { 1024 } else { 0 })?;
        w(first + 1, &gdt)?;
    }
    Ok(())
}

struct Layout {
    blocks: u64,
    groups: u64,
    inodes_per_group: u64,
    gdt_blocks: u64,
    journal_blocks: u64,
    /// Per group: block bitmap, inode bitmap and inode table locations.
    meta: Vec<[u64; 3]>,
    /// Blocks in use, as sorted, disjoint `(start, len)` runs.
    used: Vec<(u64, u64)>,
}

impl Layout {
    fn new(size: u64) -> io::Result<Layout> {
        if !(MIN_SIZE..=MAX_SIZE).contains(&size) {
            return Err(Error::new(ErrorKind::InvalidInput, format!("ext4 size {size} out of range ({MIN_SIZE}..={MAX_SIZE})")));
        }
        let mut blocks = size / BS;
        let mut groups = blocks.div_ceil(BLOCKS_PER_GROUP);
        // Like mke2fs, drop a last group too small to hold its own metadata plus a little data.
        let rem = blocks % BLOCKS_PER_GROUP;
        let full_itable = BLOCKS_PER_GROUP * BS / BYTES_PER_INODE / INODES_PER_BLOCK;
        let sb_overhead = if has_super(groups - 1) { 1 + gdt_blocks(groups) } else { 0 };
        if groups > 1 && rem != 0 && rem < sb_overhead + 2 + full_itable + 50 {
            blocks -= rem;
            groups -= 1;
        }
        let inodes_per_group =
            (blocks * BS / BYTES_PER_INODE).div_ceil(groups).next_multiple_of(INODES_PER_BLOCK).clamp(INODES_PER_BLOCK, 8 * BS);
        let itable_blocks = inodes_per_group / INODES_PER_BLOCK;
        let gdt_blocks = gdt_blocks(groups);

        let mut used: Vec<(u64, u64)> = (0..groups).filter(|&g| has_super(g)).map(|g| (g * BLOCKS_PER_GROUP, 1 + gdt_blocks)).collect();
        // flex_bg: each run of 16 groups keeps its bitmaps and inode tables together at the
        // start of its first group.
        let mut meta = vec![[0; 3]; groups as usize];
        for first in (0..groups).step_by(GROUPS_PER_FLEX as usize) {
            let n = GROUPS_PER_FLEX.min(groups - first);
            let start = first * BLOCKS_PER_GROUP + if has_super(first) { 1 + gdt_blocks } else { 0 };
            let len = 2 * n + n * itable_blocks;
            if start + len > ((first + 1) * BLOCKS_PER_GROUP).min(blocks) {
                return Err(Error::other(format!("ext4 layout: group {first} can't hold its flex group's metadata")));
            }
            for i in 0..n {
                meta[(first + i) as usize] = [start + i, start + n + i, start + 2 * n + i * itable_blocks];
            }
            used.push((start, len));
        }
        used.sort_unstable();
        Ok(Layout { blocks, groups, inodes_per_group, gdt_blocks, journal_blocks: journal_blocks(blocks), meta, used })
    }

    /// Allocates `count` blocks first-fit, returning them as runs of at most one extent each.
    fn alloc(&mut self, mut count: u64) -> io::Result<Vec<(u64, u64)>> {
        let mut runs = Vec::new();
        let mut cursor = 0;
        for &(start, len) in self.used.iter().chain([(self.blocks, 0)].iter()) {
            while cursor < start && count > 0 {
                let take = (start - cursor).min(count).min(MAX_EXTENT_LEN);
                runs.push((cursor, take));
                cursor += take;
                count -= take;
            }
            if count == 0 {
                break;
            }
            cursor = cursor.max(start + len);
        }
        if count > 0 {
            return Err(Error::new(ErrorKind::StorageFull, "ext4 image too small for its metadata"));
        }
        self.used.extend_from_slice(&runs);
        self.used.sort_unstable();
        Ok(runs)
    }
}

/// Whether group `g` holds a superblock backup under `sparse_super`.
fn has_super(g: u64) -> bool {
    let power_of = |base: u64| {
        let mut n = g;
        while n > 1 && n.is_multiple_of(base) {
            n /= base;
        }
        n == 1
    };
    g <= 1 || power_of(3) || power_of(5) || power_of(7)
}

fn gdt_blocks(groups: u64) -> u64 {
    (groups * DESC_SIZE).div_ceil(BS)
}

/// mke2fs's default journal size for a filesystem of `blocks` 4 KiB blocks.
fn journal_blocks(blocks: u64) -> u64 {
    match blocks {
        0..32_768 => 1024,
        32_768..262_144 => 4096,
        262_144..524_288 => 8192,
        524_288..1_048_576 => 16_384,
        _ => 32_768,
    }
}

fn blocks_of(runs: &[(u64, u64)]) -> impl Iterator<Item = u64> + '_ {
    runs.iter().flat_map(|&(start, len)| start..start + len)
}

/// An inode whose data is `runs`, mapped in order by extents held in the inode itself.
fn inode(mode: u16, links: u16, size: u64, runs: &[(u64, u64)], now: u32) -> io::Result<[u8; INODE_SIZE as usize]> {
    if runs.len() > 4 {
        return Err(Error::other("ext4 layout: file needs more than 4 extents"));
    }
    let mut i = [0u8; INODE_SIZE as usize];
    put16(&mut i, 0x00, mode);
    put32(&mut i, 0x04, size as u32);
    for at in [0x08, 0x0C, 0x10] {
        put32(&mut i, at, now); // atime, ctime, mtime
    }
    put16(&mut i, 0x1A, links);
    put32(&mut i, 0x1C, (runs.iter().map(|r| r.1).sum::<u64>() * (BS / 512)) as u32);
    put32(&mut i, 0x20, EXT4_EXTENTS_FL);
    // i_block: extent header, then up to 4 leaf extents.
    put16(&mut i, 0x28, EXTENT_MAGIC);
    put16(&mut i, 0x2A, runs.len() as u16);
    put16(&mut i, 0x2C, 4); // eh_max
    let mut logical = 0;
    for (n, &(start, len)) in runs.iter().enumerate() {
        let e = 0x34 + 12 * n;
        put32(&mut i, e, logical as u32);
        put16(&mut i, e + 4, len as u16);
        put16(&mut i, e + 6, (start >> 32) as u16);
        put32(&mut i, e + 8, start as u32);
        logical += len;
    }
    put32(&mut i, 0x6C, (size >> 32) as u32);
    put16(&mut i, 0x80, EXTRA_ISIZE);
    put32(&mut i, 0x90, now); // crtime
    Ok(i)
}

/// Writes a directory entry at `off`, returning the offset just past its name (padded to 4).
fn dirent(buf: &mut [u8], off: usize, ino: u32, rec_len: u16, name: &[u8]) -> usize {
    put32(buf, off, ino);
    put16(buf, off + 4, rec_len);
    buf[off + 6] = name.len() as u8;
    buf[off + 7] = FT_DIR;
    buf[off + 8..off + 8 + name.len()].copy_from_slice(name);
    off + (8 + name.len()).next_multiple_of(4)
}

/// The directory hash seed, derived from the UUID so a format is reproducible.
fn hash_seed(uuid: &[u8; 16]) -> [u8; 16] {
    let mut state = u64::from_le_bytes(uuid[..8].try_into().unwrap_or_default())
        ^ u64::from_le_bytes(uuid[8..].try_into().unwrap_or_default()).rotate_left(29);
    let mut next = || {
        // splitmix64
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let mut seed = [0u8; 16];
    seed[..8].copy_from_slice(&next().to_le_bytes());
    seed[8..].copy_from_slice(&next().to_le_bytes());
    seed
}

fn set_bits(buf: &mut [u8], from: u64, to: u64) {
    for bit in from..to {
        buf[(bit / 8) as usize] |= 1 << (bit % 8);
    }
}

fn put16(buf: &mut [u8], at: usize, v: u16) {
    buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// The fields of an ext4 superblock a caller (or test) usually wants, read back from an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub inodes_count: u32,
    pub blocks_count: u64,
    pub reserved_blocks: u64,
    pub free_blocks: u64,
    pub free_inodes: u32,
    pub block_size: u64,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: u16,
    pub state: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
    pub label: String,
    pub journal_inode: u32,
    pub log_groups_per_flex: u8,
}

impl Superblock {
    /// Reads the primary superblock of the image in `file`.
    pub fn read(file: &File) -> io::Result<Superblock> {
        let mut sb = [0u8; 1024];
        file.read_exact_at(&mut sb, 1024)?;
        let u16_at = |at: usize| u16::from_le_bytes([sb[at], sb[at + 1]]);
        let u32_at = |at: usize| u32::from_le_bytes([sb[at], sb[at + 1], sb[at + 2], sb[at + 3]]);
        if u16_at(0x38) != EXT4_MAGIC {
            return Err(Error::new(ErrorKind::InvalidData, "not an ext2/3/4 filesystem (bad superblock magic)"));
        }
        let hi = |at: usize| if u32_at(0x60) & 0x80 != 0 { u64::from(u32_at(at)) << 32 } else { 0 };
        let label = &sb[0x78..0x88];
        Ok(Superblock {
            inodes_count: u32_at(0x00),
            blocks_count: u64::from(u32_at(0x04)) | hi(0x150),
            reserved_blocks: u64::from(u32_at(0x08)) | hi(0x154),
            free_blocks: u64::from(u32_at(0x0C)) | hi(0x158),
            free_inodes: u32_at(0x10),
            block_size: 1024 << u32_at(0x18),
            blocks_per_group: u32_at(0x20),
            inodes_per_group: u32_at(0x28),
            inode_size: u16_at(0x58),
            state: u16_at(0x3A),
            feature_compat: u32_at(0x5C),
            feature_incompat: u32_at(0x60),
            feature_ro_compat: u32_at(0x64),
            uuid: sb[0x68..0x78].try_into().unwrap_or_default(),
            label: String::from_utf8_lossy(&label[..label.iter().position(|&b| b == 0).unwrap_or(16)]).into_owned(),
            journal_inode: u32_at(0xE0),
            log_groups_per_flex: sb[0x174],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_super_groups() {
        let backups: Vec<u64> = (0..100).filter(|&g| has_super(g)).collect();
        assert_eq!(backups, [0, 1, 3, 5, 7, 9, 25, 27, 49, 81]);
    }

    #[test]
    fn small_last_group_is_dropped() {
        // 16 full groups plus 100 blocks: too small for a group of its own.
        let l = Layout::new((16 * BLOCKS_PER_GROUP + 100) * BS).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((l.groups, l.blocks), (16, 16 * BLOCKS_PER_GROUP));
        // ...but 1024 blocks is kept, and holds its own (one-group) flex metadata.
        let l = Layout::new((16 * BLOCKS_PER_GROUP + 1024) * BS).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(l.groups, 17);
        assert!(l.meta[16][2] >= 16 * BLOCKS_PER_GROUP && l.meta[16][2] + 512 <= l.blocks);
    }

    #[test]
    fn layout_runs_are_disjoint() {
        for size in [MIN_SIZE, 64 << 20, 1 << 30, 10 << 30, (3 << 40) + 12345] {
            let mut l = Layout::new(size).unwrap_or_else(|e| panic!("{e}"));
            let j = l.journal_blocks;
            let runs = l.alloc(1).and_then(|_| l.alloc(j)).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(runs.iter().map(|r| r.1).sum::<u64>(), j);
            assert!(l.used.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0), "{size}: overlapping runs");
            assert!(l.used.last().is_some_and(|r| r.0 + r.1 <= l.blocks));
            assert_eq!(l.inodes_per_group % INODES_PER_BLOCK, 0);
        }
    }

    #[test]
    fn rejects_bad_input() {
        let path = std::env::temp_dir().join(format!("emu-ext4-bad-{}", std::process::id()));
        let f = File::options().read(true).write(true).create(true).truncate(true).open(&path).unwrap_or_else(|e| panic!("{e}"));
        assert!(format(&f, MIN_SIZE - 1, "x", [0; 16]).is_err());
        assert!(format(&f, 64 << 20, "this label is too long", [0; 16]).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn superblock_round_trip() {
        let path = std::env::temp_dir().join(format!("emu-ext4-sb-{}", std::process::id()));
        let f = File::options().read(true).write(true).create(true).truncate(true).open(&path).unwrap_or_else(|e| panic!("{e}"));
        let uuid = *b"0123456789abcdef";
        format(&f, 64 << 20, "userdata", uuid).unwrap_or_else(|e| panic!("{e}"));
        let sb = Superblock::read(&f).unwrap_or_else(|e| panic!("{e}"));
        let _ = std::fs::remove_file(&path);
        assert_eq!(sb.blocks_count, 16384);
        assert_eq!(sb.block_size, 4096);
        assert_eq!(sb.inodes_count, 4096);
        assert_eq!(sb.free_inodes, 4096 - 11);
        assert_eq!((sb.feature_compat, sb.feature_incompat, sb.feature_ro_compat), (FEATURE_COMPAT, FEATURE_INCOMPAT, FEATURE_RO_COMPAT));
        assert_eq!((sb.label.as_str(), sb.uuid, sb.journal_inode, sb.state), ("userdata", uuid, 8, 1));
        // Used: superblock + 1 GDT block, 2 bitmaps, 256 inode-table blocks, root, lost+found, journal.
        assert_eq!(sb.free_blocks, 16384 - (2 + 2 + 256 + 1 + 4 + 1024));
    }
}
