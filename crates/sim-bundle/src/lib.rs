//! Portable device bundles (#170).
//!
//! A bundle describes a device with no host path in it: a [`Manifest`] (`manifest.json`) and
//! content-addressed chunks (`chunks/<aa>/<blake3>`, each compressed alone with LZ4). Disk images
//! are cut at fixed [`CHUNK_SIZE`] boundaries; all-zero chunks and holes are recorded as extents
//! and never stored. It is a directory, or the same as one plain uncompressed tar with the
//! manifest first, so an import streams it and checks each chunk as it arrives.
//!
//! Platforms fill in the manifest's identity and say what goes where (`emu-avd` for AVDs); this
//! crate packs, unpacks and verifies: every chunk against its hash, every disk and file against
//! its whole hash, every path (relative, no `..`), and the format's major version.

pub mod ios;
mod manifest;
mod store;
mod tar;

use sim_core::hostfs::{FileExt, allocated_bytes};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub use manifest::{
    AndroidImage, Base, CHUNK_SIZE, COLD_BOOT, COMPRESSION, Captures, Device, Disk, Entry, EntryKind, Extent, FORMAT, File, GuestIdentity,
    Identity, IosSimulator, Manifest, Secret, is_hash, safe_path,
};
pub use store::{ChunkStore, chunk_name, compress, decompress_verified, hash, hash_file, max_compressed, parse_chunk_name, read_regular};
pub use tar::{TarReader, TarWriter};

/// A bundle directory's manifest file, and a bundle archive's first entry.
pub const MANIFEST: &str = "manifest.json";

/// The largest manifest a reader accepts: a 64 GiB disk in 1 MiB chunks is ~7 MB of JSON.
pub const MAX_MANIFEST: u64 = 32 << 20;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("bundle format {0} isn't one this simee reads (it reads format {FORMAT}); update simee")]
    Format(u64),
    #[error("bad bundle manifest: {0}")]
    Manifest(String),
    #[error("unsafe path `{0}` in the bundle (absolute, empty, or with `.` or `..`)")]
    Path(String),
    #[error("chunk {0} doesn't match its hash")]
    ChunkHash(String),
    #[error("{0} doesn't match its hash in the manifest")]
    Hash(String),
    #[error("the bundle lacks {count} chunk(s) (first: {first})")]
    Missing { first: String, count: usize },
    #[error("bad bundle archive: {0}")]
    Tar(String),
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("reading the bundle: {0}")]
    Read(std::io::Error),
    #[error("writing the bundle: {0}")]
    Write(std::io::Error),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0} changed while it was exported")]
    Changed(String),
    #[error("{0} already exists")]
    Exists(PathBuf),
    /// A device or host that can't take part as asked (running, missing a runtime): why.
    #[error("{0}")]
    Refused(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// How to write a bundle.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Chunks compressed at once (0: one per CPU, at most 8).
    pub threads: usize,
}

/// What goes into a bundle: disk images read from host files, and small files given whole, each
/// at its path in the bundle.
#[derive(Clone, Debug, Default)]
pub struct Contents {
    pub disks: Vec<(String, PathBuf)>,
    pub files: Vec<(String, Vec<u8>)>,
}

/// What an export wrote, in the terms #169 measures: the disks' logical and allocated sizes
/// against the bundle's.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Summary {
    /// `dir` or `tar`.
    pub form: String,
    /// The disks' logical sizes, summed.
    pub disk_bytes: u64,
    /// What the disks take on the host (allocated blocks: holes cost nothing).
    pub allocated_bytes: u64,
    /// Bytes in all-zero chunks and holes, which the bundle records and never stores.
    pub zero_bytes: u64,
    /// Bytes in stored chunks, each time a disk or file refers to one.
    pub data_bytes: u64,
    /// Bytes in distinct chunks, uncompressed.
    pub unique_bytes: u64,
    pub chunks: u64,
    pub unique_chunks: u64,
    /// Bytes of compressed chunks the bundle holds.
    pub compressed_bytes: u64,
    /// The whole bundle: the archive, or the directory's manifest and chunk files.
    pub bundle_bytes: u64,
    pub seconds: f64,
}

/// Where one stored chunk's data is first found while exporting.
#[derive(Clone, Copy)]
enum Origin {
    Disk { disk: usize, offset: u64, len: u64 },
    File { file: usize, offset: u64, len: u64 },
}

fn is_zero(buf: &[u8]) -> bool {
    let (words, rest) = buf.as_chunks::<64>();
    words.iter().all(|w| w.iter().fold(0, |a, &b| a | b) == 0) && rest.iter().all(|&b| b == 0)
}

