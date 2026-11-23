//! Framebuffer pixel conversion, shared by the iOS framebuffer (BGRA `IOSurface`) and the Android
//! scanout (virtio-gpu formats).

/// The byte order of a 32-bit pixel in memory. `X` bytes are padding: the output is opaque.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra,
    Bgrx,
    Argb,
    Xrgb,
    Rgba,
    Rgbx,
    Abgr,
    Xbgr,
}

impl PixelFormat {
    /// A virtio-gpu format code (`VIRTIO_GPU_FORMAT_*`, whose names list bytes in memory order).
    pub fn from_virtio_gpu(code: u32) -> Option<Self> {
        Some(match code {
            1 => Self::Bgra,
            2 => Self::Bgrx,
            3 => Self::Argb,
            4 => Self::Xrgb,
            67 => Self::Rgba,
            68 => Self::Xbgr,
            121 => Self::Abgr,
            134 => Self::Rgbx,
            _ => return None,
        })
    }
}

/// Converts a framebuffer with `stride` bytes per row into tightly packed RGBA. Returns `None`
/// when `src` is too short for the described geometry.
pub fn to_rgba(src: &[u8], width: usize, height: usize, stride: usize, format: PixelFormat) -> Option<Vec<u8>> {
    let row_bytes = width.checked_mul(4)?;
    if stride < row_bytes || (height > 0 && src.len() < stride.checked_mul(height - 1)?.checked_add(row_bytes)?) {
        return None;
    }
    let mut out = vec![0u8; row_bytes * height];
    // One loop per format, so the byte shuffle is a fixed permutation the compiler vectorizes.
    let convert: fn(&[u8], &mut [u8]) = match format {
        PixelFormat::Rgba => |src, dst| dst.copy_from_slice(src),
        PixelFormat::Bgra => |src, dst| shuffle(src, dst, |p| [p[2], p[1], p[0], p[3]]),
        PixelFormat::Bgrx => |src, dst| shuffle(src, dst, |p| [p[2], p[1], p[0], 0xff]),
        PixelFormat::Argb => |src, dst| shuffle(src, dst, |p| [p[1], p[2], p[3], p[0]]),
        PixelFormat::Xrgb => |src, dst| shuffle(src, dst, |p| [p[1], p[2], p[3], 0xff]),
        PixelFormat::Rgbx => |src, dst| shuffle(src, dst, |p| [p[0], p[1], p[2], 0xff]),
        PixelFormat::Abgr => |src, dst| shuffle(src, dst, |p| [p[3], p[2], p[1], p[0]]),
        PixelFormat::Xbgr => |src, dst| shuffle(src, dst, |p| [p[3], p[2], p[1], 0xff]),
    };
    for (row, dst) in out.chunks_exact_mut(row_bytes.max(1)).enumerate().take(height) {
        convert(&src[row * stride..row * stride + row_bytes], dst);
    }
    Some(out)
}

#[inline(always)]
fn shuffle(src: &[u8], dst: &mut [u8], f: impl Fn(&[u8; 4]) -> [u8; 4]) {
    for (s, d) in src.as_chunks::<4>().0.iter().zip(dst.as_chunks_mut::<4>().0) {
        *d = f(s);
    }
}

/// Converts a BGRA framebuffer into tightly packed opaque RGBA (the iOS framebuffer's alpha byte
/// is not meaningful).
pub fn bgra_to_rgba(src: &[u8], width: usize, height: usize, stride: usize) -> Option<Vec<u8>> {
    to_rgba(src, width, height, stride, PixelFormat::Bgrx)
}

/// Shrinks tightly packed RGBA by a whole `factor`, averaging each `factor` x `factor` block.
/// Factors of 0 and 1 return the image unchanged.
pub fn downscale(width: u32, height: u32, rgba: &[u8], factor: u32) -> (u32, u32, Vec<u8>) {
    if factor <= 1 {
        return (width, height, rgba.to_vec());
    }
    let out = match factor {
        2 => box_filter::<2>(width, height, rgba),
        3 => box_filter::<3>(width, height, rgba),
        4 => box_filter::<4>(width, height, rgba),
        f => box_filter_any(width, height, rgba, f as usize),
    };
    (width / factor, height / factor, out)
}

/// `downscale` for a factor known at compile time (the display socket's usual 2–4), so each
/// block's loops unroll.
fn box_filter<const F: usize>(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let (ow, oh, in_row) = (width as usize / F, height as usize / F, width as usize * 4);
    let mut out = Vec::with_capacity(ow * oh * 4);
    let div = Reciprocal::new((F * F) as u32);
    for oy in 0..oh {
        let rows: [&[u8]; F] = std::array::from_fn(|dy| &rgba[(oy * F + dy) * in_row..][..ow * F * 4]);
        for ox in 0..ow {
            let mut acc = [0u32; 4];
            for row in rows {
                for px in row[ox * F * 4..][..F * 4].as_chunks::<4>().0 {
                    for c in 0..4 {
                        acc[c] += u32::from(px[c]);
                    }
                }
            }
            out.extend(acc.map(|v| div.of(v)));
        }
    }
    out
}

