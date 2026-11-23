//! Safe API over the host simulator service.
//!
//! The service's frameworks are private and loaded at runtime; every call goes through the
//! Objective-C runtime by selector, so a missing class or method surfaces as [`Error::Unavailable`]
//! instead of a link failure. All types here are main-thread-agnostic but not `Send`: keep a
//! [`Context`] and everything derived from it on one thread.
//!
//! The service exists only on macOS, so elsewhere this crate is empty (like `emu-hvf`): callers
//! gate their iOS backend on `target_os = "macos"` and the shared core still builds and tests.
#![cfg(target_os = "macos")]

mod carplay;
mod display;
mod error;
mod hid;
mod rt;
mod service;

pub use display::{Display, Surface};
pub use error::{Error, Result};
pub use hid::Hid;
pub use service::{Appearance, Context, Device, ExecOutput, StatusBar, developer_dir};
pub use sim_core::frame::Frame;
pub use sim_core::hid::{Direction, Orientation, WatchGesture};
