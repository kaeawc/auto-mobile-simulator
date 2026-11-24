//! Interrupt lines from devices to the interrupt controller.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::sync::Arc;

/// A wire into the interrupt controller. `set(true)` asserts it (level) or produces an edge.
pub trait IrqLine: Send + Sync {
    fn set(&self, level: bool);
}

/// An SPI on the in-kernel GIC.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub struct GicSpi {
    intid: u32,
    wake: Arc<dyn Fn(u32) + Send + Sync>,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
impl GicSpi {
    /// `wake(intid)` runs after asserting, to rouse the vCPU the SPI is routed to.
    pub fn new(intid: u32, wake: Arc<dyn Fn(u32) + Send + Sync>) -> GicSpi {
        GicSpi { intid, wake }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
impl IrqLine for GicSpi {
    fn set(&self, level: bool) {
        if let Err(e) = emu_hvf::Vm::set_spi(self.intid, level) {
            eprintln!("emu: SPI {} -> {level}: {e}", self.intid);
        }
        if level {
            (self.wake)(self.intid);
        }
    }
}

/// Test double that records the line level.
#[cfg(test)]
#[derive(Default)]
pub struct TestLine(pub std::sync::atomic::AtomicBool);

#[cfg(test)]
impl IrqLine for TestLine {
    fn set(&self, level: bool) {
        self.0.store(level, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
impl TestLine {
    pub fn level(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Test double that counts how often the line is driven and how often that changed its level:
/// a device that calls the controller only on changes has `calls() == transitions()`.
#[cfg(test)]
#[derive(Default)]
pub struct CountLine {
    level: std::sync::atomic::AtomicBool,
    calls: std::sync::atomic::AtomicU32,
    transitions: std::sync::atomic::AtomicU32,
}

#[cfg(test)]
impl IrqLine for CountLine {
    fn set(&self, level: bool) {
        use std::sync::atomic::Ordering::SeqCst;
        self.calls.fetch_add(1, SeqCst);
        if self.level.swap(level, SeqCst) != level {
            self.transitions.fetch_add(1, SeqCst);
        }
    }
}

#[cfg(test)]
impl CountLine {
    pub fn level(&self) -> bool {
        self.level.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn calls(&self) -> u32 {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn transitions(&self) -> u32 {
        self.transitions.load(std::sync::atomic::Ordering::SeqCst)
    }
}
