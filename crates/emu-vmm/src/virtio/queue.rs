//! Split virtqueue (virtio 1.x, section 2.7).
use std::sync::atomic::{Ordering, fence};

use emu_hvf::GuestRam;

pub const DESC_F_NEXT: u16 = 1;
pub const DESC_F_WRITE: u16 = 2;
pub const DESC_F_INDIRECT: u16 = 4;

/// Driver-configured queue addresses and size (from the transport registers).
#[derive(Clone, Copy, Debug, Default)]
pub struct QueueConfig {
    pub size: u16,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
}

/// One buffer of a descriptor chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    /// Device-writable (guest reads the result).
    pub write: bool,
}

/// A popped request: its head index (to return in the used ring) and its buffers in order.
#[derive(Debug, Default)]
pub struct Chain {
    pub head: u16,
    pub descs: Vec<Desc>,
}

impl Chain {
    pub fn readable(&self) -> impl Iterator<Item = &Desc> {
        self.descs.iter().filter(|d| !d.write)
    }

    pub fn writable(&self) -> impl Iterator<Item = &Desc> {
        self.descs.iter().filter(|d| d.write)
    }

    /// Copies the device-readable part of the chain into a buffer, up to `max` bytes.
    pub fn read_all(&self, mem: &GuestRam, max: usize) -> Result<Vec<u8>, QueueError> {
        let mut out = Vec::new();
        for d in self.readable() {
            let take = (d.len as usize).min(max - out.len());
            let start = out.len();
            out.resize(start + take, 0);
            mem.read(d.addr, &mut out[start..]).map_err(|_| QueueError::BadAddress(d.addr))?;
            if out.len() == max {
                break;
            }
        }
        Ok(out)
    }

    /// Copies `data` into the device-writable buffers; returns bytes written.
    pub fn write_all(&self, mem: &GuestRam, mut data: &[u8]) -> Result<u32, QueueError> {
        let mut total = 0u32;
        for d in self.writable() {
            if data.is_empty() {
                break;
            }
            let n = (d.len as usize).min(data.len());
            mem.write(d.addr, &data[..n]).map_err(|_| QueueError::BadAddress(d.addr))?;
            data = &data[n..];
            total += n as u32;
        }
        Ok(total)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum QueueError {
    BadAddress(u64),
    BadIndex(u16),
    LoopOrTooLong,
    NestedIndirect,
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueError::BadAddress(a) => write!(f, "descriptor points outside guest RAM: {a:#x}"),
            QueueError::BadIndex(i) => write!(f, "descriptor index {i} out of range"),
            QueueError::LoopOrTooLong => write!(f, "descriptor chain loops or is too long"),
            QueueError::NestedIndirect => write!(f, "nested indirect descriptor table"),
        }
    }
}

impl std::error::Error for QueueError {}

/// Device side of one split virtqueue.
pub struct Queue {
    cfg: QueueConfig,
    next_avail: u16,
    next_used: u16,
    event_idx: bool,
    /// `used.idx` at the last interrupt, for event-idx suppression.
    signalled_used: Option<u16>,
}

impl Queue {
    pub fn new(cfg: QueueConfig, event_idx: bool) -> Queue {
        Queue { cfg, next_avail: 0, next_used: 0, event_idx, signalled_used: None }
    }

    pub fn size(&self) -> u16 {
        self.cfg.size
    }

    fn read_desc(&self, mem: &GuestRam, table: u64, idx: u16) -> Result<(u64, u32, u16, u16), QueueError> {
        let base = table + u64::from(idx) * 16;
        let mut b = [0u8; 16];
        mem.read(base, &mut b).map_err(|_| QueueError::BadAddress(base))?;
        let addr = u64::from_le_bytes(b[0..8].try_into().unwrap_or_default());
        let len = u32::from_le_bytes(b[8..12].try_into().unwrap_or_default());
        let flags = u16::from_le_bytes([b[12], b[13]]);
        let next = u16::from_le_bytes([b[14], b[15]]);
        Ok((addr, len, flags, next))
    }

