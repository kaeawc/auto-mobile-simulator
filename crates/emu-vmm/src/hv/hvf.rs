//! The Hypervisor.framework backend (Apple silicon): the in-kernel GICv3 without guest-wait
//! wakeups (hence [`crate::wake`]'s kicks and host-side WFI), PSCI in the VMM, and MMIO decoded
//! from the data abort's syndrome.
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use emu_hvf::{Exit, Reg, SysReg, Vcpu, VcpuHandle, Vm};

use super::GicInfo;
use crate::irq::IrqLine;
use crate::vcpu::{Cpus, DataAbort, ForceExit};
use crate::wake::{self, VcpuControl};

pub use emu_hvf::Error;
use emu_hvf::GuestRam;
pub use emu_hvf::HostRange;

const EC_WFX: u64 = 0x01;
const EC_HVC64: u64 = 0x16;
const EC_SMC64: u64 = 0x17;
const EC_SYSREG: u64 = 0x18;
const EC_IABT_LOWER: u64 = 0x20;
const EC_DABT_LOWER: u64 = 0x24;

/// EL1h with D, A, I and F masked: the state the arm64 boot protocol requires.
const CPSR_EL1H_DAIF: u64 = 0x3c5;
const VTIMER_PPI: u32 = 27;

mod psci {
    pub const VERSION: u32 = 0x8400_0000;
    pub const CPU_SUSPEND_32: u32 = 0x8400_0001;
    pub const CPU_SUSPEND_64: u32 = 0xc400_0001;
    pub const CPU_OFF: u32 = 0x8400_0002;
    pub const CPU_ON_32: u32 = 0x8400_0003;
    pub const CPU_ON_64: u32 = 0xc400_0003;
    pub const AFFINITY_INFO_32: u32 = 0x8400_0004;
    pub const AFFINITY_INFO_64: u32 = 0xc400_0004;
    pub const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
    pub const SYSTEM_OFF: u32 = 0x8400_0008;
    pub const SYSTEM_RESET: u32 = 0x8400_0009;

    pub const SUCCESS: i64 = 0;
    pub const NOT_SUPPORTED: i64 = -1;
    pub const INVALID_PARAMS: i64 = -2;
    pub const ALREADY_ON: i64 = -4;
    pub const ON_PENDING: i64 = -5;
}

/// What device threads use to rouse vCPUs after raising an interrupt (see [`wake`]).
type Wakers = wake::Wakers<HvVcpus>;

/// The framework side of [`Wakers`]: each vCPU's handle (registered by its thread) and the
/// GIC's SPI routing.
struct HvVcpus {
    handles: Vec<OnceLock<VcpuHandle>>,
}

impl VcpuControl for HvVcpus {
    fn kick(&self, cpus: u64) {
        let handles = self.handles.iter().enumerate().filter(|(i, _)| cpus & (1 << i) != 0);
        emu_hvf::kick_vcpus(handles.filter_map(|(_, h)| h.get().copied()));
    }

