//! Minimal PNG writer for screenshots (8-bit RGBA, no filtering).
use std::io::Write;

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
}

/// Encodes `rgba` (`width * height * 4` bytes, top row first).
pub fn encode(width: u32, height: u32, rgba: &[u8]) -> std::io::Result<Vec<u8>> {
    let row = width as usize * 4;
    let mut z = flate2::write::ZlibEncoder::new(Vec::with_capacity(rgba.len() / 2), flate2::Compression::fast());
    for line in rgba.chunks(row).take(height as usize) {
        z.write_all(&[0])?; // filter: none
        z.write_all(line)?;
    }
    let idat = z.finish()?;
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, no filter, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

/// Converts a virtio-gpu scanout frame to tightly packed RGBA.
pub fn to_rgba(width: u32, height: u32, stride: u32, format: u32, src: &[u8]) -> Vec<u8> {
    // virtio-gpu formats: 1 B8G8R8A8, 2 B8G8R8X8, 3 A8R8G8B8, 4 X8R8G8B8,
    // 67 R8G8B8A8, 68 X8B8G8R8, 121 A8B8G8R8, 134 R8G8B8X8 (names list bytes in memory order).
    let mut out = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        let row = &src[(y * stride as usize).min(src.len())..];
        for px in row.as_chunks::<4>().0.iter().take(width as usize) {
            let (r, g, b, a) = match format {
                1 => (px[2], px[1], px[0], px[3]),
                2 => (px[2], px[1], px[0], 255),
                3 => (px[1], px[2], px[3], px[0]),
                4 => (px[1], px[2], px[3], 255),
                68 => (px[3], px[2], px[1], 255),
                121 => (px[3], px[2], px[1], px[0]),
                134 => (px[0], px[1], px[2], 255),
                _ => (px[0], px[1], px[2], px[3]),
            };
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_valid_png_header_and_converts_bgra() {
        let png = encode(2, 1, &[255, 0, 0, 255, 0, 255, 0, 255]).expect("encode");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(&png[12..16], b"IHDR");
        assert!(png.ends_with(&[0xae, 0x42, 0x60, 0x82]), "IEND crc");
        assert_eq!(to_rgba(1, 1, 4, 1, &[1, 2, 3, 4]), vec![3, 2, 1, 4]);
        assert_eq!(to_rgba(1, 1, 4, 67, &[1, 2, 3, 4]), vec![1, 2, 3, 4]);
    }
}
