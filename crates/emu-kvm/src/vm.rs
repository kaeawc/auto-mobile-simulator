//! The VM: guest memory slots, interrupt lines, ioeventfds and the in-kernel GIC.
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::Mutex;

use emu_hvf::{GuestRam, HostRange};
use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::{IoEventAddress, NoDatamatch, VmFd};
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

use crate::{Error, Result, Vcpu, kvm_err};

/// Memory slots in use: `(slot, guest address, length)`.
struct Slots {
    used: Vec<(u32, u64, u64)>,
    limit: u32,
}

impl Slots {
    /// The lowest free slot number for `[gpa, gpa + len)`, which must not overlap a used slot.
    fn claim(&mut self, gpa: u64, len: u64) -> Result<u32> {
        if len == 0 || gpa.checked_add(len).is_none() {
            return Err(Error::Invalid("a memory slot needs a non-empty range inside the address space"));
        }
        if self.used.iter().any(|&(_, g, l)| gpa < g + l && g < gpa + len) {
            return Err(Error::Invalid("guest memory ranges overlap"));
        }
        let slot = (0..self.limit).find(|s| self.used.iter().all(|&(u, _, _)| u != *s)).ok_or(Error::Invalid("out of KVM memory slots"))?;
        self.used.push((slot, gpa, len));
        Ok(slot)
    }

    /// Frees the slot mapped exactly at `[gpa, gpa + len)`.
    fn release(&mut self, gpa: u64, len: u64) -> Option<u32> {
        let i = self.used.iter().position(|&(_, g, l)| g == gpa && l == len)?;
        Some(self.used.swap_remove(i).0)
    }
}

/// A KVM virtual machine. Several can exist in one process; emu runs one.
pub struct Vm {
    pub(crate) fd: VmFd,
    slots: Mutex<Slots>,
}

