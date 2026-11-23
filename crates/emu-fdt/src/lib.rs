//! Minimal flattened device tree (DTB, version 17) writer.
//!
//! Nodes are written in order: `begin_node`, properties, child nodes, `end_node`.
//! `finish` checks nesting and returns the blob.

const MAGIC: u32 = 0xd00d_feed;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const END: u32 = 9;
const HEADER_LEN: usize = 40;

#[derive(Debug, PartialEq, Eq)]
pub enum FdtError {
    Unbalanced(usize),
}

impl std::fmt::Display for FdtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FdtError::Unbalanced(n) => write!(f, "device tree has {n} unclosed node(s)"),
        }
    }
}

impl std::error::Error for FdtError {}

#[derive(Default)]
pub struct Fdt {
    structs: Vec<u8>,
    strings: Vec<u8>,
    depth: usize,
    phandle: u32,
}

impl Fdt {
    pub fn new() -> Fdt {
        Fdt::default()
    }

    /// Hands out a fresh phandle (starting at 1).
    pub fn alloc_phandle(&mut self) -> u32 {
        self.phandle += 1;
        self.phandle
    }

    fn token(&mut self, t: u32) {
        self.structs.extend_from_slice(&t.to_be_bytes());
    }

    fn pad(&mut self) {
        while !self.structs.len().is_multiple_of(4) {
            self.structs.push(0);
        }
    }

    fn string_offset(&mut self, name: &str) -> u32 {
        let needle: Vec<u8> = name.bytes().chain(std::iter::once(0)).collect();
        // Reuse an existing entry (exact match at an entry start or a suffix of one).
        if let Some(pos) = self.strings.windows(needle.len()).position(|w| w == needle.as_slice()) {
            return pos as u32;
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(&needle);
        off
    }

    pub fn begin_node(&mut self, name: &str) {
        self.token(BEGIN_NODE);
        self.structs.extend_from_slice(name.as_bytes());
        self.structs.push(0);
        self.pad();
        self.depth += 1;
    }

    pub fn end_node(&mut self) {
        self.token(END_NODE);
        self.depth = self.depth.saturating_sub(1);
    }

    pub fn prop(&mut self, name: &str, value: &[u8]) {
        let nameoff = self.string_offset(name);
        self.token(PROP);
        self.structs.extend_from_slice(&(value.len() as u32).to_be_bytes());
        self.structs.extend_from_slice(&nameoff.to_be_bytes());
        self.structs.extend_from_slice(value);
        self.pad();
    }

    pub fn prop_empty(&mut self, name: &str) {
        self.prop(name, &[]);
    }

    pub fn prop_u32(&mut self, name: &str, v: u32) {
        self.prop(name, &v.to_be_bytes());
    }

    pub fn prop_u64(&mut self, name: &str, v: u64) {
        self.prop(name, &v.to_be_bytes());
    }

    pub fn prop_cells(&mut self, name: &str, cells: &[u32]) {
        let bytes: Vec<u8> = cells.iter().flat_map(|c| c.to_be_bytes()).collect();
        self.prop(name, &bytes);
    }

    /// `reg`-style list of (u64 address, u64 size) pairs, for #address-cells = #size-cells = 2.
    pub fn prop_reg(&mut self, name: &str, pairs: &[(u64, u64)]) {
        let bytes: Vec<u8> = pairs.iter().flat_map(|(a, s)| a.to_be_bytes().into_iter().chain(s.to_be_bytes())).collect();
        self.prop(name, &bytes);
    }

    pub fn prop_str(&mut self, name: &str, s: &str) {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        self.prop(name, &v);
    }

    pub fn prop_strs(&mut self, name: &str, list: &[&str]) {
        let mut v = Vec::new();
        for s in list {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        self.prop(name, &v);
    }

    pub fn finish(mut self) -> Result<Vec<u8>, FdtError> {
        if self.depth != 0 {
            return Err(FdtError::Unbalanced(self.depth));
        }
        self.token(END);
        let rsvmap_off = HEADER_LEN;
        let rsvmap_len = 16; // one terminating (0, 0) entry
        let struct_off = rsvmap_off + rsvmap_len;
        let strings_off = struct_off + self.structs.len();
        let total = strings_off + self.strings.len();
        let mut out = Vec::with_capacity(total);
        for v in [
            MAGIC,
            total as u32,
            struct_off as u32,
            strings_off as u32,
            rsvmap_off as u32,
            17, // version
            16, // last compatible version
            0,  // boot cpu
            self.strings.len() as u32,
            self.structs.len() as u32,
        ] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(&self.structs);
        out.extend_from_slice(&self.strings);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be32(b: &[u8], off: usize) -> u32 {
        u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
    }

    #[test]
    fn header_and_layout() {
        let mut f = Fdt::new();
        f.begin_node("");
        f.prop_u32("#address-cells", 2);
        f.prop_str("compatible", "linux,dummy-virt");
        f.begin_node("memory@80000000");
        f.prop_str("device_type", "memory");
        f.prop_reg("reg", &[(0x8000_0000, 0x1000_0000)]);
        f.end_node();
        f.end_node();
        let b = f.finish().expect("balanced");
        assert_eq!(be32(&b, 0), MAGIC);
        assert_eq!(be32(&b, 4) as usize, b.len());
        let struct_off = be32(&b, 8) as usize;
        assert_eq!(be32(&b, struct_off), BEGIN_NODE);
        let strings_off = be32(&b, 12) as usize;
        let strings = &b[strings_off..];
        assert!(strings.starts_with(b"#address-cells\0"));
        // Last struct token before the strings block is END.
        assert_eq!(be32(&b, strings_off - 4), END);
    }

    #[test]
    fn rejects_unbalanced() {
        let mut f = Fdt::new();
        f.begin_node("");
        assert_eq!(f.finish().err(), Some(FdtError::Unbalanced(1)));
    }

    #[test]
    fn dedups_strings() {
        let mut f = Fdt::new();
        f.begin_node("");
        f.prop_u32("reg", 1);
        f.prop_u32("reg", 2);
        f.end_node();
        let b = f.finish().expect("balanced");
        assert_eq!(be32(&b, 32), 4, "one 'reg\\0' entry");
    }
}
