//! Goldfish address space device (PCI 607d:f153): carves a large guest-physical area into
//! blocks for host-shared memory. Gralloc's host memory allocator requires it.
//!
//! The area BAR is backed by one lazily committed host mapping, so "allocating host memory" for
//! a block needs no work: pages cost nothing until the guest touches them. Pings run per-handle
//! contexts; the first ping on a handle selects its type (`metadata`).
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use emu_hvf::GuestRam;

use crate::bus::{MmioDevice, le_value, set_le};

pub const VENDOR: u16 = 0x607d;
pub const DEVICE: u16 = 0xf153;

const REG_COMMAND: u64 = 0;
const REG_STATUS: u64 = 4;
const REG_GUEST_PAGE_SIZE: u64 = 8;
const REG_BLOCK_SIZE_LOW: u64 = 12;
const REG_BLOCK_SIZE_HIGH: u64 = 16;
const REG_BLOCK_OFFSET_LOW: u64 = 20;
const REG_BLOCK_OFFSET_HIGH: u64 = 24;
const REG_PING: u64 = 28;
const REG_PING_INFO_ADDR_LOW: u64 = 32;
const REG_PING_INFO_ADDR_HIGH: u64 = 36;
const REG_HANDLE: u64 = 40;
const REG_PHYS_START_LOW: u64 = 44;
const REG_PHYS_START_HIGH: u64 = 48;
const REG_PING_WITH_DATA: u64 = 52;

const CMD_ALLOCATE_BLOCK: u32 = 1;
const CMD_DEALLOCATE_BLOCK: u32 = 2;
const CMD_GEN_HANDLE: u32 = 3;
const CMD_DESTROY_HANDLE: u32 = 4;
const CMD_TELL_PING_INFO_ADDR: u32 = 5;

/// Context types a handle can take (first ping's `metadata`).
mod ctx {
    pub const HOST_MEMORY_ALLOCATOR: u64 = 5;
    pub const SHARED_SLOTS_HOST_MEMORY_ALLOCATOR: u64 = 6;
}

/// Host memory allocator commands (ping `metadata` on an allocator handle).
mod hma {
    pub const ALLOCATE: u64 = 1;
    pub const UNALLOCATE: u64 = 2;
    pub const CHECK_SHARED_SLOTS: u64 = 3;
}

#[derive(Default)]
struct State {
    regs: HashMap<u64, u32>,
    page_size: u64,
    /// Free list over the area: offset → size.
    free: BTreeMap<u64, u64>,
    used: BTreeMap<u64, u64>,
    next_handle: u32,
    /// handle → (ping info guest address, context type once set)
    handles: HashMap<u32, (u64, Option<u64>)>,
}

pub struct GoldfishAddressSpace {
    state: Mutex<State>,
    mem: Arc<GuestRam>,
    area: Arc<GuestRam>,
}

impl GoldfishAddressSpace {
    pub fn new(mem: Arc<GuestRam>, area: Arc<GuestRam>) -> GoldfishAddressSpace {
        let mut st = State { page_size: 4096, next_handle: 1, ..Default::default() };
        st.free.insert(0, area.len());
        GoldfishAddressSpace { state: Mutex::new(st), mem, area }
    }

    fn reg(s: &State, r: u64) -> u32 {
        s.regs.get(&r).copied().unwrap_or(0)
    }

    fn reg64(s: &State, lo: u64, hi: u64) -> u64 {
        u64::from(Self::reg(s, lo)) | (u64::from(Self::reg(s, hi)) << 32)
    }

    fn set64(s: &mut State, lo: u64, hi: u64, v: u64) {
        s.regs.insert(lo, v as u32);
        s.regs.insert(hi, (v >> 32) as u32);
    }

    fn allocate(s: &mut State, size: u64) -> Option<(u64, u64)> {
        let size = size.max(1).div_ceil(s.page_size) * s.page_size;
        let (&off, &len) = s.free.iter().find(|&(_, &len)| len >= size)?;
        s.free.remove(&off);
        if len > size {
            s.free.insert(off + size, len - size);
        }
        s.used.insert(off, size);
        Some((off, size))
    }

    fn deallocate(s: &mut State, off: u64) -> bool {
        let Some(size) = s.used.remove(&off) else { return false };
        let (mut start, mut len) = (off, size);
        // Merge with neighbours.
        if let Some((&poff, &plen)) = s.free.range(..off).next_back()
            && poff + plen == off
        {
            s.free.remove(&poff);
            start = poff;
            len += plen;
        }
        if let Some(&nlen) = s.free.get(&(off + size)) {
            s.free.remove(&(off + size));
            len += nlen;
        }
        s.free.insert(start, len);
        true
    }