impl Vm {
    pub(crate) fn new(fd: VmFd, memslots: usize) -> Vm {
        Vm { fd, slots: Mutex::new(Slots { used: Vec::new(), limit: u32::try_from(memslots).unwrap_or(u32::MAX).max(1) }) }
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Maps `ram` into the guest at its base address, readable, writable and executable, with
    /// dirty logging off. The caller keeps `ram` alive as long as the VM (the machine holds an
    /// `Arc`). Pages `ram` gives back to the host ([`GuestRam::discard`]) stay correct: KVM
    /// follows the host mapping through its MMU notifier.
    pub fn map_ram(&self, ram: &GuestRam) -> Result<()> {
        let host = ram.host_range(ram.base(), ram.len() as usize).ok_or(Error::Invalid("guest RAM has no host mapping"))?;
        self.map_raw(host, ram.base())
    }

    /// Maps host memory the VMM doesn't own (a GPU renderer's host-visible allocation) as guest
    /// memory at `ipa`. Callable from any thread.
    ///
    /// The caller must keep `[host, host + len)` mapped until [`Vm::unmap_raw`] or the VM is
    /// dropped; the guest reads and writes it directly.
    pub fn map_raw(&self, host: HostRange, ipa: u64) -> Result<()> {
        let page = page_size();
        if !(host.as_ptr() as u64).is_multiple_of(page) || !ipa.is_multiple_of(page) || !(host.len() as u64).is_multiple_of(page) {
            return Err(Error::Invalid("guest memory must be host-page aligned"));
        }
        let mut slots = self.slots();
        let slot = slots.claim(ipa, host.len() as u64)?;
        let region = kvm_userspace_memory_region {
            slot,
            flags: 0,
            guest_phys_addr: ipa,
            memory_size: host.len() as u64,
            userspace_addr: host.as_ptr() as u64,
        };
        // SAFETY: the range is a live host mapping of `memory_size` bytes that the caller keeps
        // mapped while the slot exists (the contract above); KVM only accesses it through the
        // host page tables, so a later munmap can't make the kernel touch freed memory.
        let r = unsafe { self.fd.set_user_memory_region(region) };
        if let Err(e) = r {
            slots.release(ipa, host.len() as u64);
            return Err(kvm_err("KVM_SET_USER_MEMORY_REGION")(e));
        }
        Ok(())
    }

    /// Removes the slot [`Vm::map_raw`] mapped at exactly `[ipa, ipa + len)`.
    pub fn unmap_raw(&self, ipa: u64, len: usize) -> Result<()> {
        let mut slots = self.slots();
        let slot = slots.release(ipa, len as u64).ok_or(Error::Invalid("no guest memory mapped there"))?;
        let region = kvm_userspace_memory_region { slot, flags: 0, guest_phys_addr: ipa, memory_size: 0, userspace_addr: 0 };
        // SAFETY: a zero-sized region deletes the slot; no host memory is referenced.
        unsafe { self.fd.set_user_memory_region(region) }.map_err(kvm_err("KVM_SET_USER_MEMORY_REGION"))
    }

    /// A notification for 4-byte guest stores to MMIO address `addr` (of `value`, or of any value
    /// with `None`): the store completes inside the kernel, which signals the returned
    /// [`Notifier`]; the vCPU never exits to userspace. A store that doesn't match exits as MMIO.
    pub fn ioeventfd(&self, addr: u64, value: Option<u32>) -> Result<Notifier> {
        let evt = EventFd::new(EFD_NONBLOCK).map_err(|e| Error::Kvm { call: "eventfd", err: e })?;
        let at = IoEventAddress::Mmio(addr);
        match value {
            Some(v) => self.fd.register_ioevent(&evt, &at, v),
            None => self.fd.register_ioevent(&evt, &at, NoDatamatch),
        }
        .map_err(kvm_err("KVM_IOEVENTFD"))?;
        Ok(Notifier(evt))
    }

    /// A [`Notifier`] whose signal injects an edge on `gsi` inside the kernel (`KVM_IRQFD`),
    /// for device threads that only hold a file descriptor (another process's, say). On arm64
    /// GSI n is SPI n + 32 once the vGIC is initialized.
    pub fn irqfd(&self, gsi: u32) -> Result<Notifier> {
        let evt = EventFd::new(EFD_NONBLOCK).map_err(|e| Error::Kvm { call: "eventfd", err: e })?;
        self.fd.register_irqfd(&evt, gsi).map_err(kvm_err("KVM_IRQFD"))?;
        Ok(Notifier(evt))
    }

    /// Drives interrupt line `irq` (`KVM_IRQ_LINE`; on arm64 see [`Vm::spi`]) to `level`. Callable
    /// from any thread; the in-kernel interrupt controller kicks the vCPU that takes it, so no
    /// vCPU thread is involved.
    pub fn set_irq_line(&self, irq: u32, level: bool) -> Result<()> {
        self.fd.set_irq_line(irq, level).map_err(kvm_err("KVM_IRQ_LINE"))
    }

    /// The [`Vm::set_irq_line`] number of SPI `intid` (32..1020) on the vGIC.
    #[cfg(target_arch = "aarch64")]
    pub fn spi(intid: u32) -> u32 {
        (kvm_bindings::KVM_ARM_IRQ_TYPE_SPI << kvm_bindings::KVM_ARM_IRQ_TYPE_SHIFT) | (intid & 0xffff)
    }

    /// Creates vCPU `id`. On arm64 its MPIDR affinity is derived from `id` (Aff0 = id for ids
    /// below 16).
    pub fn create_vcpu(&self, id: u64) -> Result<Vcpu> {
        Ok(Vcpu::new(self.fd.create_vcpu(id).map_err(kvm_err("KVM_CREATE_VCPU"))?))
    }

    #[cfg(target_arch = "aarch64")]
    pub(crate) fn fd(&self) -> &VmFd {
        &self.fd
    }

    /// Creates the in-kernel GICv3 with its distributor at `dist` and the redistributors (one
    /// 128 KiB frame pair per vCPU) from `redist`, handling `irqs` interrupt IDs (SGIs, PPIs and
    /// SPIs; a multiple of 32, 64 to 1024). Call [`Vgic::init`] after creating every vCPU and
    /// before running any.
    #[cfg(target_arch = "aarch64")]
    pub fn create_vgic_v3(&self, dist: u64, redist: u64, irqs: u32) -> Result<Vgic> {
        use kvm_bindings::*;
        let mut dev = kvm_create_device { type_: kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3, fd: 0, flags: 0 };
        let fd = self.fd.create_device(&mut dev).map_err(|e| match e.errno() {
            libc::ENODEV | libc::ENXIO => Error::Unsupported("an in-kernel GICv3 (the host's interrupt controller isn't GICv3: GICv2 hosts such as the Raspberry Pi 5 can't run the stock board)"),
            _ => kvm_err("KVM_CREATE_DEVICE(vgic-v3)")(e),
        })?;
        let vgic = Vgic { fd };
        vgic.set(KVM_DEV_ARM_VGIC_GRP_ADDR, u64::from(KVM_VGIC_V3_ADDR_TYPE_DIST), &dist, "the GIC distributor address")?;
        vgic.set(KVM_DEV_ARM_VGIC_GRP_ADDR, u64::from(KVM_VGIC_V3_ADDR_TYPE_REDIST), &redist, "the GIC redistributor address")?;
        vgic.set(KVM_DEV_ARM_VGIC_GRP_NR_IRQS, 0, &irqs, "the GIC's interrupt count")?;
        Ok(vgic)
    }
}

/// The in-kernel GICv3.
#[cfg(target_arch = "aarch64")]
pub struct Vgic {
    fd: kvm_ioctls::DeviceFd,
}

#[cfg(target_arch = "aarch64")]
impl Vgic {
    fn set<T>(&self, group: u32, attr: u64, value: &T, what: &'static str) -> Result<()> {
        let a = kvm_bindings::kvm_device_attr { flags: 0, group, attr, addr: value as *const T as u64 };
        self.fd.set_device_attr(&a).map_err(|e| Error::Kvm { call: what, err: std::io::Error::from_raw_os_error(e.errno()) })
    }

