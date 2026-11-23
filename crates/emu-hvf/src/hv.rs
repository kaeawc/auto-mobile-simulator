//! The hypervisor itself: the VM, its GIC, vCPUs and the host counter (Apple silicon only).
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{Error, GuestRam, HostRange, Result, ffi};

fn check(call: &'static str, code: i32) -> Result<()> {
    if code == 0 { Ok(()) } else { Err(Error::Hv { call, code }) }
}

static VM_EXISTS: AtomicBool = AtomicBool::new(false);

/// General purpose and control registers (`hv_reg_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reg(pub u32);

impl Reg {
    pub const PC: Reg = Reg(31);
    pub const FPCR: Reg = Reg(32);
    pub const FPSR: Reg = Reg(33);
    pub const CPSR: Reg = Reg(34);

    /// X0..=X30.
    pub fn x(n: u32) -> Reg {
        assert!(n <= 30, "no such general purpose register");
        Reg(n)
    }
}

/// System registers (`hv_sys_reg_t`), encoded as op0:op1:crn:crm:op2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysReg(pub u16);

impl SysReg {
    pub const MPIDR_EL1: SysReg = SysReg(0xc005);
    pub const SCTLR_EL1: SysReg = SysReg(0xc080);
    pub const CNTV_CTL_EL0: SysReg = SysReg(0xdf19);
    pub const CNTV_CVAL_EL0: SysReg = SysReg(0xdf1a);
}

/// Addresses and sizes of the in-kernel GICv3 after creation.
#[derive(Clone, Copy, Debug)]
pub struct GicInfo {
    pub dist_base: u64,
    pub dist_size: u64,
    pub redist_base: u64,
    /// Size of the whole redistributor region (all vCPUs).
    pub redist_region_size: u64,
    /// First valid SPI INTID and how many follow it.
    pub spi_base: u32,
    pub spi_count: u32,
}

/// The virtual machine. One per process; destroyed on drop.
pub struct Vm {
    _private: (),
}

/// VM-wide settings fixed at creation.
#[derive(Clone, Copy, Debug)]
pub struct VmConfig {
    /// Guest physical address bits (clamped to what the host supports).
    pub ipa_bits: u32,
    /// Map guest memory at 4 KiB granularity instead of the 16 KiB default (macOS 26+). Needed
    /// for host-visible GPU memory, which the guest maps at 4 KiB-aligned offsets.
    pub granule_4k: bool,
}

impl Default for VmConfig {
    fn default() -> Self {
        VmConfig { ipa_bits: 40, granule_4k: true }
    }
}

impl Vm {
    pub fn new() -> Result<Vm> {
        Vm::with_config(VmConfig::default())
    }

    pub fn with_config(cfg: VmConfig) -> Result<Vm> {
        if VM_EXISTS.swap(true, Ordering::SeqCst) {
            return Err(Error::AlreadyCreated);
        }
        // SAFETY: plain FFI calls on a config object we own; the framework retains it.
        let rc = unsafe {
            let config = ffi::hv_vm_config_create();
            let mut max = 0u32;
            if ffi::hv_vm_config_get_max_ipa_size(&mut max) == 0 {
                let _ = ffi::hv_vm_config_set_ipa_size(config, cfg.ipa_bits.min(max));
            }
            if cfg.granule_4k {
                // Unsupported before macOS 26: keep the default granule then.
                let _ = ffi::hv_vm_config_set_ipa_granule(config, 0);
            }
            ffi::hv_vm_create(config)
        };
        if rc != 0 {
            VM_EXISTS.store(false, Ordering::SeqCst);
            return check("hv_vm_create", rc).map(|()| unreachable!());
        }
        Ok(Vm { _private: () })
    }

