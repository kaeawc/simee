//! The content-addressed chunk store: `chunks/<aa>/<blake3>`, each chunk one LZ4 block of the
//! data whose BLAKE3 names it. Identical data is stored once, and a reader checks every chunk
//! against its name.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// The BLAKE3 of `data`, as the manifest writes it.
pub fn hash(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// The BLAKE3 of the file at `path`, read in 1 MiB pieces (a 4.4 GB system image: ~3 s).
pub fn hash_file(path: &Path) -> Result<String> {
    let f = std::fs::File::open(path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    let mut h = blake3::Hasher::new();
    h.update_reader(std::io::BufReader::with_capacity(1 << 20, f)).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    Ok(h.finalize().to_hex().to_string())
}

/// The most a chunk of `len` bytes takes compressed (LZ4's bound for data that doesn't compress).
pub fn max_compressed(len: u64) -> u64 {
    lz4_flex::block::get_maximum_output_size(len as usize) as u64
}

/// `data` as one LZ4 block (no size header: the manifest gives each chunk's length).
pub fn compress(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; max_compressed(data.len() as u64) as usize];
    // The buffer is LZ4's own bound for this input, so the block always fits.
    let n = lz4_flex::block::compress_into(data, &mut out).unwrap_or(0);
    out.truncate(n);
    out
}

/// Decompresses chunk `hash` into at most `max` bytes (a block that would decompress past it is
/// refused) and checks it is what its name says.
pub fn decompress_verified(hash_hex: &str, compressed: &[u8], max: u64) -> Result<Vec<u8>> {
    let mut data = vec![0u8; max as usize];
    let n = lz4_flex::block::decompress_into(compressed, &mut data).map_err(|_| Error::ChunkHash(hash_hex.to_owned()))?;
    data.truncate(n);
    if hash(&data) != hash_hex {
        return Err(Error::ChunkHash(hash_hex.to_owned()));
    }
    Ok(data)
}

/// Reads bundle file `path` whole: a regular file (not a symlink, a device or a FIFO) of at most
/// `limit` bytes, so a directory bundle can't make a reader follow a link to `/dev/zero`.
pub fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    if !meta.file_type().is_file() {
        return Err(Error::Unsupported(format!("{} isn't a regular file", path.display())));
    }
    if meta.len() > limit {
        return Err(Error::Unsupported(format!("{} is {} bytes, more than {limit}", path.display(), meta.len())));
    }
    let f = std::fs::File::open(path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    let mut v = Vec::with_capacity(meta.len() as usize);
    f.take(limit + 1).read_to_end(&mut v).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    if v.len() as u64 > limit {
        return Err(Error::Unsupported(format!("{} grew past {limit} bytes", path.display())));
    }
    Ok(v)
}

/// The name a chunk has in a bundle: `chunks/<first two hex digits>/<hash>`.
pub fn chunk_name(hash: &str) -> String {
    format!("chunks/{}/{hash}", &hash[..2])
}

/// The hash a bundle entry named like [`chunk_name`] holds, if it is named so.
pub fn parse_chunk_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("chunks/")?;
    let (dir, hash) = rest.split_once('/')?;
    (crate::manifest::is_hash(hash) && dir == &hash[..2]).then_some(hash)
}

/// A bundle directory's chunks.
pub struct ChunkStore {
    root: PathBuf,
}

impl ChunkStore {
    /// The store of the bundle directory `root` (its `chunks/`).
    pub fn new(root: &Path) -> ChunkStore {
        ChunkStore { root: root.to_path_buf() }
    }

    pub fn path(&self, hash: &str) -> PathBuf {
        self.root.join(chunk_name(hash))
    }

    pub fn has(&self, hash: &str) -> bool {
        self.path(hash).is_file()
    }

    /// Stores `compressed` as chunk `hash` unless the store has it; whether it was new. Written
    /// beside and renamed, so a chunk file is whole or absent.
    pub fn put(&self, hash: &str, compressed: &[u8]) -> Result<bool> {
        let path = self.path(hash);
        if path.is_file() {
            return Ok(false);
        }
        let dir = path.parent().unwrap_or(&self.root);
        std::fs::create_dir_all(dir).map_err(|e| Error::Io(dir.to_path_buf(), e))?;
        let tmp = dir.join(format!(".{hash}.partial"));
        std::fs::write(&tmp, compressed).map_err(|e| Error::Io(tmp.clone(), e))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::Io(path.clone(), e))?;
        Ok(true)
    }

    /// Chunk `hash`, decompressed (at most `max` bytes) and checked against its name.
    pub fn get(&self, hash: &str, max: u64) -> Result<Vec<u8>> {
        let path = self.path(hash);
        let compressed = match read_regular(&path, max_compressed(max)) {
            Err(Error::Io(_, e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::Missing { first: hash.to_owned(), count: 1 });
            }
            r => r?,
        };
        decompress_verified(hash, &compressed, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_chunks_are_stored_once_and_checked_on_read() {
        let root = std::env::temp_dir().join(format!("sim-bundle-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = ChunkStore::new(&root);
        let data = vec![42u8; 100_000];
        let h = hash(&data);
        let z = compress(&data);
        assert!(z.len() < 1000, "a run compresses");
        // A block that decompresses past what the chunk may hold is refused, not grown into.
        assert!(matches!(decompress_verified(&h, &z, 99_999), Err(Error::ChunkHash(_))));
        assert_eq!(decompress_verified(&h, &z, 100_000).expect("exact"), data);
        assert!(store.put(&h, &z).expect("put"));
        assert!(!store.put(&h, &z).expect("put again"), "already stored");
        assert_eq!(std::fs::read_dir(root.join("chunks")).expect("chunks").count(), 1);
        assert_eq!(store.path(&h), root.join(format!("chunks/{}/{h}", &h[..2])));
        assert_eq!(parse_chunk_name(&chunk_name(&h)), Some(h.as_str()));
        assert_eq!(parse_chunk_name(&format!("chunks/zz/{h}")), None, "the directory is the hash's prefix");
        assert_eq!(parse_chunk_name("chunks/../x"), None);
        assert_eq!(store.get(&h, 1 << 20).expect("get"), data);
        assert!(store.get(&h, 10).is_err(), "longer than allowed");
        // A chunk whose content isn't what its name says.
        std::fs::write(store.path(&h), compress(b"other")).expect("overwrite");
        assert!(matches!(store.get(&h, 1 << 20), Err(Error::ChunkHash(_))));
        assert!(matches!(store.get(&hash(b"absent"), 1 << 20), Err(Error::Missing { .. })));
        // A chunk that is a link (to /dev/zero, which would read forever) is refused unread.
        #[cfg(unix)]
        {
            std::fs::remove_file(store.path(&h)).expect("rm");
            std::os::unix::fs::symlink("/dev/zero", store.path(&h)).expect("symlink");
            assert!(matches!(store.get(&h, 1 << 20), Err(Error::Unsupported(m)) if m.contains("regular file")));
            assert!(read_regular(Path::new("/dev/zero"), 1 << 20).is_err(), "a device");
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
