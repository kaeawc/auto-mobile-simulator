//! Just enough Mach-O to list a dylib's load-time dependencies (`LC_LOAD_DYLIB` and friends), so
//! the renderer's bundled libraries can be preloaded in dependency order without relying on the
//! executable's rpath.
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MH_MAGIC_64: u32 = 0xfeed_facf;
const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const MACH_HEADER_64: usize = 32;

const LC_REQ_DYLD: u32 = 0x8000_0000;
const LC_LOAD_DYLIB: u32 = 0xc;
const LC_LOAD_WEAK_DYLIB: u32 = 0x18 | LC_REQ_DYLD;
const LC_REEXPORT_DYLIB: u32 = 0x1f | LC_REQ_DYLD;
const LC_LAZY_LOAD_DYLIB: u32 = 0x20;
const LC_LOAD_UPWARD_DYLIB: u32 = 0x23 | LC_REQ_DYLD;

fn u32_at(b: &[u8], off: usize, big_endian: bool) -> Option<u32> {
    let bytes: [u8; 4] = b.get(off..off + 4)?.try_into().ok()?;
    Some(if big_endian { u32::from_be_bytes(bytes) } else { u32::from_le_bytes(bytes) })
}

fn u64_be_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Offset of the arm64 slice in a universal binary, `Some(0)` for a thin 64-bit Mach-O, `None`
/// for anything else. `head` is the start of the file (the fat header and its arch table).
fn arm64_slice(head: &[u8]) -> Option<u64> {
    match u32_at(head, 0, true)? {
        FAT_MAGIC | FAT_MAGIC_64 => {
            let wide = u32_at(head, 0, true)? == FAT_MAGIC_64;
            let n = u32_at(head, 4, true)? as usize;
            let entry = if wide { 32 } else { 20 };
            (0..n).find_map(|i| {
                let e = 8 + i * entry;
                (u32_at(head, e, true)? == CPU_TYPE_ARM64)
                    .then(|| if wide { u64_be_at(head, e + 8) } else { u32_at(head, e + 8, true).map(u64::from) })
                    .flatten()
            })
        }
        _ => (u32_at(head, 0, false)? == MH_MAGIC_64).then_some(0),
    }
}

/// The dylib paths named by a 64-bit Mach-O header plus its load commands.
pub fn parse_deps(image: &[u8]) -> Option<Vec<String>> {
    if u32_at(image, 0, false)? != MH_MAGIC_64 {
        return None;
    }
    let ncmds = u32_at(image, 16, false)? as usize;
    let mut off = MACH_HEADER_64;
    let mut deps = Vec::new();
    for _ in 0..ncmds {
        let cmd = u32_at(image, off, false)?;
        let size = u32_at(image, off + 4, false)? as usize;
        if size < 8 {
            return None;
        }
        if matches!(cmd, LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | LC_REEXPORT_DYLIB | LC_LAZY_LOAD_DYLIB | LC_LOAD_UPWARD_DYLIB) {
            let name_off = u32_at(image, off + 8, false)? as usize;
            let raw = image.get(off + name_off..off + size)?;
            let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
            deps.push(String::from_utf8_lossy(&raw[..end]).into_owned());
        }
        off += size;
    }
    Some(deps)
}

/// The dylibs `path` links against, as written in its load commands (`@rpath/libfoo.dylib`,
/// `/usr/lib/libSystem.B.dylib`, ...). Reads only the headers. Empty if it isn't an arm64 Mach-O.
pub fn dylib_deps(path: &Path) -> std::io::Result<Vec<String>> {
    let mut f = std::fs::File::open(path)?;
    let mut head = vec![0u8; 4096];
    let n = read_up_to(&mut f, &mut head)?;
    head.truncate(n);
    let Some(slice) = arm64_slice(&head) else { return Ok(Vec::new()) };
    f.seek(SeekFrom::Start(slice))?;
    let mut header = [0u8; MACH_HEADER_64];
    f.read_exact(&mut header)?;
    let Some(cmds_len) = u32_at(&header, 20, false) else { return Ok(Vec::new()) };
    let mut image = header.to_vec();
    image.resize(MACH_HEADER_64 + cmds_len as usize, 0);
    f.read_exact(&mut image[MACH_HEADER_64..])?;
    Ok(parse_deps(&image).unwrap_or_default())
}

fn read_up_to(f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal thin arm64 dylib image: header, `LC_ID_DYLIB` for itself, then one load command
    /// per dependency (the first one weak).
    pub(crate) fn fake_dylib(id: &str, deps: &[&str]) -> Vec<u8> {
        let cmd = |kind: u32, name: &str| {
            let mut c = Vec::new();
            let size = (24 + name.len() + 1).next_multiple_of(8);
            for v in [kind, size as u32, 24, 0, 0, 0] {
                c.extend_from_slice(&v.to_le_bytes());
            }
            c.extend_from_slice(name.as_bytes());
            c.resize(size, 0);
            c
        };
        let mut cmds = cmd(0xd, id);
        for (i, d) in deps.iter().enumerate() {
            cmds.extend(cmd(if i == 0 { LC_LOAD_WEAK_DYLIB } else { LC_LOAD_DYLIB }, d));
        }
        let mut out = Vec::new();
        for v in [MH_MAGIC_64, CPU_TYPE_ARM64, 0, 6, deps.len() as u32 + 1, cmds.len() as u32, 0, 0] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend(cmds);
        out
    }

    #[test]
    fn parses_thin_and_fat_images() {
        let thin = fake_dylib("@rpath/liba.dylib", &["@rpath/libb.dylib", "/usr/lib/libSystem.B.dylib"]);
        assert_eq!(parse_deps(&thin).expect("parse"), ["@rpath/libb.dylib", "/usr/lib/libSystem.B.dylib"]);
        assert!(parse_deps(b"not a mach-o image").is_none());
        // A truncated command table is rejected rather than read past.
        assert!(parse_deps(&thin[..thin.len() - 8]).is_none());

        // Universal binary: an x86_64 slice first, then arm64 at offset 4096.
        let mut fat = Vec::new();
        for v in [FAT_MAGIC, 2, 0x0100_0007, 3, 64, 16, 12, CPU_TYPE_ARM64, 0, 4096, thin.len() as u32, 12] {
            fat.extend_from_slice(&v.to_be_bytes());
        }
        assert_eq!(arm64_slice(&fat), Some(4096));
        fat.resize(4096, 0);
        fat.extend_from_slice(&thin);
        let dir = std::env::temp_dir().join(format!("emu-gfxstream-macho-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("fat.dylib");
        std::fs::write(&path, &fat).expect("write");
        assert_eq!(dylib_deps(&path).expect("deps"), ["@rpath/libb.dylib", "/usr/lib/libSystem.B.dylib"]);
        std::fs::write(&path, b"#!/bin/sh\n").expect("write");
        assert!(dylib_deps(&path).expect("not mach-o").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reads_a_real_binary() {
        // The test binary itself links libSystem.
        let exe = std::env::current_exe().expect("exe");
        let deps = dylib_deps(&exe).expect("deps");
        assert!(deps.iter().any(|d| d == "/usr/lib/libSystem.B.dylib"), "{deps:?}");
    }
}
