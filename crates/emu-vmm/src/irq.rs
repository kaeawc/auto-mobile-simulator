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
    wake: Arc<dyn Fn() + Send + Sync>,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
impl GicSpi {
    /// `wake` runs after asserting, to rouse vCPUs parked in a host-side WFI wait.
    pub fn new(intid: u32, wake: Arc<dyn Fn() + Send + Sync>) -> GicSpi {
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
            (self.wake)();
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