    /// Creates the in-kernel GICv3. Must run before any vCPU is created.
    pub fn create_gic(&self, dist_base: u64, redist_base: u64) -> Result<GicInfo> {
        let mut dist_size = 0usize;
        let mut redist_region_size = 0usize;
        let (mut spi_base, mut spi_count) = (0u32, 0u32);
        // SAFETY: out-pointers reference live locals; the config object is owned by the
        // framework and intentionally leaked (one per process lifetime).
        unsafe {
            check("hv_gic_get_distributor_size", ffi::hv_gic_get_distributor_size(&mut dist_size))?;
            check("hv_gic_get_redistributor_region_size", ffi::hv_gic_get_redistributor_region_size(&mut redist_region_size))?;
            check("hv_gic_get_spi_interrupt_range", ffi::hv_gic_get_spi_interrupt_range(&mut spi_base, &mut spi_count))?;
            let cfg = ffi::hv_gic_config_create();
            check("hv_gic_config_set_distributor_base", ffi::hv_gic_config_set_distributor_base(cfg, dist_base))?;
            check("hv_gic_config_set_redistributor_base", ffi::hv_gic_config_set_redistributor_base(cfg, redist_base))?;
            check("hv_gic_create", ffi::hv_gic_create(cfg))?;
        }
        Ok(GicInfo {
            dist_base,
            dist_size: dist_size as u64,
            redist_base,
            redist_region_size: redist_region_size as u64,
            spi_base,
            spi_count,
        })
    }

    /// Maps `ram` into the guest at its base address, readable, writable and executable.
    pub fn map_ram(&self, ram: &GuestRam) -> Result<()> {
        const RWX: u64 = 0b111;
        // SAFETY: `ram` owns a page-aligned mapping of exactly `len` bytes that outlives the VM
        // mapping as long as callers keep it alive, which `Machine` does (it holds an Arc).
        check("hv_vm_map", unsafe { ffi::hv_vm_map(ram.host_ptr().cast(), ram.base(), ram.len() as usize, RWX) })
    }

    /// Maps host memory the VMM doesn't own as guest memory (e.g. a GPU renderer's
    /// host-visible allocation) at `ipa`. Callable from any thread while the VM exists.
    ///
    /// The caller must keep `[host, host + len)` mapped until [`Vm::unmap_raw`]; the guest
    /// reads and writes it directly.
    pub fn map_raw(host: HostRange, ipa: u64) -> Result<()> {
        const RW: u64 = 0b011;
        // SAFETY: the range comes from a live host mapping (see the contract above).
        check("hv_vm_map", unsafe { ffi::hv_vm_map(host.ptr.cast(), ipa, host.len, RW) })
    }

    pub fn unmap_raw(ipa: u64, len: usize) -> Result<()> {
        // SAFETY: plain FFI call; unmapping a range only affects the guest view.
        check("hv_vm_unmap", unsafe { ffi::hv_vm_unmap(ipa, len) })
    }

    /// Raises or lowers a shared peripheral interrupt. Callable from any thread.
    pub fn set_spi(intid: u32, level: bool) -> Result<()> {
        // SAFETY: plain FFI call on the process-wide GIC.
        check("hv_gic_set_spi", unsafe { ffi::hv_gic_set_spi(intid, level) })
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        // SAFETY: plain FFI call. Errors (e.g. vCPUs still alive at process exit) are ignored.
        unsafe { ffi::hv_vm_destroy() };
        VM_EXISTS.store(false, Ordering::SeqCst);
    }
}

/// Why `Vcpu::run` returned.
#[derive(Clone, Copy, Debug)]
pub enum Exit {
    /// Another thread called [`VcpuHandle::kick`].
    Canceled,
    /// The guest trapped: `syndrome` is ESR_EL2, `ipa` the faulting guest physical address.
    Exception {
        syndrome: u64,
        virtual_address: u64,
        ipa: u64,
    },
    /// The virtual timer fired; it stays masked until [`Vcpu::set_vtimer_mask`]`(false)`.
    VtimerActivated,
    Unknown(u32),
}

/// A virtual CPU. Hypervisor.framework requires all calls for a vCPU to come from the thread
/// that created it, so this type is neither `Send` nor `Sync`.
pub struct Vcpu {
    id: u64,
    exit: *const ffi::VcpuExit,
    _not_send: PhantomData<*const ()>,
}

impl Vcpu {
    pub fn new() -> Result<Vcpu> {
        let mut id = 0u64;
        let mut exit: *const ffi::VcpuExit = std::ptr::null();
        // SAFETY: out-pointers reference live locals; a null config selects the defaults.
        check("hv_vcpu_create", unsafe { ffi::hv_vcpu_create(&mut id, &mut exit, std::ptr::null_mut()) })?;
        Ok(Vcpu { id, exit, _not_send: PhantomData })
    }

    pub fn handle(&self) -> VcpuHandle {
        VcpuHandle(self.id)
    }

