//! Formats images of several sizes and checks them with every e2fsck available on the host: the
//! Android SDK's (e2fsprogs 1.42) and Homebrew's (current). Skipped when neither is installed, and
//! not built on Windows, where e2fsprogs doesn't run and `st_blocks` doesn't exist.
#![cfg(unix)]

use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use simee_emu_ext4::{FEATURE_COMPAT, FEATURE_INCOMPAT, FEATURE_RO_COMPAT, Superblock};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

fn e2fscks() -> Vec<PathBuf> {
    let sdk = std::env::var_os("ANDROID_HOME")
        .or_else(|| std::env::var_os("ANDROID_SDK_ROOT"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join("Library/Android/sdk")));
    sdk.map(|s| s.join("emulator/bin64/e2fsck"))
        .into_iter()
        .chain(["/opt/homebrew/opt/e2fsprogs/sbin/e2fsck".into(), "/usr/local/opt/e2fsprogs/sbin/e2fsck".into()])
        .filter(|p| p.is_file())
        .collect()
}

struct TempImage(PathBuf);

impl Drop for TempImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn check(size: u64) {
    let fscks = e2fscks();
    if fscks.is_empty() {
        eprintln!("skipping: no e2fsck found (Android SDK emulator package or Homebrew e2fsprogs)");
        return;
    }
    let image = TempImage(std::env::temp_dir().join(format!("emu-ext4-{}-{size}.img", std::process::id())));
    let file = File::options().read(true).write(true).create(true).truncate(true).open(&image.0).expect("create image");
    let uuid = *b"emu-rs-ext4-test";
    simee_emu_ext4::format(&file, size, "userdata", uuid).expect("format");

    let sb = Superblock::read(&file).expect("superblock");
    assert_eq!(sb.block_size, 4096);
    assert_eq!(sb.blocks_count, size / 4096 - dropped_blocks(size));
    assert_eq!((sb.feature_compat, sb.feature_incompat, sb.feature_ro_compat), (FEATURE_COMPAT, FEATURE_INCOMPAT, FEATURE_RO_COMPAT));
    assert_eq!((sb.label.as_str(), sb.uuid, sb.inode_size, sb.journal_inode, sb.state), ("userdata", uuid, 256, 8, 1));
    assert_eq!(sb.free_inodes, sb.inodes_count - 11);
    assert!(sb.free_blocks < sb.blocks_count && sb.free_blocks > sb.blocks_count / 2, "{sb:?}");
    assert_eq!(file.metadata().expect("metadata").len(), size);

    for fsck in &fscks {
        let out = Command::new(fsck).arg("-fn").arg(&image.0).output().expect("run e2fsck");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        eprintln!("{} ({size} bytes):\n{stdout}{stderr}", fsck.display());
        assert!(out.status.success(), "{} -fn failed ({}) on a {size}-byte image:\n{stdout}{stderr}", fsck.display(), out.status);
        // `-n` answers "no" to every question; any question means e2fsck found something.
        assert!(!stdout.contains('?') && !stderr.contains('?'), "e2fsck reported problems:\n{stdout}{stderr}");
        assert!(stdout.contains(&format!("11/{} files", sb.inodes_count)), "{stdout}");
    }
}

/// Blocks a too-small last group loses (mirrors the formatter's mke2fs rule for these sizes).
fn dropped_blocks(size: u64) -> u64 {
    if size == 2 * GIB + 400 * 1024 { 100 } else { 0 }
}

#[test]
fn e2fsck_64m() {
    check(64 * MIB);
}

#[test]
fn e2fsck_minimum() {
    check(simee_emu_ext4::MIN_SIZE);
}

#[test]
fn e2fsck_1g() {
    check(GIB);
}

#[test]
fn e2fsck_odd_size() {
    // Not a whole number of blocks, and a short last group.
    check(GIB + 300 * MIB + 1234);
}

#[test]
fn e2fsck_two_flex_groups() {
    // 17 groups: the last one starts a second flex group and holds its own metadata.
    check(2 * GIB + 4 * MIB);
}

#[test]
fn e2fsck_dropped_last_group() {
    check(2 * GIB + 400 * 1024);
}

#[test]
fn e2fsck_10g_stays_sparse() {
    check(10 * GIB);
    let path = std::env::temp_dir().join(format!("emu-ext4-sparse-{}.img", std::process::id()));
    let image = TempImage(path);
    let file = File::options().read(true).write(true).create(true).truncate(true).open(&image.0).expect("create image");
    simee_emu_ext4::format(&file, 10 * GIB, "userdata", [7; 16]).expect("format");
    let allocated = file.metadata().expect("metadata").blocks() * 512;
    eprintln!("10 GiB image: {allocated} bytes allocated");
    assert!(allocated < 2 * MIB, "10 GiB image allocates {allocated} bytes");
}
