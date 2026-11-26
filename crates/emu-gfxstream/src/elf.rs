//! Just enough ELF to list a shared library's load-time dependencies (`DT_NEEDED`), so the
//! renderer's bundled libraries can be preloaded in dependency order: the SDK's Linux
//! `libgfxstream_backend.so` has no `RUNPATH`, so the dynamic loader can't find them by itself.
//! Read through the program headers (`PT_DYNAMIC`, and the `PT_LOAD` segment that maps its string
//! table), which a stripped library keeps.
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_STRSZ: u64 = 10;
/// The most program headers and dynamic entries read; real libraries have tens.
const MAX_ENTRIES: usize = 4096;

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

struct Segment {
    kind: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
}

/// Reads `len` bytes at `off`.
fn read_at(f: &mut (impl Read + Seek), off: u64, len: usize) -> Option<Vec<u8>> {
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut buf = vec![0; len];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// The `DT_NEEDED` names of a 64-bit little-endian ELF file, in order; `None` if it isn't one or
/// is malformed. A file without a dynamic section has no dependencies (`Some(vec![])`).
pub fn parse_needed(f: &mut (impl Read + Seek)) -> Option<Vec<String>> {
    let head = read_at(f, 0, 64)?;
    // \x7fELF, ELFCLASS64, ELFDATA2LSB.
    if head[..4] != *b"\x7fELF" || head[4] != 2 || head[5] != 1 {
        return None;
    }
    let (phoff, phentsize, phnum) = (u64_at(&head, 0x20)?, usize::from(u16_at(&head, 0x36)?), usize::from(u16_at(&head, 0x38)?));
    if phentsize < 56 || phnum > MAX_ENTRIES {
        return None;
    }
    let table = read_at(f, phoff, phentsize * phnum)?;
    let segments: Vec<Segment> = (0..phnum)
        .map(|i| {
            let e = i * phentsize;
            Some(Segment {
                kind: u32_at(&table, e)?,
                offset: u64_at(&table, e + 8)?,
                vaddr: u64_at(&table, e + 16)?,
                filesz: u64_at(&table, e + 32)?,
            })
        })
        .collect::<Option<_>>()?;
    let Some(dynamic) = segments.iter().find(|s| s.kind == PT_DYNAMIC) else { return Some(Vec::new()) };
    let n = usize::try_from(dynamic.filesz / 16).ok()?.min(MAX_ENTRIES);
    let entries = read_at(f, dynamic.offset, n * 16)?;
    let (mut needed, mut strtab, mut strsz) = (Vec::new(), None, None);
    for i in 0..n {
        let (tag, val) = (u64_at(&entries, i * 16)?, u64_at(&entries, i * 16 + 8)?);
        match tag {
            DT_NULL => break,
            DT_NEEDED => needed.push(val),
            DT_STRTAB => strtab = Some(val),
            DT_STRSZ => strsz = Some(val),
            _ => {}
        }
    }
    if needed.is_empty() {
        return Some(Vec::new());
    }
    // DT_STRTAB is a virtual address: find the file offset of the loaded segment holding it.
    let (strtab, strsz) = (strtab?, strsz?);
    let seg = segments.iter().find(|s| s.kind == PT_LOAD && s.vaddr <= strtab && strtab + strsz <= s.vaddr + s.filesz)?;
    let strings = read_at(f, seg.offset + (strtab - seg.vaddr), usize::try_from(strsz).ok()?)?;
    needed
        .into_iter()
        .map(|off| {
            let tail = strings.get(usize::try_from(off).ok()?..)?;
            let end = tail.iter().position(|&b| b == 0)?;
            String::from_utf8(tail[..end].to_vec()).ok()
        })
        .collect()
}

/// The `DT_NEEDED` names of the library at `path`.
pub fn needed(path: &Path) -> std::io::Result<Vec<String>> {
    let mut f = std::fs::File::open(path)?;
    parse_needed(&mut f)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: not a 64-bit ELF", path.display())))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal ELF64 shared library naming `deps` in `DT_NEEDED`: one `PT_LOAD` covering the
    /// file at a non-zero base address (so a string table address isn't its file offset) and one
    /// `PT_DYNAMIC`.
    pub fn fake_so(deps: &[&str]) -> Vec<u8> {
        const BASE: u64 = 0x40_0000;
        let mut strings = vec![0u8];
        let offs: Vec<u64> = deps
            .iter()
            .map(|d| {
                let o = strings.len() as u64;
                strings.extend_from_slice(d.as_bytes());
                strings.push(0);
                o
            })
            .collect();
        let phoff = 64u64;
        let strtab = phoff + 2 * 56;
        let dynamic = (strtab + strings.len() as u64).next_multiple_of(8);
        let mut dyns: Vec<(u64, u64)> = offs.iter().map(|&o| (DT_NEEDED, o)).collect();
        dyns.extend([(DT_STRTAB, BASE + strtab), (DT_STRSZ, strings.len() as u64), (DT_NULL, 0)]);
        let size = dynamic + dyns.len() as u64 * 16;
        let mut f = vec![0u8; size as usize];
        f[..4].copy_from_slice(b"\x7fELF");
        f[4] = 2;
        f[5] = 1;
        f[0x20..0x28].copy_from_slice(&phoff.to_le_bytes());
        f[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        f[0x38..0x3a].copy_from_slice(&2u16.to_le_bytes());
        let mut ph = |i: usize, kind: u32, offset: u64, vaddr: u64, len: u64| {
            let e = phoff as usize + i * 56;
            f[e..e + 4].copy_from_slice(&kind.to_le_bytes());
            f[e + 8..e + 16].copy_from_slice(&offset.to_le_bytes());
            f[e + 16..e + 24].copy_from_slice(&vaddr.to_le_bytes());
            f[e + 32..e + 40].copy_from_slice(&len.to_le_bytes());
        };
        ph(0, PT_LOAD, 0, BASE, size);
        ph(1, PT_DYNAMIC, dynamic, BASE + dynamic, size - dynamic);
        f[strtab as usize..strtab as usize + strings.len()].copy_from_slice(&strings);
        for (i, (tag, val)) in dyns.iter().enumerate() {
            let e = dynamic as usize + i * 16;
            f[e..e + 8].copy_from_slice(&tag.to_le_bytes());
            f[e + 8..e + 16].copy_from_slice(&val.to_le_bytes());
        }
        f
    }

    #[test]
    fn needed_names_come_from_the_dynamic_string_table() {
        let so = fake_so(&["libemugl_common.so", "libc++.so", "libc.so.6"]);
        assert_eq!(parse_needed(&mut std::io::Cursor::new(so)).expect("parse"), ["libemugl_common.so", "libc++.so", "libc.so.6"]);
        assert_eq!(parse_needed(&mut std::io::Cursor::new(fake_so(&[]))).expect("parse"), Vec::<String>::new());
    }

    #[test]
    fn not_an_elf_or_cut_short_is_none() {
        assert!(
            parse_needed(&mut std::io::Cursor::new(
                b"\xcf\xfa\xed\xfe not elf, but long enough to have a header....................".to_vec()
            ))
            .is_none()
        );
        let so = fake_so(&["liba.so"]);
        assert!(parse_needed(&mut std::io::Cursor::new(so[..so.len() - 20].to_vec())).is_none(), "dynamic section cut off");
    }

    /// The test binary itself is a real (linked, possibly stripped) ELF on Linux.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_executable_needs_libc() {
        let names = needed(&std::env::current_exe().expect("exe")).expect("parse");
        assert!(names.iter().any(|n| n.starts_with("libc.so")), "{names:?}");
    }
}
