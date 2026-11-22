//! Loading the kernel image (raw or gzip-compressed arm64 `Image`) into guest RAM.
use std::io::Read;

use anyhow::{Context, Result, bail};

const ARM64_MAGIC: u32 = 0x644d_5241; // "ARM\x64" at offset 0x38

pub struct Kernel {
    pub image: Vec<u8>,
    pub text_offset: u64,
    pub image_size: u64,
}

pub fn parse_kernel(raw: Vec<u8>) -> Result<Kernel> {
    let image = if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::with_capacity(raw.len() * 3);
        flate2::read::GzDecoder::new(raw.as_slice()).read_to_end(&mut out).context("decompressing kernel")?;
        out
    } else {
        raw
    };
    if image.len() < 64 {
        bail!("kernel image too small ({} bytes)", image.len());
    }
    let word = |o: usize| u64::from_le_bytes(image[o..o + 8].try_into().unwrap_or_default());
    let magic = u32::from_le_bytes(image[0x38..0x3c].try_into().unwrap_or_default());
    if magic != ARM64_MAGIC {
        bail!("not an arm64 kernel Image (magic {magic:#x})");
    }
    let text_offset = word(8);
    // image_size 0 means "unknown" on old kernels: assume the file plus generous BSS.
    let image_size = match word(16) {
        0 => image.len() as u64 + (16 << 20),
        n => n,
    };
    Ok(Kernel { image, text_offset, image_size })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fake_image() -> Vec<u8> {
        let mut img = vec![0u8; 128];
        img[16..24].copy_from_slice(&0x20_0000u64.to_le_bytes());
        img[0x38..0x3c].copy_from_slice(&ARM64_MAGIC.to_le_bytes());
        img
    }

    #[test]
    fn parses_raw_and_gzip() {
        let k = parse_kernel(fake_image()).expect("raw");
        assert_eq!((k.text_offset, k.image_size), (0, 0x20_0000));
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&fake_image()).expect("gz");
        let k = parse_kernel(gz.finish().expect("gz")).expect("gzip");
        assert_eq!(k.image.len(), 128);
        assert!(parse_kernel(vec![0u8; 128]).is_err());
    }
}