    fn spi_router(&self, intid: u32) -> Option<u64> {
        Vm::spi_router(intid).ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Power {
    Off,
    Pending { entry: u64, ctx: u64 },
    On,
}

struct CpuSlot {
    power: Mutex<Power>,
    cv: Condvar,
}

/// The VM, its vCPUs' wake state and their PSCI power states. One per process.
pub struct Hypervisor {
    vm: Vm,
    wakers: Arc<Wakers>,
    slots: Vec<CpuSlot>,
}

impl Hypervisor {
    /// Creates the VM for `cpus` vCPUs.
    pub fn new(cpus: u32) -> anyhow::Result<Hypervisor> {
        let vm = Vm::new().context("creating the VM (is the binary signed with com.apple.security.hypervisor?)")?;
        let n = cpus as usize;
        let control = HvVcpus { handles: (0..n).map(|_| OnceLock::new()).collect() };
        Ok(Hypervisor {
            vm,
            wakers: Arc::new(Wakers::new(n, control, wake::Mode::from_env())),
            slots: (0..n).map(|_| CpuSlot { power: Mutex::new(Power::Off), cv: Condvar::new() }).collect(),
        })
    }

    /// Maps `ram` into the guest at its base address (the caller keeps it alive past the VM).
    pub fn map_ram(&self, ram: &GuestRam) -> Result<(), Error> {
        self.vm.map_ram(ram)
    }

    /// Creates the in-kernel GICv3. Must run before any vCPU is created.
    pub fn create_gic(&self, dist_base: u64, redist_base: u64) -> Result<GicInfo, Error> {
        let g = self.vm.create_gic(dist_base, redist_base)?;
        Ok(GicInfo {
            dist_base: g.dist_base,
            dist_size: g.dist_size,
            redist_base: g.redist_base,
            redist_region_size: g.redist_region_size,
            spi_base: g.spi_base,
            spi_count: g.spi_count,
        })
    }

    /// The line for SPI `intid`: raising it rouses the vCPU the SPI is routed to.
    pub fn spi(&self, intid: u32) -> Arc<dyn IrqLine> {
        Arc::new(GicSpi { intid, wakers: self.wakers.clone() })
    }

    /// Nothing to do: Hypervisor.framework has no ioeventfd, so QueueNotify stores exit as MMIO
    /// and reach the transport that way.
    pub fn serve_notifies(&self, notifies: Vec<super::QueueNotify>) -> anyhow::Result<()> {
        drop(notifies);
        Ok(())
    }

    /// QueueNotify stores completed in the kernel: none here.
    pub fn kernel_notifies(&self) -> u64 {
        0
    }

    /// Starts one thread per vCPU: the boot CPU at `entry` with x0 = `dtb`, the rest waiting for
    /// PSCI CPU_ON. The guest counter starts near zero, identical on every vCPU.
    pub fn start_vcpus(self: &Arc<Self>, cpus: &Arc<Cpus>, entry: u64, dtb: u64) -> anyhow::Result<Vec<JoinHandle<()>>> {
        let vtimer_offset = emu_hvf::host_ticks();
        (0..cpus.count())
            .map(|idx| {
                let (hv, cpus) = (self.clone(), cpus.clone());
                let boot = (idx == 0).then_some((entry, dtb));
                std::thread::Builder::new().name(format!("vcpu{idx}")).spawn(move || {
                    let t = VcpuThread { hv: &hv, cpus: &cpus, idx, vtimer_offset };
                    cpus.finish(idx, t.main(boot));
                })
            })
            .collect::<std::io::Result<_>>()
            .map_err(Into::into)
    }
}

impl ForceExit for Hypervisor {
    fn force_exit_all(&self) {
        self.wakers.wake_all();
        for slot in &self.slots {
            let _g = slot.power.lock().unwrap_or_else(|e| e.into_inner());
            slot.cv.notify_all();
        }
    }
}

/// Maps host memory the VMM doesn't own (a GPU renderer's host-visible allocation) at `ipa`;
/// see [`Vm::map_raw`]. Callable from any thread while the VM exists.
pub fn map_raw(host: HostRange, ipa: u64) -> Result<(), Error> {
    Vm::map_raw(host, ipa)
}

pub fn unmap_raw(ipa: u64, len: usize) -> Result<(), Error> {
    Vm::unmap_raw(ipa, len)
}

/// An SPI on the in-kernel GIC.
struct GicSpi {
    intid: u32,
    wakers: Arc<Wakers>,
}

impl IrqLine for GicSpi {
    fn set(&self, level: bool) {
        if let Err(e) = Vm::set_spi(self.intid, level) {
            eprintln!("emu: SPI {} -> {level}: {e}", self.intid);
        }
        if level {
            self.wakers.wake_spi(self.intid);
        }
    }
}

enum Flow {
    CpuOff,
    Stop,
}

struct VcpuThread<'a> {
    hv: &'a Hypervisor,
    cpus: &'a Cpus,
    idx: usize,
    vtimer_offset: u64,
}

impl VcpuThread<'_> {
    fn main(&self, boot: Option<(u64, u64)>) -> Result<(), Error> {
        let (idx, cpus) = (self.idx, self.cpus);
        // The guest waits on this thread: keep it eligible for performance cores even when emu's
        // parent runs at a low QoS. Not inherited by threads it spawns (see `set_qos`).
        emu_hvf::host::set_qos(emu_hvf::host::Qos::UserInitiated);
        let vcpu = Vcpu::new()?;
        vcpu.set_sys_reg(SysReg::MPIDR_EL1, idx as u64)?;
        vcpu.set_vtimer_offset(self.vtimer_offset)?;
        let sctlr_reset = vcpu.sys_reg(SysReg::SCTLR_EL1)?;
        let slot = &self.hv.slots[idx];
        let _ = self.hv.wakers.control().handles[idx].set(vcpu.handle());
        if let Some((entry, x0)) = boot {
            *slot.power.lock().unwrap_or_else(|e| e.into_inner()) = Power::Pending { entry, ctx: x0 };
        }
        loop {
            let (entry, ctx) = {
                let mut p = slot.power.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if cpus.is_stopping() {
                        return Ok(());
                    }
                    if let Power::Pending { entry, ctx } = *p {
                        *p = Power::On;
                        self.hv.wakers.online(idx);
                        break (entry, ctx);
                    }
                    p = slot.cv.wait(p).unwrap_or_else(|e| e.into_inner());
                }
            };
            // Architectural reset state, then the PSCI/boot entry contract: x0 = context.
            for r in 0..=30 {
                vcpu.set_reg(Reg::x(r), 0)?;
            }
            vcpu.set_sys_reg(SysReg::SCTLR_EL1, sctlr_reset)?;
            vcpu.set_reg(Reg::x(0), ctx)?;
            vcpu.set_reg(Reg::PC, entry)?;
            vcpu.set_reg(Reg::CPSR, CPSR_EL1H_DAIF)?;
            match self.run(&vcpu)? {
                Flow::CpuOff => {
                    self.hv.wakers.offline(idx);
                    *slot.power.lock().unwrap_or_else(|e| e.into_inner()) = Power::Off;
                }
                Flow::Stop => return Ok(()),
            }
        }
    }

    fn run(&self, vcpu: &Vcpu) -> Result<Flow, Error> {
        let (idx, cpus) = (self.idx, self.cpus);
        let stats = &cpus.stats[idx];
        let mut vtimer_masked = false;
        loop {
            if cpus.is_stopping() {
                return Ok(Flow::Stop);
            }
            if vtimer_masked {
                // Unmask once the guest has handled the tick (timer no longer asserting).
                let ctl = vcpu.sys_reg(SysReg::CNTV_CTL_EL0)?;
                if ctl & 0b111 != 0b101 {
                    vcpu.set_vtimer_mask(false)?;
                    vtimer_masked = false;
                }
            }
            let t = cpus.entering_guest();
            self.hv.wakers.enter(idx);
            let exit = vcpu.run();
            self.hv.wakers.leave(idx);
            cpus.left_guest(idx, t);
            match exit? {
                Exit::Canceled => {
                    stats.canceled.fetch_add(1, Ordering::Relaxed);
                }
                Exit::VtimerActivated => {
                    // Only seen without an in-kernel GIC; raise the PPI ourselves.
                    stats.vtimer.fetch_add(1, Ordering::Relaxed);
                    vcpu.set_ppi_pending(VTIMER_PPI)?;
                    vtimer_masked = true;
                }
                Exit::Exception { syndrome, ipa, .. } => {
                    if let Some(flow) = self.exception(vcpu, syndrome, ipa)? {
                        return Ok(flow);
                    }
                }
                Exit::Unknown(r) => {
                    cpus.request_stop(crate::vcpu::StopReason::Fault(format!("vcpu{idx}: unknown exit reason {r}")));
                    return Ok(Flow::Stop);
                }
            }
        }
    }

    fn advance_pc(vcpu: &Vcpu) -> Result<(), Error> {
        let pc = vcpu.reg(Reg::PC)?;
        vcpu.set_reg(Reg::PC, pc + 4)
    }

    fn fault(&self, msg: String) -> Option<Flow> {
        self.cpus.request_stop(crate::vcpu::StopReason::Fault(format!("vcpu{}: {msg}", self.idx)));
        Some(Flow::Stop)
    }

    fn exception(&self, vcpu: &Vcpu, syndrome: u64, ipa: u64) -> Result<Option<Flow>, Error> {
        let (idx, cpus) = (self.idx, self.cpus);
        let stats = &cpus.stats[idx];
        let ec = (syndrome >> 26) & 0x3f;
        let iss = syndrome & 0x01ff_ffff;
        match ec {
            EC_DABT_LOWER => {
                let Some(abort) = DataAbort::decode(iss) else {
                    let pc = vcpu.reg(Reg::PC)?;
                    return Ok(self.fault(format!("data abort without syndrome at ipa {ipa:#x} (pc {pc:#x}, esr {syndrome:#x})")));
                };
                if abort.write {
                    let v = if abort.rt == 31 { 0 } else { vcpu.reg(Reg::x(abort.rt))? };
                    cpus.mmio_write(idx, ipa, &v.to_le_bytes()[..abort.size]);
                } else {
                    let mut buf = [0u8; 8];
                    cpus.mmio_read(idx, ipa, &mut buf[..abort.size]);
                    if abort.rt != 31 {
                        vcpu.set_reg(Reg::x(abort.rt), abort.load_value(&buf))?;
                    }
                }
                Self::advance_pc(vcpu)?;
            }
            EC_HVC64 | EC_SMC64 => {
                stats.hvc.fetch_add(1, Ordering::Relaxed);
                // HVC leaves PC past the instruction already; SMC traps with PC on it.
                if ec == EC_SMC64 {
                    Self::advance_pc(vcpu)?;
                }
                return self.psci(vcpu);
            }
            EC_WFX => {
                stats.wfi.fetch_add(1, Ordering::Relaxed);
                let is_wfe = iss & 1 == 1;
                Self::advance_pc(vcpu)?;
                if !is_wfe {
                    self.wfi(vcpu)?;
                }
            }
            EC_SYSREG => {
                stats.sysreg.fetch_add(1, Ordering::Relaxed);
                // Registers the framework doesn't handle (debug, PMU, ...): RAZ/WI.
                let is_read = iss & 1 == 1;
                let rt = ((iss >> 5) & 0x1f) as u32;
                let enc = iss & !(0x1f << 5) & !1;
                cpus.log_sysreg(enc, is_read);
                if is_read && rt != 31 {
                    vcpu.set_reg(Reg::x(rt), 0)?;
                }
                Self::advance_pc(vcpu)?;
            }
            EC_IABT_LOWER => {
                let pc = vcpu.reg(Reg::PC)?;
                return Ok(self.fault(format!("instruction fetch from unmapped ipa {ipa:#x} (pc {pc:#x}, esr {syndrome:#x})")));
            }
            _ => {
                let pc = vcpu.reg(Reg::PC)?;
                return Ok(self.fault(format!("unhandled exception class {ec:#x} (pc {pc:#x}, esr {syndrome:#x})")));
            }
        }
        Ok(None)
    }

    /// Host-side WFI: waits for the timer deadline, an interrupt for this vCPU, or (while
    /// another vCPU could send an SGI, which never reaches the VMM) at most [`wake::SGI_POLL`].
    fn wfi(&self, vcpu: &Vcpu) -> Result<(), Error> {
        let ctl = vcpu.sys_reg(SysReg::CNTV_CTL_EL0)?;
        let mut timer = None;
        if ctl & 1 == 1 && ctl & 2 == 0 {
            let cval = vcpu.sys_reg(SysReg::CNTV_CVAL_EL0)?;
            let now = emu_hvf::host_ticks().wrapping_sub(self.vtimer_offset);
            if cval <= now {
                return Ok(());
            }
            timer = Some(Duration::from_nanos(emu_hvf::ticks_to_nanos(cval - now)));
        }
        self.cpus.parked(self.idx, || self.hv.wakers.park(self.idx, timer));
        Ok(())
    }

    fn psci(&self, vcpu: &Vcpu) -> Result<Option<Flow>, Error> {
        let func = vcpu.reg(Reg::x(0))? as u32;
        let arg = |n| vcpu.reg(Reg::x(n));
        let slots = &self.hv.slots;
        let ret: i64 = match func {
            psci::VERSION => 0x2, // PSCI 0.2, matching the device tree
            psci::CPU_SUSPEND_32 | psci::CPU_SUSPEND_64 => {
                self.wfi(vcpu)?;
                psci::SUCCESS
            }
            psci::CPU_OFF => return Ok(Some(Flow::CpuOff)),
            psci::CPU_ON_32 | psci::CPU_ON_64 => {
                let target = (arg(1)? & 0xff) as usize;
                let (entry, ctx) = (arg(2)?, arg(3)?);
                match slots.get(target) {
                    None => psci::INVALID_PARAMS,
                    Some(slot) => {
                        let mut p = slot.power.lock().unwrap_or_else(|e| e.into_inner());
                        match *p {
                            Power::On => psci::ALREADY_ON,
                            Power::Pending { .. } => psci::ON_PENDING,
                            Power::Off => {
                                // Online before it runs: from now on it can send SGIs.
                                self.hv.wakers.online(target);
                                *p = Power::Pending { entry, ctx };
                                slot.cv.notify_all();
                                psci::SUCCESS
                            }
                        }
                    }
                }
            }
            psci::AFFINITY_INFO_32 | psci::AFFINITY_INFO_64 => {
                let target = (arg(1)? & 0xff) as usize;
                match slots.get(target) {
                    None => psci::INVALID_PARAMS,
                    Some(slot) => match *slot.power.lock().unwrap_or_else(|e| e.into_inner()) {
                        Power::On => 0,
                        Power::Off => 1,
                        Power::Pending { .. } => 2,
                    },
                }
            }
            psci::MIGRATE_INFO_TYPE => 2, // no trusted OS
            psci::SYSTEM_OFF => {
                self.cpus.request_stop(crate::vcpu::StopReason::PowerOff);
                return Ok(Some(Flow::Stop));
            }
            psci::SYSTEM_RESET => {
                self.cpus.request_stop(crate::vcpu::StopReason::Reset);
                return Ok(Some(Flow::Stop));
            }
            _ => psci::NOT_SUPPORTED,
        };
        vcpu.set_reg(Reg::x(0), ret as u64)?;
        Ok(None)
    }
}
