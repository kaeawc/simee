use super::*;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sim-bundle-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).expect("scratch");
    d
}

/// A manifest's description, as a platform fills it in before [`export`].
fn header() -> Manifest {
    let image = AndroidImage {
        api: "36".into(),
        tag: "google_apis".into(),
        abi: "arm64-v8a".into(),
        revision: "7".into(),
        package: "system-images;android-36;google_apis;arm64-v8a".into(),
        files: [("system.img".to_owned(), hash(b"s"))].into_iter().collect(),
        fingerprint: "google/sdk_gphone64_arm64/emu64a:16/X/1:userdebug/dev-keys".into(),
    };
    Manifest {
        format: FORMAT,
        simee_version: "0.1.0".into(),
        platform: sim_core::platform::Platform::Android,
        device: Device { name: "pixel".into(), form_factor: Some("phone".into()) },
        identity: Identity::Android(image),
        captures: Captures { disks: true, ram: false },
        resume: COLD_BOOT.into(),
        secrets: vec![Secret { item: "simee/encryptionkey.img".into(), what: "key".into(), excludable: false }],
        guest_identity: GuestIdentity { on_import: "duplicated".into(), identifiers: vec!["ro.serialno".into()] },
        dropped: Vec::new(),
        chunk_size: 0,
        compression: String::new(),
        disks: Vec::new(),
        files: Vec::new(),
        tree: Vec::new(),
    }
}

/// Zero chunks in [`sample_disk`]: more than the 16 MiB gap below which APFS fills a hole between
/// written blocks with zeros (measured on macOS 26), so the import's hole survives there too.
const ZEROS: u64 = 20;

/// A sparse disk: two identical data chunks, a hole, a short distinct tail.
fn sample_disk(dir: &Path) -> (PathBuf, Vec<u8>) {
    let c = CHUNK_SIZE as usize;
    let z = ZEROS as usize;
    let mut data = vec![0u8; (2 + z) * c + 1000];
    for (i, b) in data[..c].iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    data.copy_within(..c, c);
    data[(2 + z) * c..].fill(9);
    let path = dir.join("userdata.img");
    fs::write(&path, &data).expect("disk");
    (path, data)
}

fn contents(disk: &Path) -> Contents {
    Contents {
        disks: vec![("simee/userdata.img".into(), disk.to_path_buf())],
        files: vec![("config.ini".into(), b"AvdId=pixel\n".to_vec()), ("boot.prop".into(), Vec::new())],
    }
}

#[test]
fn manifests_round_trip_through_json() {
    let tmp = scratch("roundtrip");
    let (disk, _) = sample_disk(&tmp);
    let (m, _) = export(header(), &contents(&disk), &tmp.join("b"), &Options::default()).expect("export");
    let back = Manifest::parse(&m.to_json()).expect("parse");
    assert_eq!(back, m);
    let json: serde_json::Value = serde_json::from_slice(&m.to_json()).expect("json");
    assert_eq!(json["format"], 1);
    assert_eq!(json["identity"]["android"]["api"], "36");
    assert_eq!((json["captures"]["ram"].as_bool(), json["resume"].as_str()), (Some(false), Some("cold-boot")));
    assert!(json["disks"][0].get("base").is_none(), "base is reserved and absent");
    let _ = fs::remove_dir_all(tmp);
}

