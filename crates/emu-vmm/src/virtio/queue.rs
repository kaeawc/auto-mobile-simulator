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
        self.read_all_into(mem, max, &mut out)?;
        Ok(out)
    }

    /// [`Chain::read_all`] into `out` (cleared first), reusing its allocation: devices keep one
    /// buffer for every request instead of allocating per request.
    pub fn read_all_into(&self, mem: &GuestRam, max: usize, out: &mut Vec<u8>) -> Result<(), QueueError> {
        out.clear();
        let total: usize = self.readable().map(|d| d.len as usize).sum::<usize>().min(max);
        out.reserve(total);
        for d in self.readable() {
            let take = (d.len as usize).min(max - out.len());
            mem.read_append(d.addr, take, out).map_err(|_| QueueError::BadAddress(d.addr))?;
            if out.len() == max {
                break;
            }
        }
        Ok(())
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

    /// [`Chain::write_all`] of `parts` back to back, without joining them first (a packet's
    /// header and its payload's pieces). Returns bytes written.
    pub fn write_parts(&self, mem: &GuestRam, parts: &[&[u8]]) -> Result<u32, QueueError> {
        let mut total = 0u32;
        let mut parts = parts.iter().copied().filter(|p| !p.is_empty());
        let mut part = parts.next();
        for d in self.writable() {
            let (mut addr, mut room) = (d.addr, d.len as usize);
            while room > 0 {
                let Some(p) = part.as_mut() else { return Ok(total) };
                let n = room.min(p.len());
                mem.write(addr, &p[..n]).map_err(|_| QueueError::BadAddress(addr))?;
                (addr, room, total) = (addr + n as u64, room - n, total + n as u32);
                *p = &p[n..];
                if p.is_empty() {
                    part = parts.next();
                }
            }
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
    /// Notifications off while the device drains (`disable_notify`).
    quiet: bool,
    /// The last indirect table read, kept for its allocation.
    indirect: Vec<u8>,
    /// Runs when [`Queue::pop_draining`] finds the ring empty, before it re-enables
    /// notifications: a test driver publishes there to hit the lost-wakeup window exactly.
    #[cfg(test)]
    pub(crate) on_empty: Option<Box<dyn FnMut() + Send>>,
}

const VIRTQ_USED_F_NO_NOTIFY: u16 = 1;

impl Queue {
    pub fn new(cfg: QueueConfig, event_idx: bool) -> Queue {
        Queue {
            cfg,
            next_avail: 0,
            next_used: 0,
            event_idx,
            signalled_used: None,
            quiet: false,
            indirect: Vec::new(),
            #[cfg(test)]
            on_empty: None,
        }
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
        let mut chain = Chain { head: 0, descs: Vec::with_capacity(4) };
        Ok(self.pop_into(mem, &mut chain)?.then_some(chain))
    }

    /// [`Queue::pop`] into `chain` (its descriptors replaced), reusing its allocation: a device
    /// that keeps one `Chain` pops without allocating. Returns whether a chain was popped; on
    /// `false` or an error `chain` holds nothing useful.
    pub fn pop_into(&mut self, mem: &GuestRam, chain: &mut Chain) -> Result<bool, QueueError> {
        chain.descs.clear();
        let size = self.cfg.size;
        if size == 0 {
            return Ok(false);
        }
        let avail_idx = mem.read_u16(self.cfg.avail + 2).map_err(|_| QueueError::BadAddress(self.cfg.avail))?;
        if avail_idx == self.next_avail {
            return Ok(false);
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
        chain.head = head;

        let mut idx = head;
        let mut budget = u32::from(size);
        loop {
            if budget == 0 {
                return Err(QueueError::LoopOrTooLong);
            }
            budget -= 1;
            if idx >= size {
                return Err(QueueError::BadIndex(idx));
            }
            let (addr, len, flags, next) = self.read_desc(mem, self.cfg.desc, idx)?;
            if flags & DESC_F_INDIRECT != 0 {
                self.read_indirect(mem, addr, len, chain)?;
                break;
            }
            chain.descs.push(Desc { addr, len, write: flags & DESC_F_WRITE != 0 });
            if flags & DESC_F_NEXT == 0 {
                break;
            }
            idx = next;
        }
        if self.event_idx && !self.quiet {
            // Ask to be notified when the driver publishes the entry after this one, and order
            // that store before the next pop reads avail.idx (pairs with the driver's barrier
            // between bumping avail.idx and reading avail_event): otherwise both sides can miss
            // the other's store and the driver never kicks for work the device never sees.
            let avail_event = self.cfg.used + 4 + u64::from(size) * 8;
            let _ = mem.write_u16(avail_event, self.next_avail);
            fence(Ordering::SeqCst);
        }
        Ok(true)
    }

    /// Follows an indirect table: copied out of the guest in one read, then walked from entry 0.
    fn read_indirect(&mut self, mem: &GuestRam, table: u64, len: u32, chain: &mut Chain) -> Result<(), QueueError> {
        let n = len / 16;
        if n == 0 || n > 0xffff {
            return Err(QueueError::LoopOrTooLong);
        }
        self.indirect.clear();
        mem.read_append(table, n as usize * 16, &mut self.indirect).map_err(|_| QueueError::BadAddress(table))?;
        let mut idx = 0u32;
        for _ in 0..n {
            if idx >= n {
                return Err(QueueError::BadIndex(idx as u16));
            }
            let b = &self.indirect[idx as usize * 16..idx as usize * 16 + 16];
            let addr = u64::from_le_bytes(b[0..8].try_into().unwrap_or_default());
            let len = u32::from_le_bytes(b[8..12].try_into().unwrap_or_default());
            let flags = u16::from_le_bytes([b[12], b[13]]);
            if flags & DESC_F_INDIRECT != 0 {
                return Err(QueueError::NestedIndirect);
            }
            chain.descs.push(Desc { addr, len, write: flags & DESC_F_WRITE != 0 });
            if flags & DESC_F_NEXT == 0 {
                return Ok(());
            }
            idx = u32::from(u16::from_le_bytes([b[14], b[15]]));
        }
        Err(QueueError::LoopOrTooLong)
    }

    /// Stops asking the driver for notifications while the device drains the queue (QEMU's
    /// `virtio_queue_set_notification(vq, 0)`): `pop` leaves `avail_event` where it is (or sets
    /// `VIRTQ_USED_F_NO_NOTIFY` without event-idx), so requests the driver publishes meanwhile
    /// cost it no kick. Pair every call with [`Queue::enable_notify`] before the device sleeps.
    pub fn disable_notify(&mut self, mem: &GuestRam) {
        if self.quiet {
            return;
        }
        self.quiet = true;
        if !self.event_idx {
            let _ = mem.write_u16(self.cfg.used, VIRTQ_USED_F_NO_NOTIFY);
        }
    }

    /// Asks for a notification at the next published entry again, then rechecks: returns
    /// whether chains are already waiting (the driver may have published them while
    /// notifications were off, without kicking). A device sleeps only on `false`.
    pub fn enable_notify(&mut self, mem: &GuestRam) -> bool {
        self.quiet = false;
        if self.event_idx {
            let _ = mem.write_u16(self.cfg.used + 4 + u64::from(self.cfg.size) * 8, self.next_avail);
        } else {
            let _ = mem.write_u16(self.cfg.used, 0);
        }
        // Publish the request for a kick before looking for work (pairs with the driver's
        // barrier between bumping avail.idx and reading avail_event / used.flags).
        fence(Ordering::SeqCst);
        self.cfg.size != 0 && mem.read_u16(self.cfg.avail + 2).is_ok_and(|i| i != self.next_avail)
    }

    /// [`Queue::pop_into`] for a device that serves a kick by draining the ring, as blk's worker
    /// does: the first call of a drain turns kicks off ([`Queue::disable_notify`]), so the driver
    /// publishing more meanwhile costs it no exit; on an empty ring it turns them back on and
    /// rechecks ([`Queue::enable_notify`]), so an entry published in between is popped here
    /// rather than left waiting for a kick that never comes. `Ok(false)` means the ring is empty
    /// with kicks on: the device may sleep. A device that stops early (an error here, or its own
    /// back-pressure) must call [`Queue::enable_notify`] itself; on `Err` this already has.
    pub fn pop_draining(&mut self, mem: &GuestRam, chain: &mut Chain) -> Result<bool, QueueError> {
        loop {
            self.disable_notify(mem);
            match self.pop_into(mem, chain) {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(e) => {
                    self.enable_notify(mem);
                    return Err(e);
                }
            }
            #[cfg(test)]
            if let Some(f) = self.on_empty.as_mut() {
                f();
            }
            if !self.enable_notify(mem) {
                return Ok(false);
            }
        }
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

        /// Publishes a chain of (addr, len, writable) buffers; returns its head. Descriptors are
        /// reused in order once the table wraps (callers recycle buffers in order).
        pub fn push(&mut self, mem: &GuestRam, bufs: &[(u64, u32, bool)]) -> u16 {
            let head = self.next_desc % self.cfg.size;
            for (i, (addr, len, w)) in bufs.iter().enumerate() {
                let idx = self.next_desc % self.cfg.size;
                self.next_desc = self.next_desc.wrapping_add(1);
                let last = i == bufs.len() - 1;
                let mut flags = if *w { DESC_F_WRITE } else { 0 };
                if !last {
                    flags |= DESC_F_NEXT;
                }
                let mut b = [0u8; 16];
                b[..8].copy_from_slice(&addr.to_le_bytes());
                b[8..12].copy_from_slice(&len.to_le_bytes());
                b[12..14].copy_from_slice(&flags.to_le_bytes());
                b[14..16].copy_from_slice(&((idx + 1) % self.cfg.size).to_le_bytes());
                mem.write(self.cfg.desc + u64::from(idx) * 16, &b).expect("desc");
            }
            let slot = self.cfg.avail + 4 + u64::from(self.avail_idx % self.cfg.size) * 2;
            mem.write_u16(slot, head).expect("ring");
            self.avail_idx = self.avail_idx.wrapping_add(1);
            // Buffers, descriptors and ring entry before the index that publishes them (the
            // driver's virtio_wmb): a device that rechecks the ring without a kick reads them
            // with no other synchronisation.
            fence(Ordering::Release);
            mem.write_u16(self.cfg.avail + 2, self.avail_idx).expect("idx");
            head
        }

        /// Publishes a chain as one indirect descriptor whose table is written at `table`, the way
        /// Linux sends every multi-buffer request once `VIRTIO_RING_F_INDIRECT_DESC` is on.
        pub fn push_indirect(&mut self, mem: &GuestRam, table: u64, bufs: &[(u64, u32, bool)]) -> u16 {
            let mut t = vec![0u8; bufs.len() * 16];
            for (i, ((addr, len, w), b)) in bufs.iter().zip(t.as_chunks_mut::<16>().0).enumerate() {
                let mut flags = if *w { DESC_F_WRITE } else { 0 };
                if i + 1 < bufs.len() {
                    flags |= DESC_F_NEXT;
                }
                b[..8].copy_from_slice(&addr.to_le_bytes());
                b[8..12].copy_from_slice(&len.to_le_bytes());
                b[12..14].copy_from_slice(&flags.to_le_bytes());
                b[14..16].copy_from_slice(&((i + 1) as u16).to_le_bytes());
            }
            mem.write(table, &t).expect("table");
            let idx = self.next_desc % self.cfg.size;
            self.next_desc = self.next_desc.wrapping_add(1);
            let mut d = [0u8; 16];
            d[..8].copy_from_slice(&table.to_le_bytes());
            d[8..12].copy_from_slice(&(t.len() as u32).to_le_bytes());
            d[12..14].copy_from_slice(&DESC_F_INDIRECT.to_le_bytes());
            mem.write(self.cfg.desc + u64::from(idx) * 16, &d).expect("desc");
            let slot = self.cfg.avail + 4 + u64::from(self.avail_idx % self.cfg.size) * 2;
            mem.write_u16(slot, idx).expect("ring");
            self.avail_idx = self.avail_idx.wrapping_add(1);
            // Ring entry before the index that publishes it (the driver's virtio_wmb).
            fence(Ordering::Release);
            mem.write_u16(self.cfg.avail + 2, self.avail_idx).expect("idx");
            idx
        }

        /// Whether the driver must kick after publishing entries `old..avail_idx` (Linux's
        /// `virtqueue_kick_prepare`): the device's `avail_event` under event-idx, else the
        /// `VIRTQ_USED_F_NO_NOTIFY` flag.
        pub fn needs_kick(&self, mem: &GuestRam, old: u16, event_idx: bool) -> bool {
            // The index store must be ordered before reading the device's suppression state.
            fence(Ordering::SeqCst);
            if !event_idx {
                return mem.read_u16(self.cfg.used).expect("used flags") & 1 == 0;
            }
            let event = mem.read_u16(self.cfg.used + 4 + u64::from(self.cfg.size) * 8).expect("avail_event");
            let new = self.avail_idx;
            new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
        }

        /// Linux's `virtqueue_enable_cb` once the driver has read the used ring up to `seen`:
        /// under event-idx, interrupt at the next used entry (`used_event`); else clear
        /// `VIRTQ_AVAIL_F_NO_INTERRUPT`.
        pub fn enable_cb(&self, mem: &GuestRam, seen: u16, event_idx: bool) {
            if event_idx {
                mem.write_u16(self.cfg.avail + 4 + u64::from(self.cfg.size) * 2, seen).expect("used_event");
            } else {
                mem.write_u16(self.cfg.avail, 0).expect("avail flags");
            }
            fence(Ordering::SeqCst);
        }

        pub fn avail_idx(&self) -> u16 {
            self.avail_idx
        }

        pub fn used(&self, mem: &GuestRam, i: u16) -> (u32, u32) {
            let e = self.cfg.used + 4 + u64::from(i) * 8;
            (mem.read_u32(e).expect("id"), mem.read_u32(e + 4).expect("len"))
        }

        pub fn used_idx(&self, mem: &GuestRam) -> u16 {
            let idx = mem.read_u16(self.cfg.used + 2).expect("used idx");
            // Used entries and buffers are read after the index (pairs with add_used's release).
            fence(Ordering::Acquire);
            idx
        }
    }

    #[test]
    fn write_parts_spans_parts_and_buffers() {
        let mem = GuestRam::new(RAM_BASE, 0x10_0000).expect("ram");
        let mut drv = Driver::new(8);
        let mut q = Queue::new(drv.cfg, false);
        let (a, b) = (RAM_BASE + 0x8000, RAM_BASE + 0x9000);
        drv.push(&mem, &[(a, 3, true), (b, 10, true)]);
        let chain = q.pop(&mem).expect("pop").expect("chain");
        assert_eq!(chain.write_parts(&mem, &[b"hd", b"", b"payload", b"xyzw"]), Ok(13), "cut off where the buffers end");
        let mut got = [0u8; 13];
        mem.read(a, &mut got[..3]).expect("a");
        mem.read(b, &mut got[3..]).expect("b");
        assert_eq!(&got, b"hdpayloadxyzw");
        assert_eq!(chain.write_parts(&mem, &[b"ok"]), Ok(2));
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

    /// Device-side cost of popping a Linux-shaped block request: one indirect descriptor whose
    /// table holds a header, 32 data pages and a status byte.
    ///     cargo test --release -p simee-emu-vmm queue_pop_indirect -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn queue_pop_indirect() {
        use std::time::Instant;
        let mem = GuestRam::new(RAM_BASE, 0x100_0000).expect("ram");
        let mut drv = Driver::new(256);
        let mut q = Queue::new(drv.cfg, true);
        let bufs: Vec<(u64, u32, bool)> = std::iter::once((RAM_BASE + 0x10_0000, 16, false))
            .chain((0..32).map(|i| (RAM_BASE + 0x20_0000 + i * 0x2000, 4096, true)))
            .chain(std::iter::once((RAM_BASE + 0x10_0010, 1, true)))
            .collect();
        const N: u32 = 100_000;
        // Tables are written once; the timed loop republishes them (the driver's cost is not ours).
        for i in 0..64u64 {
            drv.push_indirect(&mem, RAM_BASE + 0x8_0000 + i * 0x400, &bufs);
        }
        let (mut fresh, mut reused) = (Vec::new(), Vec::new());
        let mut chain = Chain::default();
        let mut popped = 0u32;
        // Alternating rounds of `pop` (a new Vec per chain) and `pop_into` (one reused Chain).
        for round in 0..14 {
            let mut descs = 0usize;
            let start = Instant::now();
            for _ in 0..N {
                if round % 2 == 0 {
                    let c = q.pop(&mem).expect("pop").expect("chain");
                    descs += c.descs.len();
                    chain.head = c.head;
                } else {
                    assert!(q.pop_into(&mem, &mut chain).expect("pop"));
                    descs += chain.descs.len();
                }
                q.add_used(&mem, chain.head, 0).expect("used");
                // Republish the same head: bump avail.idx over a ring slot that already names it.
                let slot = drv.cfg.avail + 4 + u64::from((popped as u16).wrapping_add(64) % 256) * 2;
                mem.write_u16(slot, chain.head).expect("ring");
                popped += 1;
                mem.write_u16(drv.cfg.avail + 2, (popped as u16).wrapping_add(64)).expect("idx");
            }
            let ns = start.elapsed().as_secs_f64() * 1e9 / f64::from(N);
            assert_eq!(descs, N as usize * 34);
            if round >= 2 {
                if round % 2 == 0 { fresh.push(ns) } else { reused.push(ns) }
            }
        }
        fresh.sort_by(f64::total_cmp);
        reused.sort_by(f64::total_cmp);
        println!(
            "queue pop: 34-desc indirect chain: pop {:>6.1} ns, pop_into {:>6.1} ns (median round)",
            fresh[fresh.len() / 2],
            reused[reused.len() / 2]
        );
    }

    #[test]
    fn pop_into_reuses_the_chain_and_matches_pop() {
        let mem = GuestRam::new(RAM_BASE, 0x10_0000).expect("ram");
        let mut drv = Driver::new(8);
        let mut q = Queue::new(drv.cfg, true);
        let mut chain = Chain::default();
        assert_eq!(q.pop_into(&mem, &mut chain), Ok(false));
        let bufs: Vec<(u64, u32, bool)> = (0..20).map(|i| (RAM_BASE + 0x8000 + i * 0x100, 16, i > 0)).collect();
        let h1 = drv.push_indirect(&mem, RAM_BASE + 0x6000, &bufs);
        let h2 = drv.push(&mem, &[(RAM_BASE + 0x9000, 4, false)]);
        assert_eq!(q.pop_into(&mem, &mut chain), Ok(true));
        assert_eq!(chain.head, h1);
        let want: Vec<Desc> = bufs.iter().map(|&(addr, len, write)| Desc { addr, len, write }).collect();
        assert_eq!(chain.descs, want);
        let cap = chain.descs.capacity();
        assert_eq!(q.pop_into(&mem, &mut chain), Ok(true));
        assert_eq!((chain.head, chain.descs.as_slice()), (h2, &[Desc { addr: RAM_BASE + 0x9000, len: 4, write: false }][..]));
        assert_eq!(chain.descs.capacity(), cap, "allocation kept");
        assert_eq!(q.pop_into(&mem, &mut chain), Ok(false));
    }

    #[test]
    fn indirect_tables_are_bounds_checked() {
        let mem = GuestRam::new(RAM_BASE, 0x10_0000).expect("ram");
        let mut drv = Driver::new(4);
        let mut q = Queue::new(drv.cfg, false);
        let table = RAM_BASE + 0x6000;
        drv.push_indirect(&mem, table, &[(RAM_BASE + 0x8000, 4, false), (RAM_BASE + 0x8100, 4, true)]);
        mem.write_u16(table + 14, 2).expect("next past the table");
        assert_eq!(q.pop(&mem).err(), Some(QueueError::BadIndex(2)));
        drv.push_indirect(&mem, table, &[(RAM_BASE + 0x8000, 4, false), (RAM_BASE + 0x8100, 4, true)]);
        mem.write_u16(table + 16 + 14, 0).expect("loop back");
        mem.write_u16(table + 16 + 12, DESC_F_NEXT).expect("flags");
        assert_eq!(q.pop(&mem).err(), Some(QueueError::LoopOrTooLong));
        drv.push_indirect(&mem, table, &[(RAM_BASE + 0x8000, 4, false)]);
        mem.write_u16(table + 12, DESC_F_INDIRECT).expect("nested");
        assert_eq!(q.pop(&mem).err(), Some(QueueError::NestedIndirect));
        drv.push_indirect(&mem, RAM_BASE + 0x10_0000 - 8, &[]);
        assert_eq!(q.pop(&mem).err(), Some(QueueError::LoopOrTooLong), "empty table");
    }

    /// Kicks are off while the device drains; turning them back on asks for the next entry and
    /// reports entries published meanwhile, so the device serves them instead of sleeping.
    #[test]
    fn notifications_off_while_draining_then_recheck() {
        for event_idx in [true, false] {
            let mem = GuestRam::new(RAM_BASE, 0x10_0000).expect("ram");
            let mut drv = Driver::new(8);
            let mut q = Queue::new(drv.cfg, event_idx);
            let buf = (RAM_BASE + 0x9000, 1, true);
            assert!(!q.enable_notify(&mem), "nothing published");
            let old = drv.avail_idx();
            drv.push(&mem, &[buf]);
            assert!(drv.needs_kick(&mem, old, event_idx), "an idle device wants a kick");
            q.disable_notify(&mem);
            assert!(q.pop(&mem).expect("pop").is_some());
            let old = drv.avail_idx();
            drv.push(&mem, &[buf]);
            assert!(!drv.needs_kick(&mem, old, event_idx), "event_idx={event_idx}: no kick while draining");
            assert!(q.pop(&mem).expect("pop").is_some());
            assert!(q.pop(&mem).expect("pop").is_none());
            // Published after the device's last pop, before it re-enables: no kick, so the
            // recheck must find it.
            let old = drv.avail_idx();
            drv.push(&mem, &[buf]);
            assert!(!drv.needs_kick(&mem, old, event_idx));
            assert!(q.enable_notify(&mem), "event_idx={event_idx}: recheck sees the entry");
            assert!(q.pop(&mem).expect("pop").is_some());
            assert!(!q.enable_notify(&mem));
            let old = drv.avail_idx();
            drv.push(&mem, &[buf]);
            assert!(drv.needs_kick(&mem, old, event_idx), "event_idx={event_idx}: kicks back on");
        }
    }

    /// A burst the driver publishes while the device drains costs one kick, not one per entry,
    /// in both suppression modes; and once the drain ends kicks are back on.
    #[test]
    fn a_burst_published_while_draining_costs_one_kick() {
        const N: u16 = 32;
        for event_idx in [true, false] {
            let mem = GuestRam::new(RAM_BASE, 0x10_0000).expect("ram");
            let mut drv = Driver::new(64);
            let mut q = Queue::new(drv.cfg, event_idx);
            let buf = (RAM_BASE + 0x9000, 1, true);
            let publish = |drv: &mut Driver| {
                let old = drv.avail_idx();
                drv.push(&mem, &[buf]);
                u32::from(drv.needs_kick(&mem, old, event_idx))
            };
            let mut kicks = publish(&mut drv);
            let mut chain = Chain::default();
            let mut served = 0;
            // The device serves one entry per step; the driver publishes the rest meanwhile.
            while q.pop_draining(&mem, &mut chain).expect("pop") {
                served += 1;
                if served < N {
                    kicks += publish(&mut drv);
                }
            }
            assert_eq!((served, kicks), (N, 1), "event_idx={event_idx}");
            kicks += publish(&mut drv);
            assert_eq!(kicks, 2, "event_idx={event_idx}: an idle device is kicked again");
        }
    }

    /// The lost-wakeup window: the driver publishes after the device's last pop came back empty
    /// but before it re-enabled kicks, so the driver sees kicks off and doesn't kick. The
    /// recheck after re-enabling (ordered by its SeqCst fence) still serves the entry.
    #[test]
    fn an_entry_published_as_kicks_come_back_on_is_still_served() {
        use std::sync::{Arc, Mutex};
        for event_idx in [true, false] {
            let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
            let drv = Arc::new(Mutex::new(Driver::new(8)));
            let mut q = Queue::new(drv.lock().expect("drv").cfg, event_idx);
            let buf = (RAM_BASE + 0x9000, 1, true);
            drv.lock().expect("drv").push(&mem, &[buf]);
            let missed = Arc::new(Mutex::new(Vec::new()));
            let (m, d, missed2) = (mem.clone(), drv.clone(), missed.clone());
            let mut fired = false;
            q.on_empty = Some(Box::new(move || {
                if !std::mem::replace(&mut fired, true) {
                    let mut d = d.lock().expect("drv");
                    let old = d.avail_idx();
                    d.push(&m, &[buf]);
                    missed2.lock().expect("missed").push(!d.needs_kick(&m, old, event_idx));
                }
            }));
            let mut chain = Chain::default();
            let mut served = 0;
            while q.pop_draining(&mem, &mut chain).expect("pop") {
                served += 1;
            }
            assert_eq!(*missed.lock().expect("missed"), [true], "event_idx={event_idx}: the driver skipped its kick");
            assert_eq!(served, 2, "event_idx={event_idx}: the recheck found the entry");
        }
    }

    /// A driver thread and a device thread race on avail.idx and the suppression state, the
    /// driver kicking only when `virtqueue_kick_prepare` would: every entry is served.
    #[test]
    fn a_racing_driver_never_loses_a_wakeup() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU16;
        use std::time::{Duration, Instant};
        const N: u32 = 20_000;
        for event_idx in [true, false] {
            let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
            let mut drv = Driver::new(16);
            let mut q = Queue::new(drv.cfg, event_idx);
            let bell = Arc::new(super::super::Doorbell::default());
            let served = Arc::new(AtomicU16::new(0));
            let (m, b, s) = (mem.clone(), bell.clone(), served.clone());
            let device = std::thread::spawn(move || {
                let mut chain = Chain::default();
                while b.wait() {
                    while q.pop_draining(&m, &mut chain).expect("pop") {
                        q.add_used(&m, chain.head, 0).expect("used");
                        s.fetch_add(1, Ordering::SeqCst);
                    }
                }
            });
            let (mut sent, mut kicks, mut last) = (0u32, 0u32, (0u16, Instant::now()));
            while sent < N || served.load(Ordering::SeqCst) != N as u16 {
                let done = served.load(Ordering::SeqCst);
                if done != last.0 {
                    last = (done, Instant::now());
                }
                assert!(last.1.elapsed() < Duration::from_secs(10), "event_idx={event_idx}: lost wakeup after {sent} sent");
                if sent < N && drv.avail_idx().wrapping_sub(done) < 4 {
                    let old = drv.avail_idx();
                    drv.push(&mem, &[(RAM_BASE + 0x9000, 1, true)]);
                    sent += 1;
                    if drv.needs_kick(&mem, old, event_idx) {
                        kicks += 1;
                        bell.ring();
                    }
                } else {
                    std::hint::spin_loop();
                }
            }
            bell.stop();
            device.join().expect("device");
            assert!(kicks <= N);
        }
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
