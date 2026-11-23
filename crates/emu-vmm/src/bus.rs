//! MMIO dispatch: guest physical address ranges → devices.
use std::sync::Arc;

/// A memory-mapped device. Offsets are relative to the device's base address. Accesses are
/// 1, 2, 4 or 8 bytes, little-endian.
pub trait MmioDevice: Send + Sync {
    fn read(&self, offset: u64, data: &mut [u8]);
    fn write(&self, offset: u64, data: &[u8]);
}

struct Range {
    base: u64,
    len: u64,
    dev: Arc<dyn MmioDevice>,
}

/// Immutable after construction, so vCPU threads read it without locking.
#[derive(Default)]
pub struct Bus {
    ranges: Vec<Range>,
}

impl Bus {
    pub fn new() -> Bus {
        Bus::default()
    }

    /// Registers `dev` at `[base, base + len)`. Panics on overlap: the memory map is static.
    pub fn insert(&mut self, base: u64, len: u64, dev: Arc<dyn MmioDevice>) {
        assert!(
            self.ranges.iter().all(|r| base + len <= r.base || r.base + r.len <= base),
            "MMIO range {base:#x}+{len:#x} overlaps an existing device"
        );
        self.ranges.push(Range { base, len, dev });
        self.ranges.sort_by_key(|r| r.base);
    }

    fn find(&self, addr: u64) -> Option<&Range> {
        let i = self.ranges.partition_point(|r| r.base <= addr);
        let r = self.ranges.get(i.checked_sub(1)?)?;
        (addr < r.base + r.len).then_some(r)
    }

    /// Returns false when nothing is mapped at `addr`.
    pub fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        match self.find(addr) {
            Some(r) => {
                r.dev.read(addr - r.base, data);
                true
            }
            None => false,
        }
    }

    pub fn write(&self, addr: u64, data: &[u8]) -> bool {
        match self.find(addr) {
            Some(r) => {
                r.dev.write(addr - r.base, data);
                true
            }
            None => false,
        }
    }
}

/// Little-endian value of a 1–8 byte access.
pub fn le_value(data: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b[..data.len().min(8)].copy_from_slice(&data[..data.len().min(8)]);
    u64::from_le_bytes(b)
}

/// Stores `v` into a 1–8 byte access buffer, little-endian.
pub fn set_le(data: &mut [u8], v: u64) {
    let n = data.len().min(8);
    data[..n].copy_from_slice(&v.to_le_bytes()[..n]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Probe(Mutex<Vec<(u64, u64)>>);
    impl MmioDevice for Probe {
        fn read(&self, offset: u64, data: &mut [u8]) {
            set_le(data, offset);
        }
        fn write(&self, offset: u64, data: &[u8]) {
            self.0.lock().expect("lock").push((offset, le_value(data)));
        }
    }

    #[test]
    fn dispatches_by_range() {
        let a = Arc::new(Probe::default());
        let mut bus = Bus::new();
        bus.insert(0x1000, 0x100, a.clone());
        bus.insert(0x3000, 0x100, Arc::new(Probe::default()));
        let mut buf = [0u8; 4];
        assert!(bus.read(0x1010, &mut buf));
        assert_eq!(le_value(&buf), 0x10);
        assert!(!bus.read(0x1100, &mut buf), "one past the end");
        assert!(!bus.read(0x0fff, &mut buf));
        assert!(bus.write(0x10fc, &7u32.to_le_bytes()));
        assert_eq!(a.0.lock().expect("lock").as_slice(), &[(0xfc, 7)]);
    }

    #[test]
    #[should_panic(expected = "overlaps")]
    fn rejects_overlap() {
        let mut bus = Bus::new();
        bus.insert(0x1000, 0x100, Arc::new(Probe::default()));
        bus.insert(0x10f0, 0x100, Arc::new(Probe::default()));
    }
}
