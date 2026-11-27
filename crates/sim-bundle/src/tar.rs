//! The single-file form: a plain, uncompressed ustar archive of the bundle directory, read and
//! written entry by entry so neither side holds more than one chunk.

use std::io::{Read, Write};

use crate::{Error, Result};

const BLOCK: usize = 512;

/// Writes regular-file entries, then the two zero blocks that end an archive.
pub struct TarWriter<W: Write> {
    out: W,
    written: u64,
}

impl<W: Write> TarWriter<W> {
    pub fn new(out: W) -> TarWriter<W> {
        TarWriter { out, written: 0 }
    }

    /// Bytes written so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Appends file `name` (at most 100 bytes: the bundle's names are) holding `data`.
    pub fn entry(&mut self, name: &str, data: &[u8]) -> Result<()> {
        if name.len() > 100 {
            return Err(Error::Tar(format!("entry name `{name}` is longer than 100 bytes")));
        }
        let mut h = [0u8; BLOCK];
        h[..name.len()].copy_from_slice(name.as_bytes());
        octal(&mut h[100..108], 0o644);
        octal(&mut h[108..116], 0);
        octal(&mut h[116..124], 0);
        octal(&mut h[124..136], data.len() as u64);
        octal(&mut h[136..148], 0);
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].fill(b' ');
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        let pad = (BLOCK - data.len() % BLOCK) % BLOCK;
        self.out.write_all(&h).map_err(Error::Write)?;
        self.out.write_all(data).map_err(Error::Write)?;
        self.out.write_all(&[0u8; BLOCK][..pad]).map_err(Error::Write)?;
        self.written += (BLOCK + data.len() + pad) as u64;
        Ok(())
    }

    /// Ends the archive and returns the writer.
    pub fn finish(mut self) -> Result<W> {
        self.out.write_all(&[0u8; 2 * BLOCK]).map_err(Error::Write)?;
        self.written += 2 * BLOCK as u64;
        self.out.flush().map_err(Error::Write)?;
        Ok(self.out)
    }
}

/// `n` as zero-padded octal digits filling `field` but its last byte (a NUL).
fn octal(field: &mut [u8], n: u64) {
    let last = field.len() - 1;
    let digits = format!("{n:0last$o}");
    field[..last].copy_from_slice(digits.as_bytes());
    field[last] = 0;
}

fn parse_octal(field: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(field).ok()?;
    let digits = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if digits.is_empty() { Some(0) } else { u64::from_str_radix(digits, 8).ok() }
}

/// Reads an archive's regular files in order, each into memory (at most `max` bytes: a bundle's
/// entries are the manifest and chunks). Directory entries are skipped; any other kind (links,
/// devices, extended headers) is refused.
pub struct TarReader<R: Read> {
    input: R,
    done: bool,
}

impl<R: Read> TarReader<R> {
    pub fn new(input: R) -> TarReader<R> {
        TarReader { input, done: false }
    }

    /// The next file's name and contents, or None at the end of the archive.
    pub fn next_entry(&mut self, max: u64) -> Result<Option<(String, Vec<u8>)>> {
        loop {
            if self.done {
                return Ok(None);
            }
            let mut h = [0u8; BLOCK];
            match read_full(&mut self.input, &mut h)? {
                0 => return Err(Error::Tar("the archive ends without its end blocks".into())),
                BLOCK => {}
                _ => return Err(Error::Tar("the archive ends inside a header".into())),
            }
            if h.iter().all(|&b| b == 0) {
                self.done = true;
                return Ok(None);
            }
            let stored = parse_octal(&h[148..156]).ok_or_else(|| Error::Tar("bad header checksum field".into()))?;
            let sum: u64 = h.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { u64::from(b) }).sum();
            if sum != stored {
                return Err(Error::Tar("header checksum mismatch".into()));
            }
            let field = |r: std::ops::Range<usize>| {
                let f = &h[r];
                String::from_utf8_lossy(&f[..f.iter().position(|&b| b == 0).unwrap_or(f.len())]).into_owned()
            };
            let (name, prefix) = (field(0..100), field(345..500));
            let name = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            let size = parse_octal(&h[124..136]).ok_or_else(|| Error::Tar(format!("{name}: bad size")))?;
            let kind = h[156];
            if kind == b'5' {
                if size != 0 {
                    return Err(Error::Tar(format!("{name}: a directory with data")));
                }
                continue;
            }
            if kind != b'0' && kind != 0 {
                return Err(Error::Tar(format!("{name}: entry type `{}` (only regular files and directories)", kind as char)));
            }
            if size > max {
                return Err(Error::Tar(format!("{name}: {size} bytes is more than {max}")));
            }
            let mut data = vec![0u8; size as usize];
            if read_full(&mut self.input, &mut data)? != data.len() {
                return Err(Error::Tar(format!("{name}: the archive ends inside it")));
            }
            let pad = (BLOCK - data.len() % BLOCK) % BLOCK;
            let mut skip = [0u8; BLOCK];
            if read_full(&mut self.input, &mut skip[..pad])? != pad {
                return Err(Error::Tar(format!("{name}: the archive ends inside its padding")));
            }
            return Ok(Some((name, data)));
        }
    }
}

/// Reads until `buf` is full or the input ends; how many bytes it got.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Read(e)),
        }
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_round_trip_in_order_with_padding() {
        let mut w = TarWriter::new(Vec::new());
        w.entry("manifest.json", b"{}").expect("entry");
        w.entry("chunks/ab/abcd", &[7u8; 1000]).expect("entry");
        w.entry("empty", b"").expect("entry");
        let written = w.written();
        let bytes = w.finish().expect("finish");
        assert_eq!(bytes.len() as u64, written + 2 * BLOCK as u64, "the end blocks come last");
        assert_eq!(bytes.len() % BLOCK, 0);
        let mut r = TarReader::new(bytes.as_slice());
        assert_eq!(r.next_entry(1 << 20).expect("read"), Some(("manifest.json".into(), b"{}".to_vec())));
        assert_eq!(r.next_entry(1 << 20).expect("read"), Some(("chunks/ab/abcd".into(), vec![7u8; 1000])));
        assert_eq!(r.next_entry(1 << 20).expect("read"), Some(("empty".into(), Vec::new())));
        assert_eq!(r.next_entry(1 << 20).expect("read"), None);
    }

    #[test]
    fn damaged_or_foreign_archives_are_refused() {
        let mut w = TarWriter::new(Vec::new());
        w.entry("a", &[1u8; 10]).expect("entry");
        let good = w.finish().expect("finish");
        // A flipped byte in the header fails its checksum.
        let mut bad = good.clone();
        bad[0] = b'b';
        assert!(TarReader::new(bad.as_slice()).next_entry(100).expect_err("checksum").to_string().contains("checksum"));
        // Truncated inside the data, and with no end blocks.
        assert!(TarReader::new(&good[..BLOCK + 5]).next_entry(100).is_err());
        let mut r = TarReader::new(&good[..2 * BLOCK]);
        r.next_entry(100).expect("first").expect("entry");
        assert!(r.next_entry(100).is_err(), "no end blocks");
        // More than the reader allows.
        assert!(TarReader::new(good.as_slice()).next_entry(5).is_err());
        // A symlink entry.
        let mut link = good.clone();
        link[156] = b'2';
        link[148..156].fill(b' ');
        let sum: u32 = link[..BLOCK].iter().map(|&b| u32::from(b)).sum();
        link[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        assert!(TarReader::new(link.as_slice()).next_entry(100).expect_err("link").to_string().contains("entry type"));
    }
}
