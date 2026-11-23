//! The emu-rs virtual machine: an arm64 board that boots unmodified Android emulator system
//! images on Hypervisor.framework.
//!
//! The device models build and test on any unix host; the machine that runs them (vCPUs, the GIC,
//! the GPU renderer) needs Hypervisor.framework on Apple silicon.
#![cfg(unix)]

pub mod board;
pub mod boot;
pub mod bus;
pub mod devices;
pub mod disk;
pub mod display;
pub mod irq;
pub mod layout;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod machine;
pub mod modem;
pub mod pipe;
pub mod telephony;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod vcpu;
pub mod virtio;

pub use disk::{DiskMode, DiskSpec};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use machine::*;
pub use virtio::input::Inputs;
