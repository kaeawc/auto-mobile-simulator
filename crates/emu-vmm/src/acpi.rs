//! ACPI tables emu adds to a frontend's (QEMU's `-acpitable file=`), for x86 guests whose goldfish
//! devices are ACPI platform devices: the goldfish pipe binds to `GFSH0003` and nothing else on
//! x86 (its module's aliases are that and a device-tree compatible). Upstream QEMU has no such
//! device, so emu serves the pipe's registers in the BAR of a PCI function over vfio-user, and
//! this table describes a `GFSH0003` device whose resources are that BAR and that function's
//! interrupt, read at the time the guest asks (`_CRS`), since the firmware places both.
//!
//! Only the AML this table needs is encoded here (ACPI 6.5, chapter 20).

/// `GFSH0003` with its `_CRS`: memory at the carrier function's BAR0, `len` bytes, and the IRQ
/// its INTA reaches on QEMU's `pc` machine: slot `slot`'s INTA goes to PIRQ `(slot - 1) & 3`
/// (`pc_pci_slot_get_pirq` in hw/i386/pc_piix.c: `(intx + slot - 1) & 3`), whose current IRQ is
/// that PIRQ's route register in PIIX3 (00:01.0, config 0x60..0x63; bit 7 set means disabled),
/// which QEMU's own `LNKx` devices read too.
pub fn goldfish_pipe_ssdt(slot: u8, len: u32) -> Vec<u8> {
    let pirq = u32::from(slot.wrapping_sub(1) & 3);
    let cfg = |dev: u8, reg: u8| 0x8000_0000u32 | (u32::from(dev) << 11) | u32::from(reg);
    // ResourceTemplate: Memory32Fixed(ReadWrite, 0, len), Interrupt(Consumer, Level, ActiveHigh,
    // Shared) {0}, end tag. The base is at byte 4, the IRQ at byte 17.
    let mut rbuf = vec![0x86, 0x09, 0x00, 0x01];
    rbuf.extend_from_slice(&0u32.to_le_bytes());
    rbuf.extend_from_slice(&len.to_le_bytes());
    rbuf.extend_from_slice(&[0x89, 0x06, 0x00, 0x09, 0x01]);
    rbuf.extend_from_slice(&0u32.to_le_bytes());
    rbuf.extend_from_slice(&[0x79, 0x00]);
    debug_assert_eq!(rbuf.len(), 23);

    let mut crs = vec![
        // CreateDWordField(RBUF, 4, MBAS); CreateDWordField(RBUF, 17, IRQN)
        CREATE_DWORD_FIELD,
    ];
    crs.extend(name_seg("RBUF"));
    crs.extend([BYTE_PREFIX, 4]);
    crs.extend(name_seg("MBAS"));
    crs.push(CREATE_DWORD_FIELD);
    crs.extend(name_seg("RBUF"));
    crs.extend([BYTE_PREFIX, 17]);
    crs.extend(name_seg("IRQN"));
    // CADR = config address of the carrier's BAR0; MBAS = CDAT & ~0xf
    crs.extend(store(dword(cfg(slot, 0x10)), name_seg("CADR")));
    crs.extend(store(and(name_seg("CDAT"), dword(0xffff_fff0)), name_seg("MBAS")));
    // CADR = PIIX3's PIRQ routes; IRQN = (CDAT >> (8 * pirq)) & 0x7f
    crs.extend(store(dword(cfg(1, 0x60)), name_seg("CADR")));
    crs.extend(store(and(shift_right(name_seg("CDAT"), byte(8 * pirq as u8)), byte(0x7f)), name_seg("IRQN")));
    crs.push(RETURN);
    crs.extend(name_seg("RBUF"));

    let mut dev = Vec::new();
    dev.extend(name("_HID", string("GFSH0003")));
    dev.extend(name("_UID", vec![ZERO]));
    // OperationRegion(PCIC, SystemIO, 0xCF8, 8); Field(PCIC, DWordAcc, NoLock, Preserve) {CADR, 32, CDAT, 32}
    dev.extend([EXT, OP_REGION]);
    dev.extend(name_seg("PCIC"));
    dev.push(SYSTEM_IO);
    dev.extend(word(0xcf8));
    dev.extend(byte(8));
    let mut field = name_seg("PCIC");
    field.push(0x03); // DWordAcc, NoLock, Preserve
    for f in ["CADR", "CDAT"] {
        field.extend(name_seg(f));
        field.push(32);
    }
    dev.extend([EXT, FIELD]);
    dev.extend(pkg(field));
    dev.extend(name("RBUF", buffer(&rbuf)));
    dev.extend(method("_STA", 0, [RETURN, BYTE_PREFIX, 0x0f].to_vec()));
    dev.extend(method("_CRS", METHOD_SERIALIZED, crs));

    let mut device = name_seg("GFPP");
    device.extend(dev);
    let mut scope = vec![ROOT];
    scope.extend(name_seg("_SB_"));
    scope.extend([EXT, DEVICE]);
    scope.extend(pkg(device));
    let mut body = vec![SCOPE];
    body.extend(pkg(scope));
    table(*b"SSDT", 2, *b"GFSHPIPE", &body)
}

