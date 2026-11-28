//! [`clone_file`] on hosts without the clone and hole-seeking calls `clone.rs` uses (Windows): a
//! plain copy. It takes the full size of a sparse image on disk and the time to write it; a
//! block clone (ReFS, Dev Drive) or a copy that keeps holes (`FSCTL_QUERY_ALLOCATED_RANGES`)
//! would replace it. The names and results match `clone.rs`, so callers don't branch.

use std::io;
use std::path::Path;

use crate::{Error, Result};

/// How a file was copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloneKind {
    /// A copy-on-write clone: no data copied, blocks shared until either side writes.
    Reflink,
    /// Only the data extents copied: the file stays as sparse as the original.
    Sparse,
    /// `std::fs::copy`, on platforms with neither clones nor `SEEK_DATA`.
    Copy,
}

impl CloneKind {
    pub fn label(self) -> &'static str {
        match self {
            CloneKind::Reflink => "reflink",
            CloneKind::Sparse => "sparse copy",
            CloneKind::Copy => "copy",
        }
    }
}

/// Copies `from` to the new file `to`. `to` must not exist, and a failed copy leaves no
/// half-written file behind.
pub fn clone_file(from: &Path, to: &Path) -> Result<CloneKind> {
    if to.symlink_metadata().is_ok() {
        return Err(Error::Io(to.to_path_buf(), io::ErrorKind::AlreadyExists.into()));
    }
    match std::fs::copy(from, to) {
        Ok(_) => Ok(CloneKind::Copy),
        Err(e) => {
            let _ = std::fs::remove_file(to);
            Err(Error::Io(from.to_path_buf(), e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_content_and_never_overwrites() {
        let tmp = std::env::temp_dir().join(format!("emu-avd-clonecopy-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let (src, dst) = (tmp.join("src.img"), tmp.join("dst.img"));
        std::fs::write(&src, b"disk contents").expect("src");
        assert_eq!(clone_file(&src, &dst).expect("clone"), CloneKind::Copy);
        assert_eq!(std::fs::read(&dst).expect("dst"), b"disk contents");
        assert!(matches!(clone_file(&src, &dst), Err(Error::Io(_, e)) if e.kind() == io::ErrorKind::AlreadyExists), "never overwrites");
        assert!(clone_file(&tmp.join("missing"), &tmp.join("other")).is_err());
        assert!(!tmp.join("other").exists(), "no half-written file");
        let _ = std::fs::remove_dir_all(tmp);
    }
}
