//! virtio-rng: entropy from the host's `/dev/urandom`.
use std::io::Read;
use std::sync::Arc;

use super::queue::Queue;
use super::{Activation, ID_RNG, INT_USED_RING, Interrupt, VirtioDevice};
use emu_hvf::GuestRam;

pub struct Rng {
    active: Option<(Arc<GuestRam>, Arc<Interrupt>, Queue)>,
    urandom: Option<std::fs::File>,
}

impl Rng {
    pub fn new() -> Rng {
        Rng { active: None, urandom: std::fs::File::open("/dev/urandom").ok() }
    }
}

impl Default for Rng {
    fn default() -> Self {
        Rng::new()
    }
}

impl VirtioDevice for Rng {
    fn device_id(&self) -> u32 {
        ID_RNG
    }
    fn features(&self) -> u64 {
        0
    }
    fn num_queues(&self) -> usize {
        1
    }
    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }
    fn activate(&mut self, act: Activation) -> Result<(), String> {
        let cfg = act.queues.first().copied().flatten().ok_or("rng queue not enabled")?;
        self.active = Some((act.mem, act.irq, Queue::new(cfg, false)));
        Ok(())
    }
    // Requests are small and rare: serve them on the vCPU thread.
    fn notify(&mut self, _queue: u16) {
        let Some((mem, irq, q)) = self.active.as_mut() else { return };
        let mut used = false;
        while let Ok(Some(chain)) = q.pop(mem) {
            let want: usize = chain.writable().map(|d| d.len as usize).sum::<usize>().min(4096);
            let mut buf = vec![0u8; want];
            if let Some(f) = self.urandom.as_mut() {
                let _ = f.read_exact(&mut buf);
            }
            let n = chain.write_all(mem, &buf).unwrap_or(0);
            let _ = q.add_used(mem, chain.head, n);
            used = true;
        }
        if used && q.should_notify(mem) {
            irq.raise(INT_USED_RING);
        }
    }
    fn reset(&mut self) {
        self.active = None;
    }
}
