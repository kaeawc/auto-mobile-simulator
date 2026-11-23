//! virtio devices over the MMIO transport.
pub mod blk;
pub mod console;
pub mod gpu;
pub mod input;
pub mod mmio;
pub mod queue;
pub mod rng;
pub mod vsock;

use std::sync::{Arc, Mutex};

use emu_hvf::GuestRam;

use crate::irq::IrqLine;
use queue::QueueConfig;

pub const ID_NET: u32 = 1;
pub const ID_BLOCK: u32 = 2;
pub const ID_RNG: u32 = 4;

/// What a device receives when the driver sets DRIVER_OK.
pub struct Activation {
    pub mem: Arc<GuestRam>,
    pub irq: Arc<Interrupt>,
    /// One entry per queue; `None` for queues the driver left disabled.
    pub queues: Vec<Option<QueueConfig>>,
    pub features: u64,
}

/// A virtio device model. The transport owns it behind a lock; devices that do real work move
/// their queues to a worker thread in `activate` and have `notify` wake it.
pub trait VirtioDevice: Send {
    fn device_id(&self) -> u32;
    /// Device feature bits (the transport adds VIRTIO_F_VERSION_1).
    fn features(&self) -> u64;
    fn num_queues(&self) -> usize;
    fn queue_max_size(&self) -> u16 {
        256
    }
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    fn activate(&mut self, act: Activation) -> Result<(), String>;
    fn notify(&mut self, queue: u16);
    /// Driver reset: stop workers and forget queue state.
    fn reset(&mut self);
    /// Shared memory region `id` as (guest physical base, length), if the device has one.
    fn shm_region(&self, _id: u8) -> Option<(u64, u64)> {
        None
    }
}

/// The transport's interrupt status register and the level-triggered line it drives.
///
/// The device tree marks virtio-mmio interrupts level-high: the line stays asserted while any
/// status bit is set, so an interrupt raised while the driver is mid-handler is never lost.
pub struct Interrupt {
    status: Mutex<u32>,
    line: Arc<dyn IrqLine>,
}

pub const INT_USED_RING: u32 = 1;
pub const INT_CONFIG: u32 = 2;

impl Interrupt {
    pub fn new(line: Arc<dyn IrqLine>) -> Interrupt {
        Interrupt { status: Mutex::new(0), line }
    }

    pub fn status(&self) -> u32 {
        *self.status.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn raise(&self, bits: u32) {
        let mut s = self.status.lock().unwrap_or_else(|p| p.into_inner());
        let was = *s;
        *s |= bits;
        if was == 0 {
            self.line.set(true);
        }
    }

    pub fn ack(&self, bits: u32) {
        let mut s = self.status.lock().unwrap_or_else(|p| p.into_inner());
        let was = *s;
        *s &= !bits;
        if was != 0 && *s == 0 {
            self.line.set(false);
        }
    }
}

/// Wakeup for a device worker thread: notifications coalesce into one pending flag.
#[derive(Default)]
pub struct Doorbell {
    state: Mutex<(bool, bool)>, // (pending, stop)
    cv: std::sync::Condvar,
}

impl Doorbell {
    pub fn ring(&self) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !s.0 {
            s.0 = true;
            self.cv.notify_one();
        }
    }

    pub fn stop(&self) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        s.1 = true;
        self.cv.notify_one();
    }

    /// Blocks until rung (true) or stopped (false).
    pub fn wait(&self) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if s.1 {
                return false;
            }
            if s.0 {
                s.0 = false;
                return true;
            }
            s = self.cv.wait(s).unwrap_or_else(|p| p.into_inner());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::TestLine;

    #[test]
    fn interrupt_is_level() {
        let line = Arc::new(TestLine::default());
        let irq = Interrupt::new(line.clone());
        irq.raise(INT_USED_RING);
        assert!(line.level());
        irq.raise(INT_CONFIG);
        irq.ack(INT_USED_RING);
        assert!(line.level(), "config bit still set");
        irq.ack(INT_CONFIG);
        assert!(!line.level());
        assert_eq!(irq.status(), 0);
    }

    #[test]
    fn doorbell_coalesces_and_stops() {
        let d = Arc::new(Doorbell::default());
        d.ring();
        d.ring();
        assert!(d.wait());
        let d2 = d.clone();
        let t = std::thread::spawn(move || d2.wait());
        d.stop();
        assert!(!t.join().expect("join"));
    }
}
