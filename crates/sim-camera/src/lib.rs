//! Camera sources for simee devices: the host's cameras (built-in, external, Continuity Camera,
//! and virtual cameras: CoreMediaIO camera extensions such as OBS Virtual Camera on macOS,
//! v4l2loopback on Linux), still images and synthetic patterns, under one source id scheme.
//!
//! - [`catalog`] lists the host's cameras (never needs the camera permission) and the synthetic
//!   kinds, and diffs two listings into hot-plug events.
//! - [`id`] parses source ids (`webcam:<id or name>`, `pattern:testcard`, `qr:<text>`, …) and
//!   resolves a webcam query by id, name, index or part of the name.
//! - [`Source`] produces BGRA frames at a requested size; [`Hub`] runs one capture per source and
//!   fans it out to every subscriber, stopping it when the last one goes.
//! - [`tcc`] reads the host camera permission (never prompting) and turns a missing grant into an
//!   error that names the fix, instead of black frames.
//!
//! Frames are plain BGRA buffers shared through an `Arc`. A consumer that needs an IOSurface (a
//! VideoToolbox encoder, the iOS feed) copies one in with `sim_coresim::videotoolbox`'s
//! `PixelBufferPool::fill`, the one copy on that path; this crate doesn't link VideoToolbox.

#[cfg(target_os = "macos")]
mod avf;
pub mod catalog;
pub mod draw;
mod error;
pub mod frame;
pub mod hub;
pub mod id;
pub mod source;
pub mod synth;
pub mod tcc;
#[cfg(target_os = "linux")]
mod v4l2;

pub use catalog::{CameraKind, Change, Format, HostCamera, host_cameras};
pub use error::{Error, Result};
pub use frame::{CameraFrame, FrameSpec, Tick};
pub use hub::{Hub, Subscription};
pub use id::{Pattern, SourceSpec, resolve_webcam};
pub use source::{HostOpener, Opener, Source};
pub use tcc::Authorization;