/// Reads `buf.len()` bytes at `offset`, or fewer at the end of the file.
fn read_at(f: &fs::File, path: &Path, buf: &mut [u8], offset: u64) -> Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match f.read_at(&mut buf[got..], offset + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Io(path.to_path_buf(), e)),
        }
    }
    Ok(got)
}

/// Cuts disk `path` (the host file `file`) into extents, hashing each chunk and the whole disk.
fn plan_disk(path: &str, file: &Path, chunk_size: u64) -> Result<Disk> {
    let f = fs::File::open(file).map_err(|e| Error::Io(file.to_path_buf(), e))?;
    let size = f.metadata().map_err(|e| Error::Io(file.to_path_buf(), e))?.len();
    let mut whole = blake3::Hasher::new();
    let mut buf = vec![0u8; chunk_size as usize];
    let mut extents: Vec<Extent> = Vec::new();
    let mut offset = 0;
    while offset < size {
        let want = (size - offset).min(chunk_size) as usize;
        if read_at(&f, file, &mut buf[..want], offset)? != want {
            return Err(Error::Changed(file.display().to_string()));
        }
        let data = &buf[..want];
        whole.update(data);
        match extents.last_mut() {
            Some(last) if last.zero && is_zero(data) => last.len += want as u64,
            _ if is_zero(data) => extents.push(Extent::zero(offset, want as u64)),
            _ => extents.push(Extent::chunk(offset, want as u64, hash(data))),
        }
        offset += want as u64;
    }
    Ok(Disk { path: path.to_owned(), size, blake3: whole.finalize().to_hex().to_string(), base: None, extents })
}

fn plan_file(path: &str, data: &[u8], chunk_size: u64) -> File {
    let chunks = data.chunks(chunk_size as usize).map(hash).collect();
    File { path: path.to_owned(), size: data.len() as u64, blake3: hash(data), chunks }
}

/// Where each stored chunk is first found, in manifest order.
fn origins(m: &Manifest) -> Vec<(String, Origin)> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for (disk, d) in m.disks.iter().enumerate() {
        for e in &d.extents {
            if let Some(h) = &e.hash
                && seen.insert(h.clone())
            {
                out.push((h.clone(), Origin::Disk { disk, offset: e.offset, len: e.len }));
            }
        }
    }
    for (file, f) in m.files.iter().enumerate() {
        for (i, h) in f.chunks.iter().enumerate() {
            let offset = i as u64 * m.chunk_size;
            if seen.insert(h.clone()) {
                out.push((h.clone(), Origin::File { file, offset, len: (f.size - offset).min(m.chunk_size) }));
            }
        }
    }
    out
}

enum Sink {
    Dir(ChunkStore),
    Tar(TarWriter<BufWriter<fs::File>>),
}

impl Sink {
    fn put(&mut self, hash: &str, compressed: &[u8]) -> Result<()> {
        match self {
            Sink::Dir(store) => store.put(hash, compressed).map(|_| ()),
            Sink::Tar(w) => w.entry(&chunk_name(hash), compressed),
        }
    }
}

/// Whether `to` names the single-file form (`.tar`); anything else is a directory.
pub fn is_archive(to: &Path) -> bool {
    to.extension().is_some_and(|x| x == "tar")
}

