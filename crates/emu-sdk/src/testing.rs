//! Helpers for tests here and in crates that provision (feature `testing`).

/// A zip of stored (uncompressed) entries, which unzip reads like any other.
pub fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, data) in entries {
        let mut crc = flate2::Crc::new();
        crc.update(data);
        let header = |sig: u32, central: bool, offset: u32| {
            let mut h = sig.to_le_bytes().to_vec();
            if central {
                h.extend(20u16.to_le_bytes()); // made by
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
                h.extend(0o100644u32.wrapping_shl(16).to_le_bytes());
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