    /// Finishes the GIC once every vCPU exists (`KVM_DEV_ARM_VGIC_CTRL_INIT`).
    pub fn init(&self) -> Result<()> {
        let a = kvm_bindings::kvm_device_attr {
            flags: 0,
            group: kvm_bindings::KVM_DEV_ARM_VGIC_GRP_CTRL,
            attr: u64::from(kvm_bindings::KVM_DEV_ARM_VGIC_CTRL_INIT),
            addr: 0,
        };
        self.fd.set_device_attr(&a).map_err(kvm_err("KVM_DEV_ARM_VGIC_CTRL_INIT"))
    }
}

/// An eventfd KVM signals ([`Vm::ioeventfd`]) or listens on ([`Vm::irqfd`]). Poll its fd
/// (`AsFd`) for readability.
pub struct Notifier(EventFd);

impl Notifier {
    /// Consumes pending signals; true if there were any. Never blocks.
    pub fn drain(&self) -> bool {
        self.0.read().is_ok()
    }

    /// Signals it (for an irqfd: injects the interrupt).
    pub fn signal(&self) -> std::io::Result<()> {
        self.0.write(1)
    }
}

impl AsFd for Notifier {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: the eventfd is owned by `self` and stays open for the borrow's lifetime.
        unsafe { BorrowedFd::borrow_raw(self.0.as_raw_fd()) }
    }
}

fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(p).ok().filter(|&p| p > 0).unwrap_or(4096)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_take_the_lowest_free_number_and_refuse_overlaps() {
        let mut s = Slots { used: Vec::new(), limit: 3 };
        assert_eq!(s.claim(0x4000_0000, 0x1000).ok(), Some(0));
        assert_eq!(s.claim(0x8000_0000, 0x1000).ok(), Some(1));
        assert!(s.claim(0x4000_0800, 0x1000).is_err(), "overlaps slot 0");
        assert!(s.claim(0x3fff_f800, 0x1000).is_err(), "overlaps slot 0 from below");
        assert!(s.claim(0, 0).is_err(), "empty");
        assert!(s.claim(u64::MAX - 1, 4).is_err(), "wraps");
        assert_eq!(s.claim(0x4000_1000, 0x1000).ok(), Some(2), "adjacent is fine");
        assert!(s.claim(0x9000_0000, 0x1000).is_err(), "all slots used");
        assert_eq!(s.release(0x8000_0000, 0x1000), Some(1));
        assert_eq!(s.release(0x8000_0000, 0x1000), None, "already free");
        assert_eq!(s.release(0x4000_0000, 0x800), None, "only exact ranges");
        assert_eq!(s.claim(0x9000_0000, 0x1000).ok(), Some(1), "a freed number is reused");
    }
}