const ZERO: u8 = 0x00;
const BYTE_PREFIX: u8 = 0x0a;
const WORD_PREFIX: u8 = 0x0b;
const DWORD_PREFIX: u8 = 0x0c;
const STRING_PREFIX: u8 = 0x0d;
const SCOPE: u8 = 0x10;
const BUFFER: u8 = 0x11;
const NAME: u8 = 0x08;
const METHOD: u8 = 0x14;
const ROOT: u8 = 0x5c;
const EXT: u8 = 0x5b;
const OP_REGION: u8 = 0x80;
const FIELD: u8 = 0x81;
const DEVICE: u8 = 0x82;
const STORE: u8 = 0x70;
const AND: u8 = 0x7b;
const SHIFT_RIGHT: u8 = 0x7a;
const CREATE_DWORD_FIELD: u8 = 0x8a;
const RETURN: u8 = 0xa4;
const SYSTEM_IO: u8 = 0x01;
const METHOD_SERIALIZED: u8 = 0x08;

/// A 4-character name segment, padded with `_`.
fn name_seg(s: &str) -> Vec<u8> {
    let mut b: Vec<u8> = s.bytes().take(4).collect();
    b.resize(4, b'_');
    b
}

/// `PkgLength` then `body`: the length counts itself (one to four bytes) and the body.
fn pkg(body: Vec<u8>) -> Vec<u8> {
    let n = body.len();
    let mut out = if n + 1 < 0x40 {
        vec![(n + 1) as u8]
    } else if n + 2 < 0x1000 {
        let t = n + 2;
        vec![0x40 | (t & 0xf) as u8, (t >> 4) as u8]
    } else if n + 3 < 0x10_0000 {
        let t = n + 3;
        vec![0x80 | (t & 0xf) as u8, (t >> 4) as u8, (t >> 12) as u8]
    } else {
        let t = n + 4;
        vec![0xc0 | (t & 0xf) as u8, (t >> 4) as u8, (t >> 12) as u8, (t >> 20) as u8]
    };
    out.extend(body);
    out
}

fn byte(v: u8) -> Vec<u8> {
    vec![BYTE_PREFIX, v]
}

fn word(v: u16) -> Vec<u8> {
    let mut b = vec![WORD_PREFIX];
    b.extend_from_slice(&v.to_le_bytes());
    b
}

fn dword(v: u32) -> Vec<u8> {
    let mut b = vec![DWORD_PREFIX];
    b.extend_from_slice(&v.to_le_bytes());
    b
}

fn string(s: &str) -> Vec<u8> {
    let mut b = vec![STRING_PREFIX];
    b.extend_from_slice(s.as_bytes());
    b.push(0);
    b
}

fn buffer(data: &[u8]) -> Vec<u8> {
    let mut body = byte(data.len() as u8);
    body.extend_from_slice(data);
    let mut b = vec![BUFFER];
    b.extend(pkg(body));
    b
}

fn name(n: &str, value: Vec<u8>) -> Vec<u8> {
    let mut b = vec![NAME];
    b.extend(name_seg(n));
    b.extend(value);
    b
}

fn method(n: &str, flags: u8, body: Vec<u8>) -> Vec<u8> {
    let mut inner = name_seg(n);
    inner.push(flags);
    inner.extend(body);
    let mut b = vec![METHOD];
    b.extend(pkg(inner));
    b
}

fn store(value: Vec<u8>, target: Vec<u8>) -> Vec<u8> {
    let mut b = vec![STORE];
    b.extend(value);
    b.extend(target);
    b
}