    /// Pops the next available chain, if the driver has published one.
    pub fn pop(&mut self, mem: &GuestRam) -> Result<Option<Chain>, QueueError> {
        let size = self.cfg.size;
        if size == 0 {
            return Ok(None);
        }
        let avail_idx = mem.read_u16(self.cfg.avail + 2).map_err(|_| QueueError::BadAddress(self.cfg.avail))?;
        if avail_idx == self.next_avail {
            return Ok(None);
        }
        // Read ring entries and descriptors only after observing the index (pairs with the
        // driver's write barrier before it bumps avail.idx).
        fence(Ordering::Acquire);
        let slot = self.cfg.avail + 4 + u64::from(self.next_avail % size) * 2;
        let head = mem.read_u16(slot).map_err(|_| QueueError::BadAddress(slot))?;
        self.next_avail = self.next_avail.wrapping_add(1);
        if head >= size {
            return Err(QueueError::BadIndex(head));
        }

        let mut chain = Chain { head, descs: Vec::with_capacity(4) };
        let (mut table, mut table_len, mut idx) = (self.cfg.desc, size, head);
        let mut in_indirect = false;
        let mut budget = u32::from(size);
        loop {
            if budget == 0 {
                return Err(QueueError::LoopOrTooLong);
            }
            budget -= 1;
            if idx >= table_len {
                return Err(QueueError::BadIndex(idx));
            }
            let (addr, len, flags, next) = self.read_desc(mem, table, idx)?;
            if flags & DESC_F_INDIRECT != 0 {
                if in_indirect {
                    return Err(QueueError::NestedIndirect);
                }
                let n = len / 16;
                if n == 0 || n > 0xffff {
                    return Err(QueueError::LoopOrTooLong);
                }
                (table, table_len, idx) = (addr, n as u16, 0);
                in_indirect = true;
                budget = n;
                continue;
            }
            chain.descs.push(Desc { addr, len, write: flags & DESC_F_WRITE != 0 });
            if flags & DESC_F_NEXT == 0 {
                break;
            }
            idx = next;
        }
        if self.event_idx {
            // Ask to be notified when the driver publishes the entry after this one.
            let avail_event = self.cfg.used + 4 + u64::from(size) * 8;
            let _ = mem.write_u16(avail_event, self.next_avail);
        }
        Ok(Some(chain))
    }