    pub fn reg(&self, reg: Reg) -> Result<u64> {
        let mut v = 0;
        // SAFETY: called on the owning thread; out-pointer references a live local.
        check("hv_vcpu_get_reg", unsafe { ffi::hv_vcpu_get_reg(self.id, reg.0, &mut v) })?;
        Ok(v)
    }

    pub fn set_reg(&self, reg: Reg, value: u64) -> Result<()> {
        // SAFETY: called on the owning thread.
        check("hv_vcpu_set_reg", unsafe { ffi::hv_vcpu_set_reg(self.id, reg.0, value) })
    }

    pub fn sys_reg(&self, reg: SysReg) -> Result<u64> {
        let mut v = 0;
        // SAFETY: called on the owning thread; out-pointer references a live local.
        check("hv_vcpu_get_sys_reg", unsafe { ffi::hv_vcpu_get_sys_reg(self.id, reg.0, &mut v) })?;
        Ok(v)
    }

    pub fn set_sys_reg(&self, reg: SysReg, value: u64) -> Result<()> {
        // SAFETY: called on the owning thread.
        check("hv_vcpu_set_sys_reg", unsafe { ffi::hv_vcpu_set_sys_reg(self.id, reg.0, value) })
    }

    pub fn set_vtimer_mask(&self, masked: bool) -> Result<()> {
        // SAFETY: called on the owning thread.
        check("hv_vcpu_set_vtimer_mask", unsafe { ffi::hv_vcpu_set_vtimer_mask(self.id, masked) })
    }

    /// Guest CNTVCT_EL0 = host counter − `offset`.
    pub fn set_vtimer_offset(&self, offset: u64) -> Result<()> {
        // SAFETY: called on the owning thread.
        check("hv_vcpu_set_vtimer_offset", unsafe { ffi::hv_vcpu_set_vtimer_offset(self.id, offset) })
    }

    /// Makes a private peripheral interrupt pending on this vCPU's redistributor.
    pub fn set_ppi_pending(&self, intid: u32) -> Result<()> {
        if !(16..32).contains(&intid) {
            return Err(Error::Invalid("PPI INTID must be in 16..32"));
        }
        // SAFETY: called on the owning thread; GICR_ISPENDR0 is write-1-to-set.
        check("hv_gic_set_redistributor_reg", unsafe { ffi::hv_gic_set_redistributor_reg(self.id, ffi::GICR_ISPENDR0, 1 << intid) })
    }

    /// Enters the guest until it traps, its timer fires, or it is kicked.
    pub fn run(&self) -> Result<Exit> {
        // SAFETY: called on the owning thread.
        check("hv_vcpu_run", unsafe { ffi::hv_vcpu_run(self.id) })?;
        // SAFETY: the framework keeps `exit` valid for the vCPU's lifetime and only writes it
        // during hv_vcpu_run, which has returned.
        let exit = unsafe { &*self.exit };
        Ok(match exit.reason {
            0 => Exit::Canceled,
            1 => Exit::Exception { syndrome: exit.syndrome, virtual_address: exit.virtual_address, ipa: exit.physical_address },
            2 => Exit::VtimerActivated,
            other => Exit::Unknown(other),
        })
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        // SAFETY: called on the owning thread (the type is !Send).
        unsafe { ffi::hv_vcpu_destroy(self.id) };
    }
}

/// Sendable reference to a vCPU, good for forcing it out of the guest.
#[derive(Clone, Copy, Debug)]
pub struct VcpuHandle(u64);

impl VcpuHandle {
    /// Makes the vCPU's current or next `run` return [`Exit::Canceled`].
    pub fn kick(&self) {
        // SAFETY: hv_vcpus_exit is documented as callable from any thread.
        unsafe { ffi::hv_vcpus_exit(&self.0, 1) };
    }
}

/// Host counter the guest's virtual timer is derived from (`mach_absolute_time`).
pub fn host_ticks() -> u64 {
    // SAFETY: no arguments, no preconditions.
    unsafe { ffi::mach_absolute_time() }
}

/// Converts host counter ticks to nanoseconds.
pub fn ticks_to_nanos(ticks: u64) -> u64 {
    let mut info = ffi::MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: out-pointer references a live local.
    unsafe { ffi::mach_timebase_info(&mut info) };
    if info.denom == 0 {
        return ticks;
    }
    (u128::from(ticks) * u128::from(info.numer) / u128::from(info.denom)) as u64
}
