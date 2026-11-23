//! Just enough Mach-O to list a dylib's load-time dependencies (`LC_LOAD_DYLIB` and friends), so
//! the renderer's bundled libraries can be preloaded in dependency order without relying on the
//! executable's rpath; to find symbols `dlsym` can't (private ones, from the symbol table); and to
//! recognize one small arm64 accessor.
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
const LC_SYMTAB: u32 = 0x2;

/// `nlist_64.n_type` bits: debugger entries, and the type field's "defined in a section".
const N_STAB: u8 = 0xe0;
const N_TYPE: u8 = 0x0e;
const N_SECT: u8 = 0x0e;
const NLIST_64: usize = 16;

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

/// The unslid addresses of `names` (C names, without Mach-O's leading underscore) in `path`'s
/// symbol table, private symbols included: `dlsym` finds only exported ones. `None` for a name
/// that isn't defined there; empty if `path` isn't an arm64 Mach-O with a symbol table.
pub fn symbols(path: &Path, names: &[&str]) -> std::io::Result<Vec<Option<u64>>> {
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
    let Some((symoff, nsyms, stroff, strsize)) = symtab_command(&image) else { return Ok(Vec::new()) };
    let mut read_at = |off: u32, len: usize| -> std::io::Result<Vec<u8>> {
        f.seek(SeekFrom::Start(slice + u64::from(off)))?;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf)?;
        Ok(buf)
    };
    let syms = read_at(symoff, nsyms as usize * NLIST_64)?;
    let strs = read_at(stroff, strsize as usize)?;
    Ok(find_symbols(&syms, &strs, names))
}

/// `LC_SYMTAB`'s symbol table offset and count, string table offset and size.
fn symtab_command(image: &[u8]) -> Option<(u32, u32, u32, u32)> {
    if u32_at(image, 0, false)? != MH_MAGIC_64 {
        return None;
    }
    let ncmds = u32_at(image, 16, false)? as usize;
    let mut off = MACH_HEADER_64;
    for _ in 0..ncmds {
        let cmd = u32_at(image, off, false)?;
        let size = u32_at(image, off + 4, false)? as usize;
        if size < 8 {
            return None;
        }
        if cmd == LC_SYMTAB {
            let at = |i| u32_at(image, off + i, false);
            return Some((at(8)?, at(12)?, at(16)?, at(20)?));
        }
        off += size;
    }
    None
}

/// Looks `names` up in a symbol table (`nlist_64` entries) and its string table: section-defined
/// symbols only, debugger (stab) entries skipped.
fn find_symbols(syms: &[u8], strs: &[u8], names: &[&str]) -> Vec<Option<u64>> {
    let mut found = vec![None; names.len()];
    for e in syms.as_chunks::<NLIST_64>().0 {
        let ty = e[4];
        if ty & N_STAB != 0 || ty & N_TYPE != N_SECT {
            continue;
        }
        let Some(strx) = u32_at(e, 0, false) else { continue };
        let Some(raw) = strs.get(strx as usize..) else { continue };
        let name = &raw[..raw.iter().position(|&c| c == 0).unwrap_or(raw.len())];
        let Some(name) = name.strip_prefix(b"_") else { continue };
        if let Some(i) = names.iter().position(|n| n.as_bytes() == name) {
            found[i] = e[8..16].try_into().ok().map(u64::from_le_bytes);
        }
    }
    found
}

