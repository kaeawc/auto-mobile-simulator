//! A vCPU and its exits.
use kvm_ioctls::{VcpuExit, VcpuFd};

use crate::{Error, Result, kvm_err};

/// Why [`Vcpu::run`] returned. The kernel has already decoded MMIO and handled WFI, the timer,
/// PSCI CPU_ON/OFF/SUSPEND and the GIC itself, so only device accesses and power events remain.
#[derive(Debug)]
pub enum Exit<'a> {
    /// A guest load from device address `addr`: fill `data` (1 to 8 bytes, little-endian)
    /// before the next run.
    MmioRead { addr: u64, data: &'a mut [u8] },
    /// A guest store of `data` to device address `addr`.
    MmioWrite { addr: u64, data: &'a [u8] },
    /// PSCI SYSTEM_OFF, SYSTEM_RESET or a guest crash report.
    SystemEvent(SystemEvent),
    /// A signal interrupted the run ([`crate::Kicks`]): look at why, then run again.
    Interrupted,
    /// An x86 port write (tests drive tiny real-mode guests with these).
    #[cfg(target_arch = "x86_64")]
    IoOut { port: u16, data: &'a [u8] },
    /// The guest halted (x86).
    #[cfg(target_arch = "x86_64")]
    Hlt,
    /// Anything else, described: the machine stops with it as the fault.
    Other(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemEvent {
    Shutdown,
    Reset,
    Crash,
    Other(u32),
}

impl SystemEvent {
    fn from_kvm(kind: u32) -> SystemEvent {
        match kind {
            kvm_bindings::KVM_SYSTEM_EVENT_SHUTDOWN => SystemEvent::Shutdown,
            kvm_bindings::KVM_SYSTEM_EVENT_RESET => SystemEvent::Reset,
            kvm_bindings::KVM_SYSTEM_EVENT_CRASH => SystemEvent::Crash,
            other => SystemEvent::Other(other),
        }
    }
}

/// A virtual CPU. Any thread may run it, one at a time (`run` takes `&mut self`).
pub struct Vcpu {
    pub(crate) fd: VcpuFd,
}

/// PSCI 0.2 (`KVM_REG_ARM_PSCI_VERSION` encoding: major in [31:16], minor in [15:0]).
#[cfg(target_arch = "aarch64")]
pub const KVM_PSCI_0_2: u64 = 0x2;

/// arm64 core registers (`struct kvm_regs` offsets, for `KVM_{GET,SET}_ONE_REG`).
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreReg {
    /// X0..=X30.
    X(u8),
    Sp,
    Pc,
    Pstate,
}

#[cfg(target_arch = "aarch64")]
impl CoreReg {
    /// The `KVM_REG_ARM_CORE` id: the register's offset in `struct kvm_regs` in 32-bit words.
    pub fn id(self) -> u64 {
        let words = match self {
            CoreReg::X(n) => {
                assert!(n <= 30, "no such general purpose register");
                u64::from(n) * 2
            }
            CoreReg::Sp => 62,
            CoreReg::Pc => 64,
            CoreReg::Pstate => 66,
        };
        kvm_bindings::KVM_REG_ARM64 | kvm_bindings::KVM_REG_SIZE_U64 | u64::from(kvm_bindings::KVM_REG_ARM_CORE) | words
    }
}

impl Vcpu {
    pub(crate) fn new(fd: VcpuFd) -> Vcpu {
        Vcpu { fd }
    }

    /// Initializes the vCPU as the host's preferred CPU type, with PSCI 0.2 handled in the
    /// kernel. A `powered_off` vCPU waits for PSCI CPU_ON from another vCPU (the kernel starts
    /// it); the boot vCPU starts at the reset state, EL1h with DAIF masked.
    #[cfg(target_arch = "aarch64")]
    pub fn init(&self, vm: &crate::Vm, powered_off: bool) -> Result<()> {
        let mut kvi = kvm_bindings::kvm_vcpu_init::default();
        vm.fd().get_preferred_target(&mut kvi).map_err(kvm_err("KVM_ARM_PREFERRED_TARGET"))?;
        kvi.features[0] |= 1 << kvm_bindings::KVM_ARM_VCPU_PSCI_0_2;
        if powered_off {
            kvi.features[0] |= 1 << kvm_bindings::KVM_ARM_VCPU_POWER_OFF;
        }
        self.fd.vcpu_init(&kvi).map_err(kvm_err("KVM_ARM_VCPU_INIT"))
    }

    #[cfg(target_arch = "aarch64")]
    pub fn set_core_reg(&self, reg: CoreReg, value: u64) -> Result<()> {
        self.fd.set_one_reg(reg.id(), &value.to_le_bytes()).map(drop).map_err(kvm_err("KVM_SET_ONE_REG"))
    }

    #[cfg(target_arch = "aarch64")]
    pub fn core_reg(&self, reg: CoreReg) -> Result<u64> {
        let mut b = [0u8; 8];
        self.fd.get_one_reg(reg.id(), &mut b).map_err(kvm_err("KVM_GET_ONE_REG"))?;
        Ok(u64::from_le_bytes(b))
    }

    /// The PSCI version the kernel reports to the guest (`KVM_REG_ARM_PSCI_VERSION`); set it
    /// before the first run.
    #[cfg(target_arch = "aarch64")]
    pub fn set_psci_version(&self, version: u64) -> Result<()> {
        let id = kvm_bindings::KVM_REG_ARM64 | kvm_bindings::KVM_REG_SIZE_U64 | u64::from(kvm_bindings::KVM_REG_ARM_FW);
        self.fd.set_one_reg(id, &version.to_le_bytes()).map(drop).map_err(kvm_err("KVM_SET_ONE_REG(PSCI version)"))
    }

    /// Enters the guest until it does something the kernel leaves to the VMM, or a signal
    /// interrupts it.
    pub fn run(&mut self) -> Result<Exit<'_>> {
        match self.fd.run() {
            Ok(VcpuExit::MmioRead(addr, data)) => Ok(Exit::MmioRead { addr, data }),
            Ok(VcpuExit::MmioWrite(addr, data)) => Ok(Exit::MmioWrite { addr, data }),
            Ok(VcpuExit::SystemEvent(kind, _)) => Ok(Exit::SystemEvent(SystemEvent::from_kvm(kind))),
            Ok(VcpuExit::Intr) => Ok(Exit::Interrupted),
            #[cfg(target_arch = "x86_64")]
            Ok(VcpuExit::IoOut(port, data)) => Ok(Exit::IoOut { port, data }),
            #[cfg(target_arch = "x86_64")]
            Ok(VcpuExit::Hlt) => Ok(Exit::Hlt),
            Ok(VcpuExit::FailEntry(reason, cpu)) => Ok(Exit::Other(format!("entry failed (reason {reason:#x}, cpu {cpu})"))),
            Ok(VcpuExit::InternalError) => Ok(Exit::Other("KVM internal error".into())),
            Ok(other) => Ok(Exit::Other(format!("unexpected exit {other:?}"))),
            Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => Ok(Exit::Interrupted),
            Err(e) => Err(match e.errno() {
                // A data abort without a valid syndrome (a load/store pair to a device, say):
                // nothing the VMM can emulate.
                libc::ENOSYS => Error::Invalid("the guest accessed a device with an instruction KVM can't decode (no ISV)"),
                _ => kvm_err("KVM_RUN")(e),
            }),
        }
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;

    #[test]
    fn core_register_ids_follow_struct_kvm_regs() {
        // Values from the arm64 uapi: KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE | off/4.
        assert_eq!(CoreReg::X(0).id(), 0x6030_0000_0010_0000);
        assert_eq!(CoreReg::X(30).id(), 0x6030_0000_0010_003c);
        assert_eq!(CoreReg::Pc.id(), 0x6030_0000_0010_0040);
        assert_eq!(CoreReg::Pstate.id(), 0x6030_0000_0010_0042);
    }
}
