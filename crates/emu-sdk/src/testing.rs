//! Helpers for tests here and in crates that provision (feature `testing`).

/// A zip of stored (uncompressed) entries, which unzip reads like any other.
pub fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let entries: Vec<_> = entries.iter().map(|(n, d)| (*n, *d, 0o100644)).collect();
    stored_zip_modes(&entries)
}

/// [`stored_zip`] with each entry's Unix mode (`0o100644` a file, `0o120777` a symlink whose data is its target).
pub fn stored_zip_modes(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, data, mode) in entries {
        let mut crc = flate2::Crc::new();
        crc.update(data);
        let header = |sig: u32, central: bool, offset: u32| {
            let mut h = sig.to_le_bytes().to_vec();
            if central {
                h.extend(0x0314u16.to_le_bytes()); // made by: Unix, so the mode below counts
            }
            for v in [20u16, 0, 0, 0, 0x21] {
                h.extend(v.to_le_bytes()); // version, flags, stored, time, date
            }
            for v in [crc.sum(), data.len() as u32, data.len() as u32] {
                h.extend(v.to_le_bytes());
            }
            h.extend((name.len() as u16).to_le_bytes());
            h.extend(0u16.to_le_bytes()); // extra
            if central {
                for v in [0u16, 0, 0] {
                    h.extend(v.to_le_bytes()); // comment, disk, internal attributes
                }
                h.extend(mode.wrapping_shl(16).to_le_bytes());
                h.extend(offset.to_le_bytes());
            }
            h.extend(name.as_bytes());
            h
        };
        central.extend(header(0x0201_4b50, true, out.len() as u32));
        out.extend(header(0x0403_4b50, false, 0));
        out.extend(*data);
    }
    let start = out.len() as u32;
    out.extend(&central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    for v in [0u16, 0, entries.len() as u16, entries.len() as u16] {
        out.extend(v.to_le_bytes());
    }
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(start.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}
