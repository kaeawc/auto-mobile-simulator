//! Times the screenshot path: pixel conversion and PNG encoding, on a real screen (any PNG) and on
//! a synthetic iOS-sized home screen.
//!
//!     cargo run --release -p simee-core --example frame_bench [image.png ...]
use std::time::Instant;

use simee_core::frame::{Frame, encode_png};
use simee_core::pixels::{PixelFormat, downscale, to_rgba};

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    let paths = if paths.is_empty() { vec!["docs/desktop-android.png".to_owned()] } else { paths };
    for path in &paths {
        let file = std::fs::File::open(path).expect("image");
        let mut reader = png::Decoder::new(std::io::BufReader::new(file)).read_info().expect("png");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        let info = reader.next_frame(&mut buf).expect("frame");
        buf.truncate(info.buffer_size());
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => buf.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
            other => panic!("{path}: unsupported color type {other:?}"),
        };
        bench(path, info.width, info.height, rgba, true);
    }
    // iOS 1206x2622, the iPhone 17 Pro's home screen.
    bench("synthetic iOS home screen", 1206, 2622, synthetic_home_screen(1206, 2622), false);
}

/// Gradient wallpaper, a status bar, a 4x6 grid of rounded icon tiles (each a gradient with a
/// glyph of noise), labels of text-like noise under them, and a dock: not flat, not random.
fn synthetic_home_screen(w: usize, h: usize) -> Vec<u8> {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut rand = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 32) as u32
    };
    let mut px = vec![255u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let o = (y * w + x) * 4;
            px[o] = (40 + 120 * x / w) as u8;
            px[o + 1] = (60 + 140 * y / h) as u8;
            px[o + 2] = (200 - 100 * y / h) as u8;
        }
    }
    let mut fill = |px: &mut [u8], x0: usize, y0: usize, tw: usize, th: usize, color: [u8; 3], noise: u32| {
        for y in y0..(y0 + th).min(h) {
            for x in x0..(x0 + tw).min(w) {
                let o = (y * w + x) * 4;
                let (cx, cy) = (x - x0, y - y0);
                let corner = u8::from(cx.min(tw - 1 - cx) < 12 && cy.min(th - 1 - cy) < 12);
                let g = (cy * 60 / th) as u8;
                for c in 0..3 {
                    let n = if noise > 0 && (cx / 3 + cy / 3) % 5 < 2 { (rand() % noise) as u8 } else { 0 };
                    px[o + c] = color[c].saturating_add(g).saturating_add(n).saturating_sub(corner * 20);
                }
            }
        }
    };
    fill(&mut px, 0, 0, w, 120, [20, 20, 30], 40); // status bar
    let (tile, gap) = (200, 60);
    for row in 0..6 {
        for col in 0..4 {
            let (x, y) = (60 + col * (tile + gap), 200 + row * (tile + 110));
            let color = [(row * 37 + col * 91) as u8 % 200, (row * 71 + col * 53) as u8 % 200, (row * 29 + col * 113) as u8 % 200];
            fill(&mut px, x, y, tile, tile, color, 0);
            fill(&mut px, x + 50, y + 50, 100, 100, [230, 230, 230], 120); // glyph
            fill(&mut px, x + 20, y + tile + 20, tile - 40, 28, [250, 250, 250], 200); // label
        }
    }
    fill(&mut px, 40, h - 300, w - 80, 220, [200, 200, 220], 0); // dock
    for col in 0..4 {
        fill(&mut px, 90 + col * (tile + gap), h - 280, tile, 180, [(60 * col) as u8, 120, 200], 90);
    }
    px
}

fn encode(w: u32, h: u32, data: &[u8], color: png::ColorType, c: png::Compression) -> usize {
    let mut out = Vec::new();
    let mut e = png::Encoder::new(&mut out, w, h);
    e.set_color(color);
    e.set_depth(png::BitDepth::Eight);
    e.set_compression(c);
    e.write_header().and_then(|mut wr| wr.write_image_data(data)).expect("png");
    out.len()
}

fn bench(name: &str, w: u32, h: u32, rgba: Vec<u8>, conversions: bool) {
    let time = |name: &str, n: u32, f: &mut dyn FnMut() -> usize| {
        f();
        let start = Instant::now();
        let mut size = 0;
        for _ in 0..n {
            size = f();
        }
        println!("{name:<34} {:>8.2} ms  ({size} bytes)", start.elapsed().as_secs_f64() * 1000.0 / f64::from(n));
    };
    println!("\n{name}: {w}x{h}");
    if conversions {
        let bgra: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
        time("to_rgba (BGRA scanout)", 20, &mut || {
            to_rgba(&bgra, w as usize, h as usize, w as usize * 4, PixelFormat::Bgra).expect("rgba").len()
        });
        for factor in [2, 3] {
            time(&format!("downscale /{factor} (display socket)"), 20, &mut || downscale(w, h, &rgba, factor).2.len());
        }
    }
    let rgb: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    let frame = Frame { width: w, height: h, rgba };
    time("encode_png (shipping)", 10, &mut || encode_png(&frame).expect("png").len());
    for (label, c) in [("Fast", png::Compression::Fast), ("Fastest", png::Compression::Fastest), ("Balanced", png::Compression::Balanced)] {
        time(&format!("{label} RGBA"), 10, &mut || encode(w, h, &frame.rgba, png::ColorType::Rgba, c));
        time(&format!("{label} RGB"), 10, &mut || encode(w, h, &rgb, png::ColorType::Rgb, c));
    }
}