    /// Returns a chain to the driver with `len` bytes written into its buffers.
    pub fn add_used(&mut self, mem: &GuestRam, head: u16, len: u32) -> Result<(), QueueError> {
        let size = self.cfg.size;
        let elem = self.cfg.used + 4 + u64::from(self.next_used % size) * 8;
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&u32::from(head).to_le_bytes());
        b[4..].copy_from_slice(&len.to_le_bytes());
        mem.write(elem, &b).map_err(|_| QueueError::BadAddress(elem))?;
        self.next_used = self.next_used.wrapping_add(1);
        // The element must be visible before the index that publishes it.
        fence(Ordering::Release);
        mem.write_u16(self.cfg.used + 2, self.next_used).map_err(|_| QueueError::BadAddress(self.cfg.used))?;
        Ok(())
    }

    /// Whether the driver wants an interrupt for the used entries added since the last one.
    pub fn should_notify(&mut self, mem: &GuestRam) -> bool {
        // Our used.idx store must be ordered before reading the driver's suppression state.
        fence(Ordering::SeqCst);
        let new = self.next_used;
        if !self.event_idx {
            let flags = mem.read_u16(self.cfg.avail).unwrap_or(0);
            return flags & 1 == 0; // VIRTQ_AVAIL_F_NO_INTERRUPT
        }
        let used_event_addr = self.cfg.avail + 4 + u64::from(self.cfg.size) * 2;
        let used_event = mem.read_u16(used_event_addr).unwrap_or(0);
        let old = self.signalled_used.replace(new);
        match old {
            None => true,
            // vring_need_event(): notify if used_event lies in (old, new].
            Some(old) => new.wrapping_sub(used_event).wrapping_sub(1) < new.wrapping_sub(old),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const RAM_BASE: u64 = 0x4000_0000;

    /// Driver-side helper that lays out a queue in guest RAM.
    pub struct Driver {
        pub cfg: QueueConfig,
        next_desc: u16,
        avail_idx: u16,
    }

    impl Driver {
        pub fn new(size: u16) -> Driver {
            Driver::at(RAM_BASE + 0x1000, size)
        }

        /// A queue whose rings start at `desc` (devices with several queues need each apart).
        pub fn at(desc: u64, size: u16) -> Driver {
            let avail = desc + u64::from(size) * 16;
            let used = (avail + 6 + u64::from(size) * 2 + 3) & !3;
            Driver { cfg: QueueConfig { size, desc, avail, used }, next_desc: 0, avail_idx: 0 }
        }

        /// Publishes a chain of (addr, len, writable) buffers; returns its head.
        pub fn push(&mut self, mem: &GuestRam, bufs: &[(u64, u32, bool)]) -> u16 {
            let head = self.next_desc;
            for (i, (addr, len, w)) in bufs.iter().enumerate() {
                let idx = self.next_desc;
                self.next_desc += 1;
                let last = i == bufs.len() - 1;
                let mut flags = if *w { DESC_F_WRITE } else { 0 };
                if !last {
                    flags |= DESC_F_NEXT;
                }
                let mut b = [0u8; 16];
                b[..8].copy_from_slice(&addr.to_le_bytes());
                b[8..12].copy_from_slice(&len.to_le_bytes());
                b[12..14].copy_from_slice(&flags.to_le_bytes());
                b[14..16].copy_from_slice(&(idx + 1).to_le_bytes());
                mem.write(self.cfg.desc + u64::from(idx) * 16, &b).expect("desc");
            }
            let slot = self.cfg.avail + 4 + u64::from(self.avail_idx % self.cfg.size) * 2;
            mem.write_u16(slot, head).expect("ring");
            self.avail_idx += 1;
            mem.write_u16(self.cfg.avail + 2, self.avail_idx).expect("idx");
            head
        }

        pub fn used(&self, mem: &GuestRam, i: u16) -> (u32, u32) {
            let e = self.cfg.used + 4 + u64::from(i) * 8;
            (mem.read_u32(e).expect("id"), mem.read_u32(e + 4).expect("len"))
        }

        pub fn used_idx(&self, mem: &GuestRam) -> u16 {
            mem.read_u16(self.cfg.used + 2).expect("used idx")
        }
    }

    #[test]
    fn pop_and_complete() {
        let mem = GuestRam::new(RAM_BASE, 0x10_0000).expect("ram");
        let mut drv = Driver::new(8);
        let mut q = Queue::new(drv.cfg, false);
        assert!(q.pop(&mem).expect("pop").is_none());
        mem.write(RAM_BASE + 0x8000, b"ping").expect("w");
        let head = drv.push(&mem, &[(RAM_BASE + 0x8000, 4, false), (RAM_BASE + 0x9000, 16, true)]);
        let chain = q.pop(&mem).expect("pop").expect("chain");
        assert_eq!(chain.head, head);
        assert_eq!(chain.read_all(&mem, 64).expect("read"), b"ping");
        assert_eq!(chain.write_all(&mem, b"pong").expect("write"), 4);
        q.add_used(&mem, chain.head, 4).expect("used");
        assert_eq!(drv.used_idx(&mem), 1);
        assert_eq!(drv.used(&mem, 0), (u32::from(head), 4));
        assert!(q.should_notify(&mem));
        assert!(q.pop(&mem).expect("pop").is_none());
    }

    #[test]
    fn follows_indirect_tables() {
        let mem = GuestRam::new(RAM_BASE, 0x10_0000).expect("ram");
        let drv = Driver::new(4);
        let mut q = Queue::new(drv.cfg, false);
        let table = RAM_BASE + 0x6000;
        for (i, (addr, len, flags)) in [(0x7000u64, 16u32, DESC_F_NEXT), (0x7100, 1, DESC_F_WRITE)].iter().enumerate() {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&(RAM_BASE + addr).to_le_bytes());
            b[8..12].copy_from_slice(&len.to_le_bytes());
            b[12..14].copy_from_slice(&flags.to_le_bytes());
            b[14..16].copy_from_slice(&((i + 1) as u16).to_le_bytes());
            mem.write(table + i as u64 * 16, &b).expect("w");
        }
        let mut d = [0u8; 16];
        d[..8].copy_from_slice(&table.to_le_bytes());
        d[8..12].copy_from_slice(&32u32.to_le_bytes());
        d[12..14].copy_from_slice(&DESC_F_INDIRECT.to_le_bytes());
        mem.write(drv.cfg.desc, &d).expect("w");
        mem.write_u16(drv.cfg.avail + 4, 0).expect("w");
        mem.write_u16(drv.cfg.avail + 2, 1).expect("w");
        let chain = q.pop(&mem).expect("pop").expect("chain");
        assert_eq!(chain.descs.len(), 2);
        assert!(chain.descs[1].write);
    }

    #[test]
    fn rejects_loops() {
        let mem = GuestRam::new(RAM_BASE, 0x10_0000).expect("ram");
        let drv = Driver::new(4);
        let mut q = Queue::new(drv.cfg, false);
        let mut d = [0u8; 16];
        d[..8].copy_from_slice(&(RAM_BASE + 0x8000).to_le_bytes());
        d[8..12].copy_from_slice(&4u32.to_le_bytes());
        d[12..14].copy_from_slice(&DESC_F_NEXT.to_le_bytes());
        d[14..16].copy_from_slice(&0u16.to_le_bytes()); // points at itself
        mem.write(drv.cfg.desc, &d).expect("w");
        mem.write_u16(drv.cfg.avail + 2, 1).expect("w");
        assert_eq!(q.pop(&mem).err(), Some(QueueError::LoopOrTooLong));
    }

    #[test]
    fn event_idx_suppression() {
        let mem = GuestRam::new(RAM_BASE, 0x10_0000).expect("ram");
        let mut drv = Driver::new(8);
        let mut q = Queue::new(drv.cfg, true);
        let used_event = drv.cfg.avail + 4 + 8 * 2;
        for _ in 0..3 {
            drv.push(&mem, &[(RAM_BASE + 0x9000, 1, true)]);
        }
        mem.write_u16(used_event, 0).expect("w");
        let c = q.pop(&mem).expect("pop").expect("chain");
        q.add_used(&mem, c.head, 1).expect("used");
        assert!(q.should_notify(&mem), "first completion always notifies");
        // Driver says: interrupt me only once used.idx passes 2.
        mem.write_u16(used_event, 2).expect("w");
        let c = q.pop(&mem).expect("pop").expect("chain");
        q.add_used(&mem, c.head, 1).expect("used");
        assert!(!q.should_notify(&mem), "used.idx=2 does not pass used_event=2");
        let c = q.pop(&mem).expect("pop").expect("chain");
        q.add_used(&mem, c.head, 1).expect("used");
        assert!(q.should_notify(&mem), "used.idx=3 passes used_event=2");
    }
}