/// `And(a, b)` with no target (`NullName`).
fn and(a: Vec<u8>, b: Vec<u8>) -> Vec<u8> {
    let mut out = vec![AND];
    out.extend(a);
    out.extend(b);
    out.push(ZERO);
    out
}

fn shift_right(a: Vec<u8>, n: Vec<u8>) -> Vec<u8> {
    let mut out = vec![SHIFT_RIGHT];
    out.extend(a);
    out.extend(n);
    out.push(ZERO);
    out
}

/// A system description table: the 36-byte header (OEM "SIMEE", creator "SIME") and `body`, with
/// its length and checksum (all bytes sum to zero).
fn table(signature: [u8; 4], revision: u8, oem_table: [u8; 8], body: &[u8]) -> Vec<u8> {
    let len = 36 + body.len();
    let mut t = Vec::with_capacity(len);
    t.extend_from_slice(&signature);
    t.extend_from_slice(&(len as u32).to_le_bytes());
    t.push(revision);
    t.push(0); // checksum, below
    t.extend_from_slice(b"SIMEE ");
    t.extend_from_slice(&oem_table);
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(b"SIME");
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(body);
    t[9] = 0u8.wrapping_sub(t.iter().fold(0u8, |a, &b| a.wrapping_add(b)));
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes a `PkgLength` at the start of `b`: (value, bytes it takes).
    fn pkg_len(b: &[u8]) -> (usize, usize) {
        let extra = (b[0] >> 6) as usize;
        if extra == 0 {
            return ((b[0] & 0x3f) as usize, 1);
        }
        let mut v = (b[0] & 0x0f) as usize;
        for (i, &x) in b[1..=extra].iter().enumerate() {
            v |= (x as usize) << (4 + 8 * i);
        }
        (v, extra + 1)
    }

    #[test]
    fn package_lengths_count_themselves() {
        for n in [0usize, 1, 61, 62, 63, 200, 4093, 4094, 70_000] {
            let p = pkg(vec![0xaa; n]);
            let (v, w) = pkg_len(&p);
            assert_eq!((v, p.len()), (p.len(), w + n), "body of {n}");
        }
    }

    #[test]
    fn the_pipe_table_is_a_valid_ssdt() {
        let t = goldfish_pipe_ssdt(0x1e, 0x2000);
        assert_eq!(&t[0..4], b"SSDT");
        assert_eq!(u32::from_le_bytes(t[4..8].try_into().unwrap_or_default()) as usize, t.len());
        assert_eq!(t.iter().fold(0u8, |a, &b| a.wrapping_add(b)), 0, "checksum");
        // Scope(\_SB) spanning the whole body, holding the device.
        assert_eq!(t[36], SCOPE);
        let (len, w) = pkg_len(&t[37..]);
        assert_eq!(37 + len, t.len(), "the scope's package is the rest of the table");
        assert_eq!(&t[37 + w..37 + w + 5], b"\\_SB_");
        let has = |needle: &[u8]| t.windows(needle.len()).any(|x| x == needle);
        assert!(has(b"GFSH0003\0") && has(b"GFPP") && has(b"_CRS") && has(b"_STA"));
        // The carrier's BAR0 config address (slot 0x1e, register 0x10) and PIIX3's routes.
        assert!(has(&[DWORD_PREFIX, 0x10, 0xf0, 0x00, 0x80]));
        assert!(has(&[DWORD_PREFIX, 0x60, 0x08, 0x00, 0x80]));
        // Slot 0x1e's INTA is PIRQB ((0 + 0x1e - 1) & 3 = 1): the second route byte.
        assert!(has(&[SHIFT_RIGHT, b'C', b'D', b'A', b'T', BYTE_PREFIX, 8, ZERO]));
        // The resource template: Memory32Fixed of 0x2000 bytes, a shared level interrupt.
        assert!(has(&[0x86, 0x09, 0x00, 0x01, 0, 0, 0, 0, 0x00, 0x20, 0, 0]));
        assert!(has(&[0x89, 0x06, 0x00, 0x09, 0x01]));
    }

    /// Writes the table for a disassembler: `EMU_ACPI_DUMP=<path> cargo test -p simee-emu-vmm dump_for_iasl -- --ignored`.
    #[test]
    #[ignore = "writes a file"]
    fn dump_for_iasl() {
        if let Some(path) = std::env::var_os("EMU_ACPI_DUMP") {
            std::fs::write(path, goldfish_pipe_ssdt(0x1e, 0x2000)).expect("write");
        }
    }
}