fn box_filter_any(width: u32, height: u32, rgba: &[u8], f: usize) -> Vec<u8> {
    let (ow, oh, in_row) = (width as usize / f, height as usize / f, width as usize * 4);
    let mut out = Vec::with_capacity(ow * oh * 4);
    let div = Reciprocal::new((f * f) as u32);
    for oy in 0..oh {
        for ox in 0..ow {
            let mut acc = [0u32; 4];
            for dy in 0..f {
                for px in rgba[(oy * f + dy) * in_row + ox * f * 4..][..f * 4].as_chunks::<4>().0 {
                    for c in 0..4 {
                        acc[c] += u32::from(px[c]);
                    }
                }
            }
            out.extend(acc.map(|v| div.of(v)));
        }
    }
    out
}

/// Division by a fixed `n` as a multiply and shift: exact (`v / n`, rounded down) for the sums
/// a box filter produces (`v <= 255 * n`, `n < 2^16`).
#[derive(Clone, Copy)]
struct Reciprocal(u64);

impl Reciprocal {
    fn new(n: u32) -> Reciprocal {
        Reciprocal((1u64 << 32).div_ceil(u64::from(n)))
    }

    fn of(self, v: u32) -> u8 {
        ((u64::from(v) * self.0) >> 32) as u8
    }
}

/// The smallest whole factor that brings `width` down to at most `max_width` (at least 1).
pub fn downscale_factor(width: u32, max_width: u32) -> u32 {
    width.div_ceil(max_width.max(1)).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The box filter `downscale` replaced, kept as the reference.
    fn naive(width: u32, height: u32, rgba: &[u8], factor: u32) -> Vec<u8> {
        let (ow, oh) = (width / factor, height / factor);
        let mut out = Vec::new();
        for oy in 0..oh {
            for ox in 0..ow {
                let mut acc = [0u32; 4];
                for dy in 0..factor {
                    let row = ((oy * factor + dy) * width + ox * factor) as usize * 4;
                    for dx in 0..factor as usize {
                        for (c, a) in acc.iter_mut().enumerate() {
                            *a += u32::from(rgba[row + dx * 4 + c]);
                        }
                    }
                }
                out.extend(acc.map(|v| (v / (factor * factor)) as u8));
            }
        }
        out
    }

    #[test]
    fn downscale_matches_the_reference_box_filter_for_every_factor() {
        let (w, h) = (37u32, 29u32); // not multiples: the remainder rows and columns are dropped
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for factor in 2..=7 {
            let (ow, oh, out) = downscale(w, h, &rgba, factor);
            assert_eq!((ow, oh), (w / factor, h / factor));
            assert_eq!(out, naive(w, h, &rgba, factor), "factor {factor}");
        }
    }

    #[test]
    fn reciprocal_division_is_exact_for_box_sums() {
        for n in 1..=64u32 {
            let r = Reciprocal::new(n);
            for v in 0..=255 * n {
                assert_eq!(u32::from(r.of(v)), v / n, "{v} / {n}");
            }
        }
    }

    #[test]
    fn swaps_channels_and_drops_padding() {
        // 1x2 image, stride 8 (4 bytes of padding per row).
        let src = [1, 2, 3, 0, 9, 9, 9, 9, 4, 5, 6, 0];
        assert_eq!(bgra_to_rgba(&src, 1, 2, 8), Some(vec![3, 2, 1, 255, 6, 5, 4, 255]));
    }

    #[test]
    fn rejects_short_buffers() {
        assert_eq!(bgra_to_rgba(&[0; 7], 2, 1, 8), None);
        assert_eq!(bgra_to_rgba(&[0; 8], 2, 1, 4), None);
        assert_eq!(bgra_to_rgba(&[], 0, 0, 0), Some(vec![]));
    }

    #[test]
    fn converts_every_virtio_gpu_format() {
        let px = [1, 2, 3, 4];
        let cases = [
            (1, [3, 2, 1, 4]),
            (2, [3, 2, 1, 255]),
            (3, [2, 3, 4, 1]),
            (4, [2, 3, 4, 255]),
            (67, [1, 2, 3, 4]),
            (68, [4, 3, 2, 255]),
            (121, [4, 3, 2, 1]),
            (134, [1, 2, 3, 255]),
        ];
        for (code, rgba) in cases {
            let format = PixelFormat::from_virtio_gpu(code).expect("known format");
            assert_eq!(to_rgba(&px, 1, 1, 4, format), Some(rgba.to_vec()), "format {code}");
        }
        assert_eq!(PixelFormat::from_virtio_gpu(999), None);
    }

    #[test]
    fn downscale_averages_blocks() {
        // 2x2 image: one white pixel, three black; factor 2 → one grey pixel.
        let px = [255, 255, 255, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255];
        assert_eq!(downscale(2, 2, &px, 2), (1, 1, vec![63, 63, 63, 255]));
        assert_eq!(downscale(2, 2, &px, 1).2, px.to_vec());
        assert_eq!((downscale_factor(1080, 720), downscale_factor(720, 720), downscale_factor(720, 0)), (2, 1, 720));
    }
}