    fn command(&self, s: &mut State, cmd: u32) -> u32 {
        match cmd {
            CMD_ALLOCATE_BLOCK => {
                let size = Self::reg64(s, REG_BLOCK_SIZE_LOW, REG_BLOCK_SIZE_HIGH);
                match Self::allocate(s, size) {
                    Some((off, size)) => {
                        Self::set64(s, REG_BLOCK_OFFSET_LOW, REG_BLOCK_OFFSET_HIGH, off);
                        Self::set64(s, REG_BLOCK_SIZE_LOW, REG_BLOCK_SIZE_HIGH, size);
                        0
                    }
                    None => 12, // ENOMEM
                }
            }
            CMD_DEALLOCATE_BLOCK => {
                let off = Self::reg64(s, REG_BLOCK_OFFSET_LOW, REG_BLOCK_OFFSET_HIGH);
                if Self::deallocate(s, off) { 0 } else { 22 }
            }
            CMD_GEN_HANDLE => {
                let h = s.next_handle;
                s.next_handle += 1;
                s.handles.insert(h, (0, None));
                s.regs.insert(REG_HANDLE, h);
                0
            }
            CMD_DESTROY_HANDLE => {
                let h = Self::reg(s, REG_HANDLE);
                s.handles.remove(&h);
                0
            }
            CMD_TELL_PING_INFO_ADDR => {
                let h = Self::reg(s, REG_HANDLE);
                let addr = Self::reg64(s, REG_PING_INFO_ADDR_LOW, REG_PING_INFO_ADDR_HIGH);
                if let Some(e) = s.handles.get_mut(&h) {
                    e.0 = addr;
                }
                0
            }
            _ => 22, // EINVAL
        }
    }

    /// Ping info layout: offset u64 (guest phys), size u64, metadata u64, version u32,
    /// wait_fd u32, wait_flags u32, direction u32, data_size u64.
    fn ping(&self, s: &mut State, handle: u32) {
        let Some(&mut (addr, ref mut kind)) = s.handles.get_mut(&handle) else { return };
        if addr == 0 {
            return;
        }
        let metadata_addr = addr + 16;
        let metadata = self.mem.read_u64(metadata_addr).unwrap_or(u64::MAX);
        let result: u64 = match *kind {
            None => {
                // First ping: choose the context type.
                *kind = Some(metadata);
                0
            }
            // Shared slots: the host picks the place. Allocate returns the block's offset in
            // the area (phys_addr) and its page-rounded size; the guest maps it from there.
            Some(ctx::SHARED_SLOTS_HOST_MEMORY_ALLOCATOR) => match metadata {
                hma::ALLOCATE => {
                    let size = self.mem.read_u64(addr + 8).unwrap_or(0);
                    match Self::allocate(s, size) {
                        Some((off, size)) => {
                            let _ = self.area.zero(self.area.base() + off, size as usize);
                            let _ = self.mem.write(addr, &off.to_le_bytes());
                            let _ = self.mem.write(addr + 8, &size.to_le_bytes());
                            0
                        }
                        None => u64::MAX,
                    }
                }
                hma::UNALLOCATE => {
                    // Offsets are area-relative; accept an absolute address too.
                    let at = self.mem.read_u64(addr).unwrap_or(u64::MAX);
                    let off = if at >= self.area.base() { at - self.area.base() } else { at };
                    if Self::deallocate(s, off) { 0 } else { u64::MAX }
                }
                hma::CHECK_SHARED_SLOTS => 0,
                _ => u64::MAX,
            },
            Some(ctx::HOST_MEMORY_ALLOCATOR) => {
                let phys = self.mem.read_u64(addr).unwrap_or(0);
                let size = self.mem.read_u64(addr + 8).unwrap_or(0);
                let inside = phys >= self.area.base() && phys.saturating_add(size) <= self.area.end();
                match metadata {
                    // The area is already backed; fresh blocks read as zero like stock's.
                    hma::ALLOCATE if inside => {
                        let _ = self.area.zero(phys, size as usize);
                        0
                    }
                    hma::UNALLOCATE if inside => 0,
                    _ => u64::MAX,
                }
            }
            // Other context types (graphics ASG, sensors, power...) aren't provided over this
            // transport; virtio-gpu carries graphics.
            Some(_) => u64::MAX,
        };
        let mut b = [0u8; 8];
        b.copy_from_slice(&result.to_le_bytes());
        let _ = self.mem.write(metadata_addr, &b);
    }
}