#[test]
fn identical_chunks_are_stored_once_and_zeros_never() {
    let tmp = scratch("dedup");
    let (disk, data) = sample_disk(&tmp);
    let (m, s) = export(header(), &contents(&disk), &tmp.join("b"), &Options::default()).expect("export");
    let d = &m.disks[0];
    let c = CHUNK_SIZE;
    // chunk, same chunk, zeros (coalesced into one extent), tail.
    let shape: Vec<(u64, u64, bool)> = d.extents.iter().map(|e| (e.offset, e.len, e.zero)).collect();
    assert_eq!(shape, [(0, c, false), (c, c, false), (2 * c, ZEROS * c, true), ((2 + ZEROS) * c, 1000, false)]);
    assert_eq!(d.extents[0].hash, d.extents[1].hash);
    assert_eq!(d.blake3, hash(&data));
    assert_eq!((s.disk_bytes, s.zero_bytes), (data.len() as u64, ZEROS * c));
    // Distinct: one data chunk, the tail, config.ini (the empty boot.prop has none).
    assert_eq!((s.chunks, s.unique_chunks), (4, 3));
    let stored: Vec<_> =
        fs::read_dir(tmp.join("b/chunks")).expect("chunks").flatten().flat_map(|e| fs::read_dir(e.path()).expect("dir")).collect();
    assert_eq!(stored.len(), 3);
    assert!(s.compressed_bytes < c / 10, "patterned data compresses: {}", s.compressed_bytes);
    let _ = fs::remove_dir_all(tmp);
}

#[test]
fn both_forms_unpack_to_the_same_bytes_with_holes_kept() {
    let tmp = scratch("unpack");
    let (disk, data) = sample_disk(&tmp);
    let (m, s) = export(header(), &contents(&disk), &tmp.join("b.tar"), &Options { threads: 2 }).expect("export tar");
    assert_eq!(s.form, "tar");
    assert_eq!(fs::metadata(tmp.join("b.tar")).expect("tar").len(), s.bundle_bytes);
    export(header(), &contents(&disk), &tmp.join("b"), &Options::default()).expect("export dir");
    // The archive's first entry is the manifest; then chunks only.
    let mut tar = TarReader::new(fs::File::open(tmp.join("b.tar")).expect("open"));
    assert_eq!(tar.next_entry(1 << 20).expect("entry").expect("first").0, MANIFEST);
    while let Some((name, _)) = tar.next_entry(2 << 20).expect("entry") {
        assert!(parse_chunk_name(&name).is_some(), "{name}");
    }
    for (src, out) in [("b.tar", "out-tar"), ("b", "out-dir")] {
        let r = Reader::open(&tmp.join(src)).expect("open");
        assert_eq!(r.manifest(), &m);
        let dest = tmp.join(out);
        fs::create_dir_all(&dest).expect("dest");
        let x = r.extract(&dest).expect("extract");
        assert_eq!(x.written_bytes, 2 * CHUNK_SIZE + 1000 + 12, "zeros are not written");
        assert_eq!(fs::read(dest.join("simee/userdata.img")).expect("disk"), data);
        assert_eq!(fs::read(dest.join("config.ini")).expect("config"), b"AvdId=pixel\n");
        assert_eq!(fs::read(dest.join("boot.prop")).expect("boot.prop"), b"");
        // Windows has no allocated size to read, and a copy there isn't sparse.
        #[cfg(unix)]
        {
            let blocks = sim_core::hostfs::allocated_bytes(&fs::metadata(dest.join("simee/userdata.img")).expect("meta"));
            assert!(blocks < data.len() as u64 - 16 * CHUNK_SIZE, "the zero chunks stay holes: {blocks} allocated");
        }
    }
    // Refuses to overwrite.
    assert!(matches!(export(header(), &contents(&disk), &tmp.join("b"), &Options::default()), Err(Error::Exists(_))));
    let _ = fs::remove_dir_all(tmp);
}

/// Writes an archive holding `manifest` and the given chunk entries.
fn archive(path: &Path, manifest: &[u8], chunks: &[(String, Vec<u8>)]) {
    let mut w = TarWriter::new(fs::File::create(path).expect("create"));
    w.entry(MANIFEST, manifest).expect("manifest");
    for (name, data) in chunks {
        w.entry(name, data).expect("chunk");
    }
    w.finish().expect("finish");
}

fn chunks_of(dir: &Path, m: &Manifest) -> Vec<(String, Vec<u8>)> {
    m.chunk_hashes().iter().map(|h| (chunk_name(h), fs::read(dir.join(chunk_name(h))).expect("chunk"))).collect()
}