/// The two field offsets in an accessor compiled as
/// `ldr xA, [x0, #a]; ldr xB, [xA, #b]; cmp xB, #0; cset w0, ne; ret`
/// (`return this->a->b != nullptr`), as clang builds `FrameBuffer::hasEmulationVk()`: where the
/// object keeps its implementation, and where that keeps the pointer tested. `None` for any other
/// code, so a renderer built differently is never misread.
pub fn pointer_test_offsets(code: [u32; 5]) -> Option<(usize, usize)> {
    // LDR (immediate, unsigned offset), 64-bit: imm12 scaled by 8, Rn, Rt.
    let ldr = |w: u32| (w & 0xffc0_0000 == 0xf940_0000).then(|| (((w >> 10) & 0xfff) as usize * 8, (w >> 5) & 31, w & 31));
    let (a, base, ra) = ldr(code[0])?;
    let (b, mid, rb) = ldr(code[1])?;
    // CMP xB, #0 is SUBS xzr, xB, #0.
    let cmp_zero = 0xf100_001f | (rb << 5);
    // CSET w0, ne (CSINC w0, wzr, wzr, eq); RET.
    (base == 0 && mid == ra && code[2] == cmp_zero && code[3] == 0x1a9f_07e0 && code[4] == 0xd65f_03c0).then_some((a, b))
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

    /// `fake_dylib` plus an `LC_SYMTAB` and its tables: `(name, n_type, value)` per symbol.
    pub(crate) fn fake_dylib_with_symbols(symbols: &[(&str, u8, u64)]) -> Vec<u8> {
        let mut image = fake_dylib("@rpath/libsym.dylib", &[]);
        let mut strs = vec![0u8];
        let mut syms = Vec::new();
        for (name, ty, value) in symbols {
            syms.extend_from_slice(&(strs.len() as u32).to_le_bytes());
            syms.extend_from_slice(&[*ty, 1, 0, 0]);
            syms.extend_from_slice(&value.to_le_bytes());
            strs.extend_from_slice(format!("_{name}").as_bytes());
            strs.push(0);
        }
        // Header fields: ncmds at 16, sizeofcmds at 20; the new command goes after the others.
        let cmds_end = MACH_HEADER_64 + u32_at(&image, 20, false).expect("cmds") as usize;
        let symoff = (cmds_end + 24) as u32;
        let stroff = symoff + syms.len() as u32;
        let mut cmd = Vec::new();
        for v in [LC_SYMTAB, 24, symoff, symbols.len() as u32, stroff, strs.len() as u32] {
            cmd.extend_from_slice(&v.to_le_bytes());
        }
        image.splice(cmds_end..cmds_end, cmd);
        let ncmds = u32_at(&image, 16, false).expect("ncmds") + 1;
        let size = u32_at(&image, 20, false).expect("size") + 24;
        image[16..20].copy_from_slice(&ncmds.to_le_bytes());
        image[20..24].copy_from_slice(&size.to_le_bytes());
        image.extend(syms);
        image.extend(strs);
        image
    }

    #[test]
    fn finds_private_and_exported_symbols_but_not_debug_entries() {
        let image = fake_dylib_with_symbols(&[
            ("exported", 0x0f, 0x1000),
            ("private", 0x1e, 0x2000),
            ("private", 0x24, 0x9999), // the N_FUN stab for it, listed first in real images
            ("undefined", 0x01, 0),
        ]);
        let dir = std::env::temp_dir().join(format!("emu-gfxstream-syms-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("libsym.dylib");
        std::fs::write(&path, &image).expect("write");
        let found = symbols(&path, &["private", "exported", "undefined", "missing"]).expect("symbols");
        assert_eq!(found, [Some(0x2000), Some(0x1000), None, None]);
        // Load-time dependencies still parse with a symbol table present.
        assert!(dylib_deps(&path).expect("deps").is_empty());
        std::fs::write(&path, b"#!/bin/sh\n").expect("write");
        assert!(symbols(&path, &["private"]).expect("not mach-o").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recognizes_only_the_exact_pointer_test() {
        // gfxstream 37.3.10's FrameBuffer::hasEmulationVk(): mImpl at 112, m_emulationVk at 13384.
        let real = [0xf940_3808, 0xf95a_2508, 0xf100_011f, 0x1a9f_07e0, 0xd65f_03c0];
        assert_eq!(pointer_test_offsets(real), Some((112, 13384)));
        let mut other_base = real;
        other_base[0] = 0xf940_3828; // ldr x8, [x1, #112]: not `this`
        assert_eq!(pointer_test_offsets(other_base), None);
        let mut chained_wrong = real;
        chained_wrong[1] = 0xf95a_2528; // ldr x8, [x9, #13384]: not the first load's result
        assert_eq!(pointer_test_offsets(chained_wrong), None);
        let mut cmp_other = real;
        cmp_other[2] = 0xf100_013f; // cmp x9, #0
        assert_eq!(pointer_test_offsets(cmp_other), None);
        let mut not_ret = real;
        not_ret[4] = 0xd503_201f; // nop
        assert_eq!(pointer_test_offsets(not_ret), None);
        assert_eq!(pointer_test_offsets([0xd65f_03c0; 5]), None);
    }

    #[test]
    fn reads_a_real_binary() {
        // The test binary itself links libSystem.
        let exe = std::env::current_exe().expect("exe");
        let deps = dylib_deps(&exe).expect("deps");
        assert!(deps.iter().any(|d| d == "/usr/lib/libSystem.B.dylib"), "{deps:?}");
    }
}
