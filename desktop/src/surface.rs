//! Zero-copy bridge: wraps the device framebuffer `IOSurface` as a pixel buffer Rune samples on the
//! GPU directly. The only module allowed to use `unsafe`.

#![allow(unsafe_code)]

use rune::native_surface::ColorInfo;
use rune::native_surface::macos::PixelBuffer;
use sim_coresim::Surface;

/// Wraps `surface` without copying. The buffer retains the surface.
pub fn wrap(surface: &Surface) -> Result<PixelBuffer, String> {
    // SAFETY: `as_raw` is a live, retained BGRA IOSurfaceRef for the duration of the call, and the
    // pixel buffer takes its own retain. Rune asks for the surface to stay immutable while the GPU
    // reads it; the simulated device keeps rendering into it, exactly as the stock simulator shows
    // it. A concurrent write can only show a partially updated frame on screen: the memory stays
    // mapped and valid, and no Rust reference ever aliases it.
    unsafe { PixelBuffer::from_iosurface(surface.as_raw(), ColorInfo::default()) }
}