#[test]
fn damaged_bundles_are_refused() {
    let tmp = scratch("verify");
    let (disk, _) = sample_disk(&tmp);
    let (m, _) = export(header(), &contents(&disk), &tmp.join("b"), &Options::default()).expect("export");
    let chunks = chunks_of(&tmp.join("b"), &m);
    let extract = |name: &str| {
        let dest = tmp.join(format!("out-{name}"));
        fs::create_dir_all(&dest).expect("dest");
        Reader::open(&tmp.join(name)).and_then(|r| r.extract(&dest))
    };

    // A chunk whose data isn't what its name says.
    let mut swapped = chunks.clone();
    swapped[0].1 = compress(b"not the chunk");
    archive(&tmp.join("bad-chunk.tar"), &m.to_json(), &swapped);
    assert!(matches!(extract("bad-chunk.tar"), Err(Error::ChunkHash(_))));

    // A disk whose chunks are fine but whose whole hash isn't.
    let mut wrong = m.clone();
    wrong.disks[0].blake3 = hash(b"else");
    archive(&tmp.join("bad-disk.tar"), &wrong.to_json(), &chunks);
    assert!(matches!(extract("bad-disk.tar"), Err(Error::Hash(p)) if p == "simee/userdata.img"));

    // A missing chunk, an extra one, one twice.
    archive(&tmp.join("missing.tar"), &m.to_json(), &chunks[1..]);
    assert!(matches!(extract("missing.tar"), Err(Error::Missing { count: 1, .. })));
    let mut extra = chunks.clone();
    extra.push((chunk_name(&hash(b"x")), compress(b"x")));
    archive(&tmp.join("extra.tar"), &m.to_json(), &extra);
    assert!(matches!(extract("extra.tar"), Err(Error::Tar(_))));
    let mut twice = chunks.clone();
    twice.push(chunks[0].clone());
    archive(&tmp.join("twice.tar"), &m.to_json(), &twice);
    assert!(matches!(extract("twice.tar"), Err(Error::Tar(_))));

    // Paths that would leave the destination.
    for path in ["../escape.img", "/etc/passwd", "simee/../../x", "./config.ini", "a//b", ""] {
        let mut evil = m.clone();
        evil.disks[0].path = path.into();
        archive(&tmp.join("evil.tar"), &evil.to_json(), &chunks);
        assert!(matches!(extract("evil.tar"), Err(Error::Path(p)) if p == path), "{path}");
    }
    assert!(!tmp.join("escape.img").exists() && !tmp.join("x").exists());
    // An archive whose entries aren't the bundle's.
    archive(&tmp.join("foreign.tar"), &m.to_json(), &[("../../x".into(), b"x".to_vec())]);
    assert!(matches!(extract("foreign.tar"), Err(Error::Tar(_))));

    // A newer major is refused as such, before the rest is read.
    let mut v: serde_json::Value = serde_json::from_slice(&m.to_json()).expect("json");
    v["format"] = 2.into();
    v["disks"] = "something format 2 means".into();
    archive(&tmp.join("v2.tar"), &serde_json::to_vec(&v).expect("json"), &chunks);
    assert!(matches!(extract("v2.tar"), Err(Error::Format(2))));
    assert!(Error::Format(2).to_string().contains("update simee"));
    // A huge manifest is refused by size, and a bulky field isn't built before `format` is read.
    let mut bulky: serde_json::Value = serde_json::from_slice(&m.to_json()).expect("json");
    bulky["format"] = 9.into();
    bulky["padding"] = serde_json::Value::Array(vec![0.into(); 100_000]);
    assert!(matches!(Manifest::parse(&serde_json::to_vec(&bulky).expect("json")), Err(Error::Format(9))));
    archive(&tmp.join("huge.tar"), &vec![b' '; MAX_MANIFEST as usize + 1], &chunks);
    assert!(matches!(Reader::open(&tmp.join("huge.tar")), Err(Error::Tar(_))));

    // A chunk that decompresses past chunk_size (its data is 2 MiB of zeros under a hash of
    // that) is refused, not decompressed into a larger buffer.
    let big = vec![0u8; 2 * CHUNK_SIZE as usize];
    let mut grown = m.clone();
    grown.disks[0].extents[0].hash = Some(hash(&big));
    let mut grown_chunks = chunks.clone();
    grown_chunks.push((chunk_name(&hash(&big)), compress(&big)));
    archive(&tmp.join("grown.tar"), &grown.to_json(), &grown_chunks);
    assert!(matches!(extract("grown.tar"), Err(Error::ChunkHash(h)) if h == hash(&big)));

    // A directory bundle whose manifest or chunk is a link is refused before it is read.
    #[cfg(unix)]
    {
        let linked = tmp.join("linked");
        fs::create_dir_all(linked.join("chunks")).expect("dir");
        std::os::unix::fs::symlink(tmp.join("b/manifest.json"), linked.join(MANIFEST)).expect("symlink");
        assert!(matches!(Reader::open(&linked), Err(Error::Unsupported(_))));
        fs::remove_file(linked.join(MANIFEST)).expect("rm");
        fs::copy(tmp.join("b/manifest.json"), linked.join(MANIFEST)).expect("copy");
        for h in m.chunk_hashes() {
            fs::create_dir_all(linked.join(chunk_name(h)).parent().expect("parent")).expect("dir");
            std::os::unix::fs::symlink("/dev/zero", linked.join(chunk_name(h))).expect("symlink");
        }
        assert!(matches!(extract("linked"), Err(Error::Unsupported(_))));
    }

    // A base delta (#171) is reserved: a format 1 reader refuses it before writing.
    let mut delta = m.clone();
    delta.disks[0].base = Some(Base { kind: "golden".into(), identity: "x".into(), blake3: hash(b"base") });
    delta.disks[0].extents[1] = Extent { offset: CHUNK_SIZE, len: CHUNK_SIZE, hash: None, zero: false, from_base: true };
    archive(&tmp.join("delta.tar"), &delta.to_json(), &chunks);
    assert!(matches!(Reader::open(&tmp.join("delta.tar")), Err(Error::Unsupported(_))));

    // Extents that don't tile the disk.
    let mut gap = m.clone();
    gap.disks[0].extents.remove(1);
    assert!(gap.validate().is_err());
    let mut two = m.clone();
    two.disks[0].extents[0].zero = true;
    assert!(two.validate().is_err(), "a hash and zero at once");
    let _ = fs::remove_dir_all(tmp);
}

