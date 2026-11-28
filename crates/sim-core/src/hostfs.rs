//! File and process operations whose std spelling is unix-only: positional reads and writes
//! (`pread`, `pwrite`), the space a file takes on disk, the `stat` fields, permission modes and
//! process groups.
//!
//! On unix these are std's own traits. On Windows they are stand-ins that let the code compile
//! and run, each documented where it only approximates: [`FileExt`] is over `seek_read` and
//! `seek_write`, which move the file's cursor (don't mix them with `Read`/`Write` on one
//! handle); [`MetadataExt`] has no inode or change time; [`PermissionsExt`] only knows read-only;
//! [`OpenOptionsExt::mode`] and [`CommandExt::process_group`] do nothing.

use std::fs::Metadata;

#[cfg(unix)]
pub use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
pub use std::os::unix::process::CommandExt;

#[cfg(windows)]
pub use windows::{CommandExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};

/// Bytes the filesystem allocated to the file (`st_blocks`), so a sparse file counts its data
/// only. Windows reports the length: std has no allocated size there.
pub fn allocated_bytes(meta: &Metadata) -> u64 {
    #[cfg(unix)]
    return std::os::unix::fs::MetadataExt::blocks(meta) * 512;
    #[cfg(not(unix))]
    return meta.len();
}

#[cfg(windows)]
mod windows {
    use std::fs::{Metadata, Permissions};
    use std::io;
    use std::os::windows::fs::FileExt as WindowsFileExt;
    use std::process::Command;
    use std::time::UNIX_EPOCH;

    /// `stat` fields on Windows: the modification time and size as unix has them, no inode
    /// (`ino` and `dev` are always 0, so a stamp built from them leans on the other fields), no change time.
    pub trait MetadataExt {
        fn ino(&self) -> u64;
        fn dev(&self) -> u64;
        fn size(&self) -> u64;
        fn mtime(&self) -> i64;
        fn mtime_nsec(&self) -> i64;
    }

    impl MetadataExt for Metadata {
        fn ino(&self) -> u64 {
            0
        }

        fn dev(&self) -> u64 {
            0
        }

        fn size(&self) -> u64 {
            self.len()
        }

        fn mtime(&self) -> i64 {
            self.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64)
        }

        fn mtime_nsec(&self) -> i64 {
            self.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| i64::from(d.subsec_nanos()))
        }
    }

    /// Permission bits on Windows know only read-only: `0o555` when it is, `0o755` otherwise (a
    /// file there is executable by its extension, which a mode doesn't say). Nothing sets a
    /// mode: `from_mode` doesn't exist there.
    pub trait PermissionsExt {
        fn mode(&self) -> u32;
    }

    impl PermissionsExt for Permissions {
        fn mode(&self) -> u32 {
            if self.readonly() { 0o555 } else { 0o755 }
        }
    }

    /// Creation modes don't exist on Windows: `mode` is accepted and ignored. A file made in the
    /// per-user temp directory is private by that directory's ACL.
    pub trait OpenOptionsExt {
        fn mode(&mut self, mode: u32) -> &mut Self;
    }

    impl OpenOptionsExt for std::fs::OpenOptions {
        fn mode(&mut self, _mode: u32) -> &mut Self {
            self
        }
    }

    /// Process groups don't exist on Windows (a job object would stand in): `process_group` is
    /// accepted and ignored.
    pub trait CommandExt {
        fn process_group(&mut self, pgroup: i32) -> &mut Command;
    }

    impl CommandExt for Command {
        fn process_group(&mut self, _pgroup: i32) -> &mut Command {
            self
        }
    }

    pub trait FileExt {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
        fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize>;

        fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
            while !buf.is_empty() {
                match self.read_at(buf, offset) {
                    Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(n) => (buf, offset) = (&mut buf[n..], offset + n as u64),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }

        fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
            while !buf.is_empty() {
                match self.write_at(buf, offset) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(n) => (buf, offset) = (&buf[n..], offset + n as u64),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }

    /// Windows' `seek_read` and `seek_write` move the file's cursor and `pread` and `pwrite` don't;
    /// callers rely on that (a sequential reader after a positional peek at the header or trailer).
    /// The cursor is put back afterwards. Two threads using the cursor and positions on one handle
    /// at once can still disturb each other's cursor, never each other's positional results.
    fn keeping_cursor<T>(file: &std::fs::File, op: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        use std::io::{Seek, SeekFrom};
        let mut file = file;
        let cursor = file.stream_position()?;
        let done = op();
        file.seek(SeekFrom::Start(cursor))?;
        done
    }

    impl FileExt for std::fs::File {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            keeping_cursor(self, || self.seek_read(buf, offset))
        }

        fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
            keeping_cursor(self, || self.seek_write(buf, offset))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::process::Command;

    use super::*;

    #[test]
    fn positional_io_leaves_the_cursor_where_it_was() {
        use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
        let path = std::env::temp_dir().join(format!("hostfs-cursor-{}", std::process::id()));
        let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("open");
        f.write_all(b"0123456789").expect("write");
        f.seek(SeekFrom::Start(2)).expect("seek");
        let mut tail = [0u8; 3];
        f.read_exact_at(&mut tail, 7).expect("pread");
        assert_eq!(&tail, b"789");
        f.write_all_at(b"ab", 8).expect("pwrite");
        let mut next = [0u8; 2];
        f.read_exact(&mut next).expect("sequential");
        assert_eq!(&next, b"23", "the cursor stayed at 2");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_process_group_request_is_accepted_everywhere() {
        let mut command = Command::new("any");
        command.process_group(0);
        std::fs::OpenOptions::new().mode(0o600);
    }

    #[test]
    fn positional_io_round_trips_and_reports_a_short_file() {
        let path = std::env::temp_dir().join(format!("sim-core-positional-{}", std::process::id()));
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("create");
        f.write_all_at(b"hello", 10).expect("write past the end");
        f.write_all_at(b"HE", 10).expect("overwrite");
        let mut back = [0u8; 15];
        f.read_exact_at(&mut back, 0).expect("read");
        assert_eq!(&back[..10], &[0; 10], "the gap reads as zeroes");
        assert_eq!(&back[10..], b"HEllo");
        let mut tail = [0u8; 4];
        assert_eq!(f.read_at(&mut tail, 13).expect("short read"), 2);
        assert_eq!(f.read_exact_at(&mut tail, 13).expect_err("past the end").kind(), io::ErrorKind::UnexpectedEof);
        let meta = f.metadata().expect("meta");
        assert!(allocated_bytes(&meta) > 0);
        assert_eq!(meta.size(), 15);
        assert!(meta.mtime() > 1_600_000_000, "modified after 2020");
        assert_ne!(meta.permissions().mode() & 0o200, 0, "a writable file");
        let _ = std::fs::remove_file(&path);
    }
}
