//! Linux bootconfig: `androidboot.*` properties appended to the initrd.
//!
//! Layout (Documentation/admin-guide/bootconfig.rst): the initrd, then `key="value"\n` lines
//! padded with NULs to a 4-byte boundary, then the le32 size of that block, the le32 checksum
//! (sum of its bytes) and the 12-byte magic `#BOOTCONFIG\n`.

const MAGIC: &[u8] = b"#BOOTCONFIG\n";

#[derive(Clone, Debug, Default)]
pub struct BootConfig {
    entries: Vec<(String, String)>,
}

impl BootConfig {
    pub fn new() -> BootConfig {
        BootConfig::default()
    }

    /// Sets `key`, replacing an earlier value.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        let (key, value) = (key.into(), value.into());
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some(e) => e.1 = value,
            None => self.entries.push((key, value)),
        }
        self
    }

    /// Every entry, in the order first set.
    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// The block appended after the initrd.
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        for (k, v) in &self.entries {
            body.extend_from_slice(k.as_bytes());
            body.extend_from_slice(b"=\"");
            body.extend_from_slice(v.replace('"', "'").as_bytes());
            body.extend_from_slice(b"\"\n");
        }
        body.push(0);
        while !body.len().is_multiple_of(4) {
            body.push(0);
        }
        let checksum = body.iter().fold(0u32, |a, &b| a.wrapping_add(u32::from(b)));
        let mut out = body.clone();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(MAGIC);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_trailer() {
        let mut bc = BootConfig::new();
        bc.set("androidboot.hardware", "ranchu").set("androidboot.qemu", "1").set("androidboot.qemu", "1");
        let b = bc.encode();
        assert!(b.ends_with(MAGIC));
        let n = b.len();
        let size = u32::from_le_bytes([b[n - 20], b[n - 19], b[n - 18], b[n - 17]]) as usize;
        let sum = u32::from_le_bytes([b[n - 16], b[n - 15], b[n - 14], b[n - 13]]);
        assert_eq!(size, n - 20);
        assert_eq!(size % 4, 0);
        assert_eq!(sum, b[..size].iter().map(|&x| u32::from(x)).sum::<u32>());
        let text = String::from_utf8_lossy(&b[..size]);
        assert!(text.starts_with("androidboot.hardware=\"ranchu\"\nandroidboot.qemu=\"1\"\n"));
        assert_eq!(text.matches("androidboot.qemu=").count(), 1);
    }
}
