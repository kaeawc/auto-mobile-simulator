//! Safe wrappers over Linux KVM, on the rust-vmm `kvm-ioctls` crate. The KVM backend's FFI
//! boundary: every `unsafe` block for KVM lives here. It exposes:
//!
//! - [`open`]: `/dev/kvm`, checked up front, with an error that says how to get access;
//! - [`Vm`]: guest memory slots ([`Vm::map_ram`], [`Vm::map_raw`]), interrupt lines
//!   ([`Vm::set_irq_line`], [`Vm::irqfd`]), MMIO notifications that never leave the kernel
//!   ([`Vm::ioeventfd`]), and on arm64 the in-kernel vGICv3 ([`Vm::create_vgic_v3`]);
//! - [`Vcpu`]: one vCPU and its exits, already decoded by the kernel ([`Exit`]);
//! - [`Kicks`]: forcing vCPU threads out of `KVM_RUN` with a signal.
//!
//! Linux only; the crate is empty elsewhere.
#![cfg(target_os = "linux")]

mod access;
mod kick;
mod vcpu;
mod vm;

pub use access::{AccessProblem, KVM_PATH, diagnose};
pub use kick::Kicks;
#[cfg(target_arch = "aarch64")]
pub use vcpu::{CoreReg, KVM_PSCI_0_2};
pub use vcpu::{Exit, SystemEvent, Vcpu};
#[cfg(target_arch = "aarch64")]
pub use vm::Vgic;
pub use vm::{Notifier, Vm};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `/dev/kvm` can't be opened; the problem says why and how to fix it.
    #[error("{0}")]
    Access(AccessProblem),
    #[error("{call} failed: {err}")]
    Kvm { call: &'static str, err: std::io::Error },
    /// The host's KVM lacks something the machine needs.
    #[error("this host's KVM doesn't support {0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    Invalid(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

fn kvm_err(call: &'static str) -> impl FnOnce(vmm_sys_util::errno::Error) -> Error {
    move |e| Error::Kvm { call, err: std::io::Error::from_raw_os_error(e.errno()) }
}

/// The opened `/dev/kvm`.
pub struct Kvm {
    kvm: kvm_ioctls::Kvm,
}

/// Opens `/dev/kvm` and checks what every machine needs: the stable API, user memory, ioeventfd,
/// irqfd and an in-kernel interrupt controller. An unusable device is an [`Error::Access`] that
/// says how to fix it (load the module, join the `kvm` group); no root needed once that's done.
pub fn open() -> Result<Kvm> {
    let kvm = kvm_ioctls::Kvm::new().map_err(|e| Error::Access(diagnose(std::path::Path::new(KVM_PATH), e.errno())))?;
    use kvm_ioctls::Cap;
    for (cap, what) in [
        (Cap::UserMemory, "user memory regions (KVM_CAP_USER_MEMORY)"),
        (Cap::Ioeventfd, "ioeventfd (KVM_CAP_IOEVENTFD)"),
        (Cap::Irqfd, "irqfd (KVM_CAP_IRQFD)"),
        (Cap::ImmediateExit, "immediate exits (KVM_CAP_IMMEDIATE_EXIT)"),
    ] {
        if !kvm.check_extension(cap) {
            return Err(Error::Unsupported(what));
        }
    }
    #[cfg(target_arch = "aarch64")]
    if !kvm.check_extension(Cap::ArmPsci02) {
        return Err(Error::Unsupported("in-kernel PSCI 0.2 (KVM_CAP_ARM_PSCI_0_2)"));
    }
    Ok(Kvm { kvm })
}

impl Kvm {
    /// Most vCPUs one VM may have on this host.
    pub fn max_vcpus(&self) -> usize {
        self.kvm.get_max_vcpus()
    }

    /// Most guest physical address bits a VM can have (arm64; 0 when KVM picks the default 40).
    #[cfg(target_arch = "aarch64")]
    pub fn ipa_limit(&self) -> u32 {
        u32::try_from(self.kvm.get_host_ipa_limit()).unwrap_or(0)
    }

    /// Creates a VM. On arm64 its guest physical address space has `ipa_bits` bits, clamped to
    /// what the host supports.
    pub fn create_vm(&self, ipa_bits: u32) -> Result<Vm> {
        #[cfg(target_arch = "aarch64")]
        let fd = {
            let limit = self.ipa_limit();
            if limit == 0 {
                self.kvm.create_vm().map_err(kvm_err("KVM_CREATE_VM"))?
            } else {
                self.kvm.create_vm_with_ipa_size(ipa_bits.min(limit)).map_err(kvm_err("KVM_CREATE_VM"))?
            }
        };
        #[cfg(not(target_arch = "aarch64"))]
        let fd = {
            let _ = ipa_bits;
            self.kvm.create_vm().map_err(kvm_err("KVM_CREATE_VM"))?
        };
        Ok(Vm::new(fd, self.kvm.get_nr_memslots()))
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod x86_tests;
