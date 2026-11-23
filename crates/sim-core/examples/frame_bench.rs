//! Times the screenshot path on a real screen: pixel conversion and PNG encoding.
//!
//!     cargo run --release -p sim-core --example frame_bench [image.png]
use std::time::Instant;

use sim_core::frame::{Frame, encode_png};
use sim_core::pixels::{PixelFormat, to_rgba};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "docs/desktop-android.png".into());
    let file = std::fs::File::open(&path).expect("image");
    let mut reader = png::Decoder::new(std::io::BufReader::new(file)).read_info().expect("png");
    let mut rgba = vec![0; reader.output_buffer_size().expect("size")];
    let info = reader.next_frame(&mut rgba).expect("frame");
    let (w, h) = (info.width, info.height);
    rgba.truncate(info.buffer_size());
    // As a guest scanout: BGRA.
    let bgra: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    let time = |name: &str, n: u32, f: &mut dyn FnMut() -> usize| {
        let start = Instant::now();
        let mut size = 0;
        for _ in 0..n {
            size = f();
        }
        println!("{name:<28} {:>8.2} ms  ({size} bytes)", start.elapsed().as_secs_f64() * 1000.0 / f64::from(n));
    };
    println!("{path}: {w}x{h}");
    time("to_rgba (BGRA scanout)", 20, &mut || {
        to_rgba(&bgra, w as usize, h as usize, w as usize * 4, PixelFormat::Bgra).expect("rgba").len()
    });
    let frame = Frame { width: w, height: h, rgba };
    time("encode_png", 10, &mut || encode_png(&frame).expect("png").len());
    for (name, c) in [
        ("Fastest", png::Compression::Fastest),
        ("Fast", png::Compression::Fast),
        ("Balanced", png::Compression::Balanced),
        ("NoCompression", png::Compression::NoCompression),
    ] {
        time(&format!("png {name}"), 10, &mut || {
            let mut out = Vec::new();
            let mut e = png::Encoder::new(&mut out, w, h);
            e.set_color(png::ColorType::Rgba);
            e.set_depth(png::BitDepth::Eight);
            e.set_compression(c);
            e.write_header().and_then(|mut wr| wr.write_image_data(&frame.rgba)).expect("png");
            out.len()
        });
    }
}