#[test]
fn paths_and_hashes_are_checked_by_shape() {
    for ok in ["config.ini", "simee/userdata.img", "a.b/c-d_e", "\u{e9}t\u{e9}/stra\u{df}e"] {
        assert!(safe_path(ok), "{ok}");
    }
    // Ignorable and format code points: a zero-width space, joiner and no-break space, a bidi
    // override, a soft hyphen, a variation selector and a tag.
    let ignorables = ["a\u{200b}b", "a\u{200d}b", "\u{feff}a", "a\u{202e}b", "a\u{ad}b", "a\u{fe0f}", "a\u{e0041}"];
    for bad in ["", "/abs", "../x", "a/../b", "a/./b", ".", "a/", "a\\b", "a\0b"].into_iter().chain(ignorables) {
        assert!(!safe_path(bad), "{bad:?}");
    }
    assert!(safe_target("../a/b") && safe_target("/System/Library") && safe_target(&"a".repeat(crate::MAX_TARGET)));
    for bad in ["", "a\0b", "a\u{200b}"] {
        assert!(!safe_target(bad), "{bad:?}");
    }
    assert!(!safe_target(&"a".repeat(crate::MAX_TARGET + 1)), "longer than macOS makes");
    assert!(is_hash(&hash(b"x")));
    assert!(!is_hash(&hash(b"x").to_uppercase()) && !is_hash("abc"));
}
