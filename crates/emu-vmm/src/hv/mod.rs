//! The hypervisor seam: everything the machine needs from the host's hypervisor, one backend
//! per host, selected at build time. Each backend module exports the same items:
//!
//! - `Hypervisor`: `new(cpus)`, `map_ram(&GuestRam)`, `create_gic(dist, redist) -> GicInfo`,
//!   `spi(intid) -> Arc<dyn IrqLine>` (a device's interrupt line), and
//!   `start_vcpus(&Arc<Self>, &Arc<Cpus>, entry, dtb)`, which runs the vCPU threads until the
//!   machine stops. It implements [`crate::vcpu::ForceExit`], which [`crate::vcpu::Cpus`] uses to
//!   stop them.
//! - `map_raw(HostRange, ipa)` / `unmap_raw(ipa, len)`: host memory the VMM doesn't own (the GPU
//!   renderer's host-visible blobs) mapped into the guest from any thread.
//! - `Error` and `HostRange`.
//!
//! The seam sits above the vCPU run loop, not at the raw vCPU calls: hypervisors differ in what
//! they handle in the kernel (Hypervisor.framework leaves WFI wakeups and PSCI to the VMM and
//! reports MMIO as a raw syndrome), so each backend owns its loop and shares the device
//! dispatch, exit counters and stop protocol in [`crate::vcpu`].
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod hvf;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use hvf::*;

/// Addresses and sizes of the interrupt controller (GICv3) once created.
#[derive(Clone, Copy, Debug)]
pub struct GicInfo {
    pub dist_base: u64,
    pub dist_size: u64,
    pub redist_base: u64,
    /// Size of the whole redistributor region the backend allows (all vCPUs).
    pub redist_region_size: u64,
    /// First valid SPI INTID and how many follow it.
    pub spi_base: u32,
    pub spi_count: u32,
}
