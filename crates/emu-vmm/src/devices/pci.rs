//! Minimal PCIe host bridge (ECAM, "pci-host-ecam-generic") on bus 0.
//!
//! BARs are assigned by us and the guest keeps them (`linux,pci-probe-only`), so BAR-backed
//! MMIO sits at fixed addresses on the ordinary bus and RAM-like BARs are mapped once at boot.
//! Only what's needed for config cycles is modelled: IDs, command/status, class, BAR sizing.
use std::sync::{Arc, Mutex};

use crate::bus::{MmioDevice, le_value, set_le};

#[derive(Clone, Copy, Debug)]
pub enum BarKind {
    Mem32,
    /// 64-bit, prefetchable; occupies two BAR registers.
    Mem64Prefetch,
}

#[derive(Clone, Copy, Debug)]
pub struct Bar {
    pub kind: BarKind,
    pub addr: u64,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct PciFunction {
    pub vendor: u16,
    pub device: u16,
    /// class << 16 | subclass << 8 | prog-if
    pub class: u32,
    /// Up to 6 BAR registers; a 64-bit BAR at index i also uses i + 1.
    pub bars: Vec<(usize, Bar)>,
}

struct FnState {
    f: PciFunction,
    command: u16,
    /// Current BAR register values (what the guest wrote, masked by size).
    bar_regs: [u32; 6],
    /// Size masks per BAR register (0 for unused registers).
    bar_masks: [u32; 6],
    bar_flags: [u32; 6],
}

impl FnState {
    fn new(f: PciFunction) -> FnState {
        let (mut regs, mut masks, mut flags) = ([0u32; 6], [0u32; 6], [0u32; 6]);
        for &(i, b) in &f.bars {
            match b.kind {
                BarKind::Mem32 => {
                    regs[i] = b.addr as u32;
                    masks[i] = !(b.size as u32 - 1);
                    flags[i] = 0;
                }
                BarKind::Mem64Prefetch => {
                    let mask = !(b.size - 1);
                    regs[i] = b.addr as u32;
                    regs[i + 1] = (b.addr >> 32) as u32;
                    masks[i] = mask as u32;
                    masks[i + 1] = (mask >> 32) as u32;
                    flags[i] = 0b1100; // 64-bit, prefetchable
                }
            }
        }
        FnState { f, command: 0, bar_regs: regs, bar_masks: masks, bar_flags: flags }
    }

    fn read(&self, reg: usize) -> u32 {
        match reg {
            0x00 => u32::from(self.f.vendor) | (u32::from(self.f.device) << 16),
            0x04 => u32::from(self.command) | (0x0010 << 16), // status: capabilities list off, ok
            0x08 => self.f.class << 8 | 0x01,                 // revision 1
            0x0c => 0,                                        // header type 0, single function
            0x10..=0x24 => {
                let i = (reg - 0x10) / 4;
                (self.bar_regs[i] & self.bar_masks[i]) | self.bar_flags[i]
            }
            0x2c => u32::from(self.f.vendor) | (u32::from(self.f.device) << 16), // subsystem
            0x3c => 0x0000_00ff,                                                 // no interrupt pin
            _ => 0,
        }
    }

    fn write(&mut self, reg: usize, v: u32) {
        match reg {
            0x04 => self.command = v as u16,
            0x10..=0x24 => {
                let i = (reg - 0x10) / 4;
                if self.bar_masks[i] != 0 {
                    self.bar_regs[i] = v;
                }
            }
            _ => {}
        }
    }
}

/// The ECAM window: 1 MiB per bus, 32 KiB per device, 4 KiB per function.
pub struct PciHost {
    fns: Mutex<Vec<FnState>>,
}

impl PciHost {
    /// Devices land at 00:01.0, 00:02.0, ... in order.
    pub fn new(functions: Vec<PciFunction>) -> Arc<PciHost> {
        Arc::new(PciHost { fns: Mutex::new(functions.into_iter().map(FnState::new).collect()) })
    }

    fn locate(offset: u64) -> (u64, u64, u64, usize) {
        let bus = offset >> 20;
        let dev = (offset >> 15) & 0x1f;
        let func = (offset >> 12) & 0x7;
        (bus, dev, func, (offset & 0xfff) as usize)
    }
}

impl MmioDevice for PciHost {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let (bus, dev, func, reg) = Self::locate(offset);
        let fns = self.fns.lock().unwrap_or_else(|e| e.into_inner());
        let v = match (bus, dev, func) {
            (0, d, 0) if d >= 1 && (d as usize) <= fns.len() => {
                let word = fns[d as usize - 1].read(reg & !3);
                u64::from(word >> ((reg & 3) * 8))
            }
            _ => u64::MAX, // no device: all ones
        };
        set_le(data, v);
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let (bus, dev, func, reg) = Self::locate(offset);
        let mut fns = self.fns.lock().unwrap_or_else(|e| e.into_inner());
        if bus == 0 && func == 0 && dev >= 1 && (dev as usize) <= fns.len() {
            let f = &mut fns[dev as usize - 1];
            let v = le_value(data) as u32;
            if data.len() == 4 {
                f.write(reg & !3, v);
            } else {
                // Sub-word writes (command register is written as u16): merge into the word.
                let shift = (reg & 3) * 8;
                let mask = if data.len() >= 4 { u32::MAX } else { ((1u32 << (data.len() * 8)) - 1) << shift };
                let old = f.read(reg & !3);
                f.write(reg & !3, (old & !mask) | ((v << shift) & mask));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rd(h: &PciHost, off: u64) -> u32 {
        let mut b = [0u8; 4];
        h.read(off, &mut b);
        u32::from_le_bytes(b)
    }

    fn wr(h: &PciHost, off: u64, v: u32) {
        h.write(off, &v.to_le_bytes());
    }

    #[test]
    fn ids_and_bar_sizing() {
        let h = PciHost::new(vec![PciFunction {
            vendor: 0x607d,
            device: 0xf153,
            class: 0x05_00_00,
            bars: vec![
                (0, Bar { kind: BarKind::Mem32, addr: 0x1000_0000, size: 0x1000 }),
                (1, Bar { kind: BarKind::Mem64Prefetch, addr: 0x8_0000_0000, size: 16 << 30 }),
            ],
        }]);
        let dev1 = 1 << 15;
        assert_eq!(rd(&h, dev1), 0xf153_607d);
        assert_eq!(rd(&h, 0), u32::MAX, "00:00.0 is empty");
        assert_eq!(rd(&h, dev1 + 0x10), 0x1000_0000);
        assert_eq!(rd(&h, dev1 + 0x14), 0x0000_000c, "64-bit prefetchable, low bits of 0x8_0000_0000");
        assert_eq!(rd(&h, dev1 + 0x18), 0x8);
        // Sizing: write all ones, read the mask back.
        wr(&h, dev1 + 0x10, u32::MAX);
        assert_eq!(rd(&h, dev1 + 0x10), 0xffff_f000);
        wr(&h, dev1 + 0x14, u32::MAX);
        wr(&h, dev1 + 0x18, u32::MAX);
        assert_eq!(rd(&h, dev1 + 0x14), 0x0000_000c);
        assert_eq!(rd(&h, dev1 + 0x18), 0xffff_fffc);
        // Restore and enable memory decoding with a 16-bit write.
        wr(&h, dev1 + 0x10, 0x1000_0000);
        h.write(dev1 + 0x04, &6u16.to_le_bytes());
        assert_eq!(rd(&h, dev1 + 0x04) & 0xffff, 6);
    }
}
