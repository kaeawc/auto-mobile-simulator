//! The simee virtual machine: an arm64 board that boots unmodified Android emulator system
//! images on Hypervisor.framework.
//!
//! The device models and the vCPU threads' shared half ([`vcpu`]) build and test on any unix host;
//! the machine that runs them needs a hypervisor backend ([`hv`]: Hypervisor.framework on Apple
//! silicon) and the GPU renderer.
#![cfg(unix)]

pub mod acpi;
pub mod audio;
pub mod board;
pub mod boot;
pub mod bt;
pub mod bus;
pub mod devices;
pub mod disk;
pub mod display;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod hv;
pub mod irq;
pub mod layout;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod machine;
pub mod modem;
pub mod net;
pub mod pipe;
#[cfg(target_os = "linux")]
pub mod soft;
pub mod telephony;
pub mod vcpu;
#[cfg(target_os = "linux")]
pub mod vfio_user;
#[cfg(target_os = "linux")]
pub mod vhost_user;
pub mod virtio;
pub mod wake;

pub use disk::{DiskMode, DiskSpec};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use machine::*;
pub use virtio::input::Inputs;

/// Called first on every device worker thread: user-initiated QoS, as the vCPUs run. A thread
/// doesn't inherit its spawner's QoS on Darwin (`emu_hvf::host` pins this), so a worker started
/// from a vCPU thread would otherwise run at the default class.
pub(crate) fn device_thread() {
    emu_hvf::host::set_qos(emu_hvf::host::Qos::UserInitiated);
}
