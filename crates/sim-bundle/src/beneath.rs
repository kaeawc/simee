//! Writing under a destination directory without following any symlink below it. Every
//! directory on a path is opened relative to its parent with `O_NOFOLLOW`, so a link in an
//! extracted tree (spelled as the bundle has it, or in another case or Unicode normalization,
//! which a case-insensitive volume resolves to the same name) can't take a write outside.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

#[cfg(unix)]
pub(crate) use unix::Beneath;

#[cfg(unix)]
mod unix {
    use super::*;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

    /// `rel`'s parent (empty for the root) and its last component.
    fn split(rel: &str) -> (&str, &str) {
        rel.rsplit_once('/').unwrap_or(("", rel))
    }

    /// Permission bits (at most `0o7777`, as a manifest holds them) as `mode_t`, which is 16 bits
    /// on macOS and 32 on Linux.
    #[allow(clippy::unnecessary_cast, clippy::cast_possible_truncation)]
    fn mode_of(bits: u32) -> Mode {
        Mode::from_raw_mode((bits & 0o7777) as rustix::fs::RawMode)
    }

    /// A destination directory, open, that paths are written under.
    pub(crate) struct Beneath {
        root: OwnedFd,
        path: PathBuf,
    }

    impl Beneath {
        pub(crate) fn new(root: &Path) -> Result<Beneath> {
            let fd = rustix::fs::open(root, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
                .map_err(|e| Error::Io(root.to_path_buf(), e.into()))?;
            Ok(Beneath { root: fd, path: root.to_path_buf() })
        }

        /// Why `comp` in `base` (at `prefix`) couldn't be opened: a link in the way, or `e`.
        fn fail(&self, base: BorrowedFd<'_>, comp: &str, prefix: &str, e: Errno) -> Error {
            let link = matches!(e, Errno::LOOP | Errno::NOTDIR | Errno::MLINK)
                && rustix::fs::statat(base, comp, AtFlags::SYMLINK_NOFOLLOW)
                    .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Symlink);
            if link { Error::ThroughLink(prefix.to_owned()) } else { Error::Io(self.path.join(prefix), e.into()) }
        }

        /// The directory at `rel` (the root for `""`), each component opened without following a
        /// link and, with `create`, made when it is missing.
        fn dir(&self, rel: &str, create: bool) -> Result<OwnedFd> {
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let mut cur = self.root.try_clone().map_err(|e| Error::Io(self.path.clone(), e))?;
            if rel.is_empty() {
                return Ok(cur);
            }
            let mut end = 0;
            for comp in rel.split('/') {
                end += comp.len();
                let prefix = &rel[..end];
                end += 1;
                let base = cur.as_fd();
                let next = match rustix::fs::openat(base, comp, flags, Mode::empty()) {
                    Err(Errno::NOENT) if create => {
                        match rustix::fs::mkdirat(base, comp, Mode::from_raw_mode(0o755)) {
                            Ok(()) | Err(Errno::EXIST) => {}
                            Err(e) => return Err(self.fail(base, comp, prefix, e)),
                        }
                        rustix::fs::openat(base, comp, flags, Mode::empty())
                    }
                    r => r,
                };
                cur = next.map_err(|e| self.fail(base, comp, prefix, e))?;
            }
            Ok(cur)
        }

        /// Opens `rel` (the last component too) with `flags` and `O_NOFOLLOW`.
        fn open_at(&self, rel: &str, flags: OFlags, mode: u32, create_parents: bool) -> Result<OwnedFd> {
            let (parent, name) = split(rel);
            let dir = self.dir(parent, create_parents)?;
            rustix::fs::openat(&dir, name, flags | OFlags::NOFOLLOW | OFlags::CLOEXEC, mode_of(mode))
                .map_err(|e| self.fail(dir.as_fd(), name, rel, e))
        }

        /// Creates the new file `rel` (and its missing parents), open to read and write.
        pub(crate) fn create(&self, rel: &str) -> Result<fs::File> {
            Ok(self.open_at(rel, OFlags::RDWR | OFlags::CREATE | OFlags::EXCL, 0o666, true)?.into())
        }

        /// Opens the existing file `rel` to read and write.
        pub(crate) fn open(&self, rel: &str) -> Result<fs::File> {
            Ok(self.open_at(rel, OFlags::RDWR, 0, false)?.into())
        }

        /// Makes directory `rel` and its missing parents.
        pub(crate) fn mkdir_all(&self, rel: &str) -> Result<()> {
            self.dir(rel, true).map(drop)
        }

        /// Makes symlink `rel` to `target` (and its missing parents).
        pub(crate) fn symlink(&self, rel: &str, target: &str) -> Result<()> {
            let (parent, name) = split(rel);
            let dir = self.dir(parent, true)?;
            rustix::fs::symlinkat(target, &dir, name).map_err(|e| self.fail(dir.as_fd(), name, rel, e))
        }

        /// Sets the permission bits of `rel`, a directory or a regular file (never a link's target).
        pub(crate) fn chmod(&self, rel: &str, mode: u32, is_dir: bool) -> Result<()> {
            let flags = if is_dir { OFlags::RDONLY | OFlags::DIRECTORY } else { OFlags::RDONLY };
            let fd = self.open_at(rel, flags, 0, false)?;
            rustix::fs::fchmod(&fd, mode_of(mode)).map_err(|e| Error::Io(self.path.join(rel), e.into()))
        }

        /// The bytes free to an unprivileged writer on the destination's volume.
        pub(crate) fn free_bytes(&self) -> Option<u64> {
            let st = rustix::fs::fstatvfs(&self.root).ok()?;
            Some(st.f_bavail.saturating_mul(st.f_frsize))
        }

        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }
}

#[cfg(not(unix))]
pub(crate) use other::Beneath;

/// Hosts with no `openat` (Windows): plain paths. Nothing makes symlinks there (an iOS tree is
/// applied on macOS only), and every extract goes into a directory it just made.
#[cfg(not(unix))]
mod other {
    use super::*;

    pub(crate) struct Beneath {
        path: PathBuf,
    }

    impl Beneath {
        pub(crate) fn new(root: &Path) -> Result<Beneath> {
            if !root.is_dir() {
                return Err(Error::Io(root.to_path_buf(), std::io::ErrorKind::NotADirectory.into()));
            }
            Ok(Beneath { path: root.to_path_buf() })
        }

        pub(crate) fn create(&self, rel: &str) -> Result<fs::File> {
            let p = self.path.join(rel);
            if let Some(dir) = p.parent() {
                fs::create_dir_all(dir).map_err(|e| Error::Io(dir.to_path_buf(), e))?;
            }
            fs::File::create_new(&p).map_err(|e| Error::Io(p, e))
        }

        pub(crate) fn open(&self, rel: &str) -> Result<fs::File> {
            let p = self.path.join(rel);
            fs::OpenOptions::new().read(true).write(true).open(&p).map_err(|e| Error::Io(p, e))
        }

        pub(crate) fn free_bytes(&self) -> Option<u64> {
            None
        }

        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }
}