/// Writes `contents` as a bundle at `to` (a `.tar` archive, else a new directory), with
/// `manifest`'s description and identity; its chunk fields, disks and files are filled in here.
/// Reads every disk twice: once to cut and hash it, once to compress the distinct chunks (each
/// checked against the first pass, so a disk that changes in between fails the export).
pub fn export(mut manifest: Manifest, contents: &Contents, to: &Path, opts: &Options) -> Result<(Manifest, Summary)> {
    let start = Instant::now();
    if to.exists() {
        return Err(Error::Exists(to.to_path_buf()));
    }
    manifest.format = FORMAT;
    manifest.chunk_size = CHUNK_SIZE;
    manifest.compression = COMPRESSION.to_owned();
    manifest.disks = contents.disks.iter().map(|(p, f)| plan_disk(p, f, CHUNK_SIZE)).collect::<Result<_>>()?;
    manifest.files = contents.files.iter().map(|(p, d)| plan_file(p, d, CHUNK_SIZE)).collect();
    manifest.validate()?;
    let mut summary = Summary { form: if is_archive(to) { "tar" } else { "dir" }.into(), ..Summary::default() };
    for ((_, file), d) in contents.disks.iter().zip(&manifest.disks) {
        summary.disk_bytes += d.size;
        summary.allocated_bytes += fs::metadata(file).map(|m| allocated_bytes(&m)).unwrap_or(0);
        for e in &d.extents {
            if e.zero {
                summary.zero_bytes += e.len;
            } else {
                summary.data_bytes += e.len;
                summary.chunks += 1;
            }
        }
    }
    for f in &manifest.files {
        summary.data_bytes += f.size;
        summary.chunks += f.chunks.len() as u64;
    }

    let partial = to.with_file_name(format!(".{}.partial", to.file_name().and_then(|n| n.to_str()).unwrap_or("bundle")));
    let made = (|| -> Result<u64> {
        let mut sink = if is_archive(to) {
            let f = fs::File::create_new(&partial).map_err(|e| Error::Io(partial.clone(), e))?;
            let mut w = TarWriter::new(BufWriter::with_capacity(1 << 20, f));
            w.entry(MANIFEST, &manifest.to_json())?;
            Sink::Tar(w)
        } else {
            fs::create_dir(&partial).map_err(|e| Error::Io(partial.clone(), e))?;
            Sink::Dir(ChunkStore::new(&partial))
        };
        let disks: Vec<fs::File> =
            contents.disks.iter().map(|(_, f)| fs::File::open(f).map_err(|e| Error::Io(f.clone(), e))).collect::<Result<_>>()?;
        let threads = match opts.threads {
            0 => std::thread::available_parallelism().map_or(4, |n| n.get()).min(8),
            n => n,
        };
        let all = origins(&manifest);
        for batch in all.chunks(threads * 2) {
            let mut raw = Vec::with_capacity(batch.len());
            for (h, origin) in batch {
                let data = match *origin {
                    Origin::Disk { disk, offset, len } => {
                        let mut buf = vec![0u8; len as usize];
                        let path = &contents.disks[disk].1;
                        if read_at(&disks[disk], path, &mut buf, offset)? != buf.len() || hash(&buf) != *h {
                            return Err(Error::Changed(path.display().to_string()));
                        }
                        buf
                    }
                    Origin::File { file, offset, len } => contents.files[file].1[offset as usize..(offset + len) as usize].to_vec(),
                };
                raw.push(data);
            }
            let packed: Vec<Result<Vec<u8>>> = std::thread::scope(|s| {
                let per = raw.len().div_ceil(threads).max(1);
                let handles: Vec<_> =
                    raw.chunks(per).map(|group| s.spawn(move || group.iter().map(|d| Ok(compress(d))).collect::<Vec<_>>())).collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap_or_else(|_| vec![Err(Error::Unsupported("a compression thread panicked".into()))]))
                    .collect()
            });
            for (((h, _), data), z) in batch.iter().zip(&raw).zip(packed) {
                let z = z?;
                summary.unique_bytes += data.len() as u64;
                summary.unique_chunks += 1;
                summary.compressed_bytes += z.len() as u64;
                sink.put(h, &z)?;
            }
        }
        Ok(match sink {
            Sink::Tar(w) => {
                let written = w.written() + 1024;
                w.finish()?;
                written
            }
            Sink::Dir(_) => {
                let json = manifest.to_json();
                let path = partial.join(MANIFEST);
                fs::write(&path, &json).map_err(|e| Error::Io(path, e))?;
                summary.compressed_bytes + json.len() as u64
            }
        })
    })();
    match made {
        Ok(bytes) => {
            fs::rename(&partial, to).map_err(|e| Error::Io(to.to_path_buf(), e))?;
            summary.bundle_bytes = bytes;
            summary.seconds = (start.elapsed().as_secs_f64() * 100.0).round() / 100.0;
            Ok((manifest, summary))
        }
        Err(e) => {
            let _ = if partial.is_dir() { fs::remove_dir_all(&partial) } else { fs::remove_file(&partial) };
            Err(e)
        }
    }
}

enum Source {
    Dir(PathBuf),
    Tar(TarReader<BufReader<fs::File>>),
}

/// An opened bundle: its manifest, read and validated, and the chunks still to read.
pub struct Reader {
    manifest: Manifest,
    source: Source,
}

/// What [`Reader::extract`] wrote.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Extracted {
    /// Bytes written from chunks (zeros are left as holes).
    pub written_bytes: u64,
    pub chunks: u64,
    pub seconds: f64,
}

