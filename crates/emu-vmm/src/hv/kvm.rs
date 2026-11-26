//! The KVM backend (arm64 Linux). KVM does in the kernel what Hypervisor.framework leaves to emu:
//!
//! - **Interrupts:** the in-kernel vGICv3. A device raises its line with `KVM_IRQ_LINE` from its
//!   own thread and the kernel kicks the vCPU the SPI is routed to: no wakers, no host-side WFI,
//!   no SGI polling.
//! - **WFI, the arch timer and PSCI** (CPU_ON/OFF/SUSPEND, AFFINITY_INFO) never reach emu; only
//!   SYSTEM_OFF/RESET come back, as system events.
//! - **Notifications:** every virtio QueueNotify is an ioeventfd ([`Hypervisor::serve_notifies`]):
//!   the store completes in the kernel and the vCPU goes straight back into the guest.
//! - **MMIO** arrives decoded (address, size, data), so there's no syndrome to parse.
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use emu_hvf::GuestRam;
pub use emu_hvf::HostRange;
pub use emu_kvm::Error;
use emu_kvm::{CoreReg, Exit, Kicks, Notifier, SystemEvent, Vcpu, Vgic, Vm};

use super::{GicInfo, QueueNotify};
use crate::irq::IrqLine;
use crate::vcpu::{Cpus, ForceExit, StopReason};

/// Interrupt IDs the vGIC handles: SGIs and PPIs (32), then SPIs (the board uses up to INTID 96).
const GIC_IRQS: u32 = 256;
/// GICv3 distributor size, and each vCPU's redistributor (RD_base + SGI_base frames).
const GIC_DIST_SIZE: u64 = 0x1_0000;
const GIC_REDIST_PER_CPU: u64 = 0x2_0000;
/// EL1h with D, A, I and F masked: the state the arm64 boot protocol requires.
const PSTATE_EL1H_DAIF: u64 = 0x3c5;
/// How often a stop kicks vCPU threads again: a kick that lands just before a thread enters
/// `KVM_RUN` is lost, the next one isn't.
const KICK_RETRY: Duration = Duration::from_millis(1);

/// The running VM, for [`map_raw`] / [`unmap_raw`] (callable from any thread, like HVF's).
static CURRENT: Mutex<Option<Arc<Vm>>> = Mutex::new(None);

pub struct Hypervisor {
    vm: Arc<Vm>,
    cpus: u32,
    vgic: Mutex<Option<Vgic>>,
    kicks: Arc<Kicks>,
    kicking: AtomicBool,
    /// The ioeventfd dispatcher: its stop signal and thread.
    notifies: Mutex<Option<(rustix::fd::OwnedFd, JoinHandle<()>)>>,
}

impl Hypervisor {
    /// Opens `/dev/kvm` (an inaccessible one fails here, saying how to fix it) and creates the VM.
    pub fn new(cpus: u32) -> anyhow::Result<Hypervisor> {
        let kvm = emu_kvm::open()?;
        if cpus as usize > kvm.max_vcpus() {
            anyhow::bail!("this host's KVM allows {} vCPUs per VM; the AVD asks for {cpus}", kvm.max_vcpus());
        }
        // 40 bits, as on Hypervisor.framework: the board's highest address is well below 1 TiB.
        let vm = Arc::new(kvm.create_vm(40).context("creating the KVM VM")?);
        *CURRENT.lock().unwrap_or_else(PoisonError::into_inner) = Some(vm.clone());
        Ok(Hypervisor {
            vm,
            cpus,
            vgic: Mutex::new(None),
            kicks: Arc::new(Kicks::new(cpus as usize).context("installing the vCPU kick signal")?),
            kicking: AtomicBool::new(false),
            notifies: Mutex::new(None),
        })
    }

    pub fn map_ram(&self, ram: &GuestRam) -> Result<(), Error> {
        self.vm.map_ram(ram)
    }

