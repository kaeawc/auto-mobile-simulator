//! Captured frames and their PNG encoding, shared by every screenshot path (CLI, API, desktop).

/// One captured frame: tightly packed opaque RGBA, top row first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Encodes a frame as an 8-bit PNG, favouring speed over size: screenshots are taken after nearly
/// every automated step. It is RGB when every alpha byte is 0xff (all of the iOS and Android
/// frames: 25% fewer bytes to filter and compress), RGBA otherwise.
pub fn encode_png(frame: &Frame) -> Result<Vec<u8>, String> {
    encode_rgba(frame.width, frame.height, &frame.rgba)
}

/// [`encode_png`] for RGBA rows held elsewhere (a capture shared with the display stream), with no
/// copy into a [`Frame`] first.
///
/// `Fastest` is fdeflate with the `Up` filter alone. `Fast` also filters each row five ways and
/// keeps the smallest, which on a 1206x2622 home screen costs 1.3x the time for a file half the
/// size (`examples/frame_bench.rs`); a screenshot goes to a local file or a socket, so time wins.
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    // One pass drops alpha and ANDs the alpha bytes together (0xff only if all are); a frame with
    // real alpha throws the RGB copy away, which is rare.
    // A zeroed Vec this size comes straight from the allocator's zeroed pages: no fill pass.
    let mut rgb = vec![0u8; rgba.len() / 4 * 3];
    let mut alpha = 0xffu8;
    for (d, p) in rgb.as_chunks_mut::<3>().0.iter_mut().zip(rgba.as_chunks::<4>().0) {
        *d = [p[0], p[1], p[2]];
        alpha &= p[3];
    }
    let (color, data) = if alpha == 0xff { (png::ColorType::Rgb, rgb.as_slice()) } else { (png::ColorType::Rgba, rgba) };
    // Fastest output is about half its input; sized for that, and the Vec grows if it is not.
    let mut out = Vec::with_capacity(data.len() / 2 + 1024);
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fastest);
    encoder.write_header().and_then(|mut w| w.write_image_data(data)).map_err(|e| e.to_string())?;
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
        let (info, pixels) = decode(&png);
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(pixels, [255, 0, 0, 0, 0, 255]);
    }

    fn decode(png: &[u8]) -> (png::OutputInfo, Vec<u8>) {
        let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().expect("header");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("frame");
        buf.truncate(info.buffer_size());
        (info, buf)
    }

    /// Deterministic busy pixels, with `alpha(i)` for pixel `i`.
    fn busy_frame(width: u32, height: u32, alpha: impl Fn(usize) -> u8) -> Frame {
        let rgba = (0..(width * height) as usize)
            .flat_map(|i| {
                let v = i.wrapping_mul(2_654_435_761) >> 7;
                [v as u8, (v >> 8) as u8, (v >> 16) as u8, alpha(i)]
            })
            .collect();
        Frame { width, height, rgba }
    }

    #[test]
    fn an_opaque_frame_round_trips_as_rgb() {
        let frame = busy_frame(61, 37, |_| 0xff);
        let (info, pixels) = decode(&encode_png(&frame).expect("encode"));
        assert_eq!((info.width, info.height, info.color_type), (61, 37, png::ColorType::Rgb));
        let want: Vec<u8> = frame.rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        assert_eq!(pixels, want);
    }

    #[test]
    fn a_frame_with_real_alpha_stays_rgba() {
        let frame = busy_frame(61, 37, |i| if i == 1000 { 0x7f } else { 0xff });
        let (info, pixels) = decode(&encode_png(&frame).expect("encode"));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(pixels, frame.rgba);
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
        assert!(encode_rgba(2, 2, &[0; 4]).is_err());
        assert_eq!(encode_rgba(1, 1, &[1, 2, 3, 4]), encode_png(&Frame { width: 1, height: 1, rgba: vec![1, 2, 3, 4] }));
    }
}
