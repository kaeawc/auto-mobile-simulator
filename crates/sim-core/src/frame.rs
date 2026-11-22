//! Captured frames and their PNG encoding, shared by every screenshot path (CLI, API, desktop).

/// One captured frame: tightly packed opaque RGBA, top row first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Encodes a frame as an 8-bit RGBA PNG.
pub fn encode_png(frame: &Frame) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().and_then(|mut w| w.write_image_data(&frame.rgba)).map_err(|e| e.to_string())?;
    Ok(out)
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
    fn rejects_a_short_buffer() {
        assert!(encode_png(&Frame { width: 2, height: 2, rgba: vec![0; 4] }).is_err());
    }
}
