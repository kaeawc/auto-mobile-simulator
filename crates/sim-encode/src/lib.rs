//! Real-time H.264 from simee's frame sources, as Annex B access units with presentation times
//! stamped at capture: what `simee-rtc`'s publisher sends.
//!
//! - [`annexb`]: VideoToolbox's length-prefixed samples to Annex B, and the cached SPS/PPS put in
//!   front of every IDR.
//! - [`pacing`]: when to encode: the source's frame callbacks, an fps cap, an idle heartbeat that
//!   re-encodes the last frame on a static screen, and strictly increasing timestamps.
//! - [`stream`]: the loop that ties a [`stream::Source`] to an [`stream::Encoder`], with the
//!   keyframe and bitrate [`stream::Controls`] a publisher's feedback drives.
//! - [`EncoderConfig`]: profile (Constrained Baseline or Main), hardware policy, fps, bitrate and
//!   its ceiling, keyframe interval, the largest output size.
//! - `vt` (macOS): [`VtEncoder`], a real-time `VTCompressionSession` (no frame reordering), and
//!   [`Decoder`], which checks that output decodes. `source` (macOS): [`IosSource`] wraps the
//!   device's framebuffer `IOSurface` as the pixel buffer (no copy, no colour conversion on the
//!   CPU); [`AndroidSource`] copies emu's RGBA frames into an IOSurface-backed pool (one copy).
//!
//! Elsewhere there is no encoder yet ([`Error::Unsupported`]); the pure parts build and test
//! everywhere. Design and measurements: `docs/streaming.md`.

pub mod annexb;
mod config;
pub mod pacing;
#[cfg(target_os = "macos")]
mod source;
pub mod stream;
#[cfg(target_os = "macos")]
mod vt;

pub use config::{EncoderConfig, Hardware, Profile, output_size};
#[cfg(target_os = "macos")]
pub use source::{AndroidSource, IosSource};
pub use stream::{AccessUnit, Controls, Encoder, Flow, Source, Stats};
#[cfg(target_os = "macos")]
pub use vt::{Decoder, VtEncoder};

/// Whether this build has an encoder: VideoToolbox on macOS, nothing elsewhere yet.
pub fn available() -> bool {
    cfg!(target_os = "macos")
}

/// What can go wrong encoding.
#[derive(Debug)]
pub enum Error {
    /// No encoder on this platform, or none that meets the configuration (no hardware encoder
    /// when one is required, Constrained Baseline before macOS 12).
    Unsupported(String),
    /// A malformed argument or bitstream.
    InvalidInput(String),
    /// The encoder (or decoder) refused or failed.
    Encoder(String),
    /// The frame source is gone or broken: the device stopped, its display socket closed.
    Source(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::InvalidInput(m) => write!(f, "invalid input: {m}"),
            Error::Encoder(m) => write!(f, "encoder: {m}"),
            Error::Source(m) => write!(f, "frame source: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T, E = Error> = std::result::Result<T, E>;
