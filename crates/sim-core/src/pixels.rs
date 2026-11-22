//! Framebuffer pixel conversion.

/// Converts a BGRA framebuffer with `stride` bytes per row into tightly packed opaque RGBA.
/// Returns `None` when `src` is too short for the described geometry.
pub fn bgra_to_rgba(src: &[u8], width: usize, height: usize, stride: usize) -> Option<Vec<u8>> {
    let row_bytes = width.checked_mul(4)?;
    if stride < row_bytes || (height > 0 && src.len() < stride.checked_mul(height - 1)?.checked_add(row_bytes)?) {
        return None;
    }
    let mut out = Vec::with_capacity(row_bytes * height);
    for row in 0..height {
        let line = &src[row * stride..row * stride + row_bytes];
        for px in line.as_chunks::<4>().0 {
            out.extend_from_slice(&[px[2], px[1], px[0], 0xff]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