    /// Creates the in-kernel GICv3; [`Hypervisor::start_vcpus`] initializes it once the vCPUs
    /// exist.
    pub fn create_gic(&self, dist_base: u64, redist_base: u64) -> Result<GicInfo, Error> {
        let vgic = self.vm.create_vgic_v3(dist_base, redist_base, GIC_IRQS)?;
        *self.vgic.lock().unwrap_or_else(PoisonError::into_inner) = Some(vgic);
        Ok(GicInfo {
            dist_base,
            dist_size: GIC_DIST_SIZE,
            redist_base,
            redist_region_size: u64::from(self.cpus) * GIC_REDIST_PER_CPU,
            spi_base: 32,
            spi_count: GIC_IRQS - 32,
        })
    }

    /// The line for SPI `intid`: a level the vGIC holds until the device lowers it.
    pub fn spi(&self, intid: u32) -> Arc<dyn IrqLine> {
        Arc::new(KvmSpi { vm: self.vm.clone(), irq: Vm::spi(intid), intid })
    }

    /// Completes each store of `value` to `addr` in the kernel (ioeventfd), then runs `on` on a
    /// dispatcher thread instead of the vCPU's. Stores that don't match still exit as MMIO.
    pub fn serve_notifies(&self, notifies: Vec<QueueNotify>) -> anyhow::Result<()> {
        let mut registered = Vec::with_capacity(notifies.len());
        for n in notifies {
            let fd = self.vm.ioeventfd(n.addr, Some(n.value)).with_context(|| format!("ioeventfd at {:#x}", n.addr))?;
            registered.push((fd, n.on));
        }
        let stop = rustix::event::eventfd(0, rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK)?;
        let stop_for_thread = stop.try_clone()?;
        let thread = std::thread::Builder::new().name("virtio-notify".into()).spawn(move || {
            crate::device_thread();
            if let Err(e) = dispatch(&registered, &stop_for_thread) {
                eprintln!("emu: virtio notifications stopped: {e}");
            }
        })?;
        *self.notifies.lock().unwrap_or_else(PoisonError::into_inner) = Some((stop, thread));
        Ok(())
    }