impl Reader {
    /// Opens bundle `path` (a directory, or an archive whose first entry is the manifest) and
    /// validates its manifest. A bundle with a base delta (#171) is refused here, before anything
    /// is written.
    pub fn open(path: &Path) -> Result<Reader> {
        let (manifest, source) = if path.is_dir() {
            let bytes = read_regular(&path.join(MANIFEST), MAX_MANIFEST)?;
            (Manifest::parse(&bytes)?, Source::Dir(path.to_path_buf()))
        } else {
            let f = fs::File::open(path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
            let mut tar = TarReader::new(BufReader::with_capacity(1 << 20, f));
            match tar.next_entry(MAX_MANIFEST)? {
                Some((name, bytes)) if name == MANIFEST => (Manifest::parse(&bytes)?, Source::Tar(tar)),
                Some((name, _)) => return Err(Error::Tar(format!("the first entry is `{name}`, not {MANIFEST}"))),
                None => return Err(Error::Tar("the archive is empty".into())),
            }
        };
        if let Some(d) = manifest.disks.iter().find(|d| d.base.is_some()) {
            return Err(Error::Unsupported(format!("{} is a delta against a base disk (#171), which this simee can't apply yet", d.path)));
        }
        Ok(Reader { manifest, source })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Writes every disk and file under `dest` (an existing directory), each at its path. Disks
    /// are created at their size with nothing written where they are zero (holes); each chunk is
    /// checked against its hash as it arrives, the bundle must hold every chunk the manifest
    /// names and no other, and each disk and file is read back and checked against its whole
    /// hash. On an error, what was written stays for the caller to remove.
    pub fn extract(self, dest: &Path) -> Result<Extracted> {
        let start = Instant::now();
        let m = self.manifest;
        let mut targets: Vec<(PathBuf, fs::File)> = Vec::new();
        let mut places: BTreeMap<String, Vec<(usize, u64, u64)>> = BTreeMap::new();
        let mut create = |path: &str, size: u64| -> Result<usize> {
            if !safe_path(path) {
                return Err(Error::Path(path.to_owned()));
            }
            let p = dest.join(path);
            if let Some(dir) = p.parent() {
                fs::create_dir_all(dir).map_err(|e| Error::Io(dir.to_path_buf(), e))?;
            }
            let f = fs::File::create_new(&p).map_err(|e| Error::Io(p.clone(), e))?;
            f.set_len(size).map_err(|e| Error::Io(p.clone(), e))?;
            targets.push((p, f));
            Ok(targets.len() - 1)
        };
        for d in &m.disks {
            let t = create(&d.path, d.size)?;
            for e in &d.extents {
                if let Some(h) = &e.hash {
                    places.entry(h.clone()).or_default().push((t, e.offset, e.len));
                }
            }
        }
        for f in &m.files {
            let t = create(&f.path, f.size)?;
            for (i, h) in f.chunks.iter().enumerate() {
                let offset = i as u64 * m.chunk_size;
                places.entry(h.clone()).or_default().push((t, offset, (f.size - offset).min(m.chunk_size)));
            }
        }
        let mut out = Extracted::default();
        let mut place = |h: &str, data: &[u8], places: &mut BTreeMap<String, Vec<(usize, u64, u64)>>| -> Result<()> {
            let spots = places.remove(h).ok_or_else(|| Error::Tar(format!("chunk {h} isn't in the manifest, or comes twice")))?;
            for (t, offset, len) in spots {
                if len != data.len() as u64 {
                    return Err(Error::ChunkHash(h.to_owned()));
                }
                let (path, f) = &targets[t];
                f.write_all_at(data, offset).map_err(|e| Error::Io(path.clone(), e))?;
                out.written_bytes += len;
            }
            out.chunks += 1;
            Ok(())
        };
        match self.source {
            Source::Dir(root) => {
                let store = ChunkStore::new(&root);
                for h in m.chunk_hashes() {
                    let data = store.get(h, m.chunk_size)?;
                    place(h, &data, &mut places)?;
                }
            }
            Source::Tar(mut tar) => {
                // A chunk that doesn't compress is a little larger than itself.
                let max = max_compressed(m.chunk_size);
                while let Some((name, z)) = tar.next_entry(max)? {
                    let h = parse_chunk_name(&name).ok_or_else(|| Error::Tar(format!("unexpected entry `{name}`")))?;
                    let data = decompress_verified(h, &z, m.chunk_size)?;
                    place(h, &data, &mut places)?;
                }
            }
        }
        if let Some(first) = places.keys().next() {
            return Err(Error::Missing { first: first.clone(), count: places.len() });
        }
        let wholes = m.disks.iter().map(|d| (&d.path, &d.blake3)).chain(m.files.iter().map(|f| (&f.path, &f.blake3)));
        for ((path, f), (name, want)) in targets.iter().zip(wholes) {
            let mut h = blake3::Hasher::new();
            let mut r = BufReader::with_capacity(1 << 20, f);
            std::io::Seek::rewind(&mut r).map_err(|e| Error::Io(path.clone(), e))?;
            h.update_reader(&mut r).map_err(|e| Error::Io(path.clone(), e))?;
            if h.finalize().to_hex().as_str() != want.as_str() {
                return Err(Error::Hash(name.clone()));
            }
        }
        out.seconds = (start.elapsed().as_secs_f64() * 100.0).round() / 100.0;
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
