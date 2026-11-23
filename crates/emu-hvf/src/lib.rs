//! Safe wrappers over Apple's Hypervisor.framework on arm64.
//!
//! This is the only crate in the workspace that contains `unsafe`. It exposes:
//! - [`Vm`]: the process-wide virtual machine and its in-kernel GICv3,
//! - [`GuestRam`]: host memory mapped into the guest, with bounds-checked accessors,
//! - [`Vcpu`]: a virtual CPU, pinned to the thread that created it,
//! - [`VcpuHandle`]: a sendable token used to kick a vCPU out of the guest,
//! - [`host`]: host file syscalls std doesn't wrap (hole punching),
//! - [`SharedMap`]: a file mapped shared, which the display server's frame ring lives in.
//!
//! Guest RAM, [`host`] and [`SharedMap`] build on any unix host, so the device models above them build and test
//! on Linux; the hypervisor itself is Apple silicon only.
#![cfg(unix)]

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod ffi;
pub mod host;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod hv;
mod ram;
mod shared;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use hv::*;
pub use ram::{GuestRam, HostRange, MemError};
pub use shared::SharedMap;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{call} failed: {}", describe(*.code))]
    Hv { call: &'static str, code: i32 },
    #[error("a VM already exists in this process")]
    AlreadyCreated,
    #[error("mmap of {0} bytes failed")]
    Mmap(usize),
    #[error("{0}")]
    Invalid(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

fn describe(code: i32) -> String {
    let name = match code as u32 {
        0xfae9_4001 => "HV_ERROR",
        0xfae9_4002 => "HV_BUSY",
        0xfae9_4003 => "HV_BAD_ARGUMENT",
        0xfae9_4004 => "HV_ILLEGAL_GUEST_STATE",
        0xfae9_4005 => "HV_NO_RESOURCES",
        0xfae9_4006 => "HV_NO_DEVICE",
        0xfae9_4007 => "HV_DENIED (binary is missing the com.apple.security.hypervisor entitlement)",
        0xfae9_400f => "HV_UNSUPPORTED",
        _ => "unknown",
    };
    format!("{name} ({:#x})", code as u32)
}