    /// Creates every vCPU (KVM wants them all before the vGIC initializes), then runs each on its
    /// own thread: the boot CPU from `entry` with x0 = `dtb`, the others powered off until the
    /// guest's PSCI CPU_ON, which the kernel handles.
    pub fn start_vcpus(self: &Arc<Self>, cpus: &Arc<Cpus>, entry: u64, dtb: u64) -> anyhow::Result<Vec<JoinHandle<()>>> {
        let mut vcpus = Vec::with_capacity(cpus.count());
        for idx in 0..cpus.count() {
            let vcpu = self.vm.create_vcpu(idx as u64)?;
            vcpu.init(&self.vm, idx != 0)?;
            // What the device tree and the HVF backend say: PSCI 0.2. Older kernels may refuse to
            // change it; they report 0.2 or 1.0, either of which the guest's driver handles.
            let _ = vcpu.set_psci_version(emu_kvm::KVM_PSCI_0_2);
            if idx == 0 {
                vcpu.set_core_reg(CoreReg::Pc, entry)?;
                vcpu.set_core_reg(CoreReg::X(0), dtb)?;
                vcpu.set_core_reg(CoreReg::Pstate, PSTATE_EL1H_DAIF)?;
            }
            vcpus.push(vcpu);
        }
        if let Some(vgic) = self.vgic.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            vgic.init()?;
        }
        let mut threads = Vec::with_capacity(vcpus.len());
        for (idx, vcpu) in vcpus.into_iter().enumerate() {
            let (hv, cpus) = (self.clone(), cpus.clone());
            threads.push(std::thread::Builder::new().name(format!("vcpu{idx}")).spawn(move || {
                let r = hv.run(&cpus, idx, vcpu);
                cpus.finish(idx, r);
            })?);
        }
        Ok(threads)
    }

    fn run(&self, cpus: &Cpus, idx: usize, mut vcpu: Vcpu) -> Result<(), Error> {
        let _running = self.kicks.register(idx);
        let stats = &cpus.stats[idx];
        while !cpus.is_stopping() {
            let t = cpus.entering_guest();
            let exit = vcpu.run();
            cpus.left_guest(idx, t);
            match exit? {
                Exit::MmioRead { addr, data } => cpus.mmio_read(idx, addr, data),
                Exit::MmioWrite { addr, data } => cpus.mmio_write(idx, addr, data),
                Exit::Interrupted => {
                    stats.canceled.fetch_add(1, Ordering::Relaxed);
                }
                Exit::SystemEvent(event) => {
                    cpus.request_stop(stop_reason(event, idx));
                    return Ok(());
                }
                Exit::Other(what) => {
                    cpus.request_stop(StopReason::Fault(format!("vcpu{idx}: {what}")));
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

fn stop_reason(event: SystemEvent, idx: usize) -> StopReason {
    match event {
        SystemEvent::Shutdown => StopReason::PowerOff,
        SystemEvent::Reset => StopReason::Reset,
        SystemEvent::Crash => StopReason::Fault(format!("vcpu{idx}: the guest reported a crash (PSCI system event)")),
        SystemEvent::Other(kind) => StopReason::Fault(format!("vcpu{idx}: unexpected system event {kind}")),
    }
}

impl ForceExit for Hypervisor {
    /// Kicks every running vCPU, and keeps kicking from a helper thread until none is left: the
    /// caller may be a vCPU thread (a guest power-off), which mustn't wait for itself.
    fn force_exit_all(&self) {
        if self.kicks.kick_running() == 0 || self.kicking.swap(true, Ordering::SeqCst) {
            return;
        }
        let kicks = self.kicks.clone();
        let _ = std::thread::Builder::new().name("vcpu-kick".into()).spawn(move || {
            while kicks.kick_running() > 0 {
                std::thread::sleep(KICK_RETRY);
            }
        });
    }
}

impl Drop for Hypervisor {
    fn drop(&mut self) {
        if let Some((stop, thread)) = self.notifies.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = rustix::io::write(&stop, &1u64.to_ne_bytes());
            let _ = thread.join();
        }
        let mut current = CURRENT.lock().unwrap_or_else(PoisonError::into_inner);
        if current.as_ref().is_some_and(|vm| Arc::ptr_eq(vm, &self.vm)) {
            *current = None;
        }
    }
}

type Callback = Arc<dyn Fn() + Send + Sync>;

/// Waits on every notifier and `stop` at once; runs a notifier's callback each time it fires.
fn dispatch(notifies: &[(Notifier, Callback)], stop: &rustix::fd::OwnedFd) -> std::io::Result<()> {
    use rustix::event::epoll;
    let ep = epoll::create(epoll::CreateFlags::CLOEXEC)?;
    let stop_key = notifies.len() as u64;
    epoll::add(&ep, stop, epoll::EventData::new_u64(stop_key), epoll::EventFlags::IN)?;
    for (i, (fd, _)) in notifies.iter().enumerate() {
        epoll::add(&ep, fd.as_fd(), epoll::EventData::new_u64(i as u64), epoll::EventFlags::IN)?;
    }
    let mut events = Vec::with_capacity(notifies.len() + 1);
    loop {
        events.clear();
        match epoll::wait(&ep, rustix::buffer::spare_capacity(&mut events), None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        for ev in &events {
            let key = ev.data.u64();
            if key == stop_key {
                return Ok(());
            }
            if let Some((fd, on)) = notifies.get(key as usize)
                && fd.drain()
            {
                on();
            }
        }
    }
}

/// An SPI on the vGIC, driven with `KVM_IRQ_LINE`.
struct KvmSpi {
    vm: Arc<Vm>,
    irq: u32,
    intid: u32,
}

impl IrqLine for KvmSpi {
    fn set(&self, level: bool) {
        if let Err(e) = self.vm.set_irq_line(self.irq, level) {
            eprintln!("emu: SPI {} -> {level}: {e}", self.intid);
        }
    }
}

/// Maps host memory the VMM doesn't own (a GPU renderer's host-visible allocation) at `ipa`.
/// Callable from any thread while the VM exists.
pub fn map_raw(host: HostRange, ipa: u64) -> Result<(), Error> {
    current()?.map_raw(host, ipa)
}

pub fn unmap_raw(ipa: u64, len: usize) -> Result<(), Error> {
    current()?.unmap_raw(ipa, len)
}

fn current() -> Result<Arc<Vm>, Error> {
    CURRENT.lock().unwrap_or_else(PoisonError::into_inner).clone().ok_or(Error::Invalid("no VM is running"))
}