impl MmioDevice for GoldfishAddressSpace {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        set_le(data, u64::from(Self::reg(&s, offset)));
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let v = le_value(data) as u32;
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match offset {
            REG_COMMAND => {
                let status = self.command(&mut s, v);
                s.regs.insert(REG_STATUS, status);
            }
            REG_PING | REG_PING_WITH_DATA => self.ping(&mut s, v),
            REG_GUEST_PAGE_SIZE => {
                s.page_size = u64::from(v.max(4096));
                s.regs.insert(offset, v);
            }
            REG_PHYS_START_LOW | REG_PHYS_START_HIGH => {
                s.regs.insert(offset, v);
            }
            _ => {
                s.regs.insert(offset, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM: u64 = 0x4000_0000;
    const AREA: u64 = 0x8_0000_0000;

    fn wr(d: &GoldfishAddressSpace, r: u64, v: u32) {
        d.write(r, &v.to_le_bytes());
    }

    fn rd(d: &GoldfishAddressSpace, r: u64) -> u32 {
        let mut b = [0u8; 4];
        d.read(r, &mut b);
        u32::from_le_bytes(b)
    }

    #[test]
    fn blocks_handles_and_allocator_pings() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, 0x10_0000).expect("area"));
        let d = GoldfishAddressSpace::new(mem.clone(), area.clone());
        wr(&d, REG_BLOCK_SIZE_LOW, 5000);
        wr(&d, REG_COMMAND, CMD_ALLOCATE_BLOCK);
        assert_eq!(rd(&d, REG_STATUS), 0);
        assert_eq!(rd(&d, REG_BLOCK_OFFSET_LOW), 0);
        assert_eq!(rd(&d, REG_BLOCK_SIZE_LOW), 8192, "rounded to pages");
        wr(&d, REG_BLOCK_SIZE_LOW, 4096);
        wr(&d, REG_COMMAND, CMD_ALLOCATE_BLOCK);
        assert_eq!(rd(&d, REG_BLOCK_OFFSET_LOW), 8192);
        wr(&d, REG_BLOCK_OFFSET_LOW, 0);
        wr(&d, REG_COMMAND, CMD_DEALLOCATE_BLOCK);
        assert_eq!(rd(&d, REG_STATUS), 0);
        wr(&d, REG_COMMAND, CMD_DEALLOCATE_BLOCK);
        assert_ne!(rd(&d, REG_STATUS), 0, "double free");

        wr(&d, REG_COMMAND, CMD_GEN_HANDLE);
        let h = rd(&d, REG_HANDLE);
        let ping = RAM + 0x1000;
        wr(&d, REG_PING_INFO_ADDR_LOW, ping as u32);
        wr(&d, REG_PING_INFO_ADDR_HIGH, (ping >> 32) as u32);
        wr(&d, REG_COMMAND, CMD_TELL_PING_INFO_ADDR);
        assert_eq!(rd(&d, REG_PING_INFO_ADDR_LOW), ping as u32, "driver reads it back");
        // Select the host memory allocator context.
        mem.write(ping + 16, &ctx::HOST_MEMORY_ALLOCATOR.to_le_bytes()).expect("w");
        wr(&d, REG_PING, h);
        assert_eq!(mem.read_u64(ping + 16).expect("r"), 0);
        // Allocate host memory at a block inside the area.
        area.write(AREA + 8192, &[0xaa; 16]).expect("w");
        mem.write(ping, &(AREA + 8192).to_le_bytes()).expect("w");
        mem.write(ping + 8, &4096u64.to_le_bytes()).expect("w");
        mem.write(ping + 16, &hma::ALLOCATE.to_le_bytes()).expect("w");
        wr(&d, REG_PING, h);
        assert_eq!(mem.read_u64(ping + 16).expect("r"), 0);
        assert_eq!(area.read_u32(AREA + 8192).expect("r"), 0, "fresh memory is zeroed");
    }

    #[test]
    fn shared_slots_allocator_picks_offsets() {
        let mem = Arc::new(GuestRam::new(RAM, 0x10_0000).expect("ram"));
        let area = Arc::new(GuestRam::new(AREA, 0x10_0000).expect("area"));
        let d = GoldfishAddressSpace::new(mem.clone(), area.clone());
        wr(&d, REG_COMMAND, CMD_GEN_HANDLE);
        let h = rd(&d, REG_HANDLE);
        let ping = RAM + 0x2000;
        wr(&d, REG_PING_INFO_ADDR_LOW, ping as u32);
        wr(&d, REG_COMMAND, CMD_TELL_PING_INFO_ADDR);
        let call = |metadata: u64, phys: u64, size: u64| {
            mem.write(ping, &phys.to_le_bytes()).expect("w");
            mem.write(ping + 8, &size.to_le_bytes()).expect("w");
            mem.write(ping + 16, &metadata.to_le_bytes()).expect("w");
            wr(&d, REG_PING, h);
            (mem.read_u64(ping).expect("r"), mem.read_u64(ping + 8).expect("r"), mem.read_u64(ping + 16).expect("r"))
        };
        assert_eq!(call(ctx::SHARED_SLOTS_HOST_MEMORY_ALLOCATOR, 0, 0).2, 0);
        assert_eq!(call(hma::CHECK_SHARED_SLOTS, 0, 0).2, 0);
        area.write(AREA, &[0xaa; 8]).expect("w");
        assert_eq!(call(hma::ALLOCATE, 0, 100), (0, 4096, 0), "offset 0, one page");
        assert_eq!(area.read_u32(AREA).expect("r"), 0, "zeroed");
        assert_eq!(call(hma::ALLOCATE, 0, 5000), (4096, 8192, 0));
        assert_eq!(call(hma::UNALLOCATE, 0, 0).2, 0);
        assert_eq!(call(hma::UNALLOCATE, AREA + 4096, 0).2, 0, "absolute address");
        assert_eq!(call(hma::UNALLOCATE, 0, 0).2, u64::MAX, "double free");
    }
}
