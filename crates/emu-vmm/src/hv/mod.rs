//! The hypervisor seam: everything the machine needs from the host's hypervisor, one backend
//! per host, selected at build time. Each backend module exports the same items:
//!
//! - `Hypervisor`: `new(cpus)`, `map_ram(&GuestRam)`, `create_gic(dist, redist) -> GicInfo`,
//!   `spi(intid) -> Arc<dyn IrqLine>` (a device's interrupt line), `serve_notifies` (virtio
//!   QueueNotify stores the hypervisor can complete without a userspace exit), and
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
#[cfg(hv_hvf)]
mod hvf;
#[cfg(hv_hvf)]
pub use hvf::*;
#[cfg(hv_kvm)]
mod kvm;
#[cfg(hv_kvm)]
pub use kvm::*;

use std::sync::Arc;

/// A guest store the hypervisor may complete in the kernel: `value` (4 bytes) to `addr`, after
/// which `on` runs on a host thread. A virtio-mmio QueueNotify: the slot's `0x50` and the queue
/// index. Backends that can't (Hypervisor.framework) leave the store to the MMIO path, which
/// reaches the same transport.
pub struct QueueNotify {
    pub addr: u64,
    pub value: u32,
    pub on: Arc<dyn Fn() + Send + Sync>,
}

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
