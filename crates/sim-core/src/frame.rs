//! Captured frames and their PNG encoding, shared by every screenshot path (CLI, API, desktop).

/// One captured frame: tightly packed opaque RGBA, top row first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Encodes a frame as an 8-bit RGBA PNG, favouring speed over size: screenshots are taken after
/// nearly every automated step. `Fast` (fdeflate) encodes a 2480x1740 screen in 11 ms against
/// `Balanced`'s 74, for a file 1.7x larger (`examples/frame_bench.rs`).
pub fn encode_png(frame: &Frame) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    encoder.write_header().and_then(|mut w| w.write_image_data(&frame.rgba)).map_err(|e| e.to_string())?;
    Ok(out)
}

/// A PNG's width and height, from its header, without decoding it.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || !png.starts_with(b"\x89PNG\r\n\x1a\n") || &png[12..16] != b"IHDR" {
        return None;
    }
    let be = |o: usize| u32::from_be_bytes([png[o], png[o + 1], png[o + 2], png[o + 3]]);
    Some((be(16), be(20)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_decodable_png() {
        let frame = Frame { width: 2, height: 1, rgba: vec![255, 0, 0, 255, 0, 0, 255, 255] };
        let png = encode_png(&frame).expect("encode");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().expect("header");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("frame");
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(&buf[..info.buffer_size()], frame.rgba.as_slice());
    }

    #[test]
    fn reads_the_size_from_the_header() {
        let png = encode_png(&Frame { width: 3, height: 2, rgba: vec![0; 24] }).expect("encode");
        assert_eq!(png_size(&png), Some((3, 2)));
        assert_eq!(png_size(&png[..20]), None);
        assert_eq!(png_size(b"GIF89a................................"), None);
    }

    #[test]
    fn rejects_a_short_buffer() {
        assert!(encode_png(&Frame { width: 2, height: 2, rgba: vec![0; 4] }).is_err());
    }
}
