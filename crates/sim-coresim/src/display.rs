//! The device's framebuffer: a shared surface the simulated OS renders into.

use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, sel};
use sim_core::pixels::bgra_to_rgba;

use crate::error::{Error, Result};
use crate::rt::{self, Obj};

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceLock(surface: *const AnyObject, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: *const AnyObject, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: *const AnyObject) -> *mut c_void;
    fn IOSurfaceGetBytesPerRow(surface: *const AnyObject) -> usize;
    fn IOSurfaceGetWidth(surface: *const AnyObject) -> usize;
    fn IOSurfaceGetHeight(surface: *const AnyObject) -> usize;
    fn IOSurfaceGetAllocSize(surface: *const AnyObject) -> usize;
    fn IOSurfaceGetPixelFormat(surface: *const AnyObject) -> u32;
    fn IOSurfaceGetSeed(surface: *const AnyObject) -> u32;
}

const LOCK_READ_ONLY: u32 = 1;
const FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

/// One captured frame: tightly packed opaque RGBA.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A retained handle to the framebuffer `IOSurface`, for zero-copy display. The simulated device
/// keeps rendering into it, so readers see live (possibly mid-update) contents.
pub struct Surface(Obj);

impl Surface {
    /// The raw `IOSurfaceRef`, valid while this handle lives.
    pub fn as_raw(&self) -> *mut c_void {
        Retained::as_ptr(&self.0).cast_mut().cast()
    }

    /// A counter that changes whenever the contents change.
    pub fn seed(&self) -> u32 {
        // SAFETY: `self.0` is a live, retained IOSurface.
        unsafe { IOSurfaceGetSeed(&*self.0) }
    }

    /// Size in pixels.
    pub fn size(&self) -> (u32, u32) {
        // SAFETY: `self.0` is a live, retained IOSurface.
        unsafe { (IOSurfaceGetWidth(&*self.0) as u32, IOSurfaceGetHeight(&*self.0) as u32) }
    }
}

/// A device display backed by a framebuffer surface.
pub struct Display {
    descriptor: Obj,
}

impl Display {
    /// Finds the main display among the device's I/O ports.
    pub(crate) fn main(device: &AnyObject) -> Result<Self> {
        let io = rt::object_prop(device, sel!(io))?;
        if rt::responds(&io, sel!(updateIOPorts)) {
            // SAFETY: `updateIOPorts` takes no arguments and returns void (checked to exist).
            unsafe { msg_send![&*io, updateIOPorts] }
        }
        let ports = rt::object_prop(&io, sel!(ioPorts))?;
        let mut fallback = None;
        for port in rt::array_items(&ports) {
            let Ok(descriptor) = rt::object_prop(&port, sel!(descriptor)) else { continue };
            if !rt::responds(&descriptor, sel!(framebufferSurface)) {
                continue;
            }
            if display_class(&descriptor) == Some(0) {
                return Ok(Self { descriptor });
            }
            fallback.get_or_insert(descriptor);
        }
        fallback.map(|descriptor| Self { descriptor }).ok_or_else(|| Error::NoDisplay("no framebuffer port; is the device booted?".into()))
    }

    /// The current framebuffer surface (an `IOSurface`), if the device has rendered one.
    pub fn surface(&self) -> Option<Obj> {
        rt::object_prop(&self.descriptor, sel!(framebufferSurface)).ok()
    }

    /// A retained handle to the current framebuffer surface, if the device has rendered one.
    pub fn surface_handle(&self) -> Option<Surface> {
        self.surface().map(Surface)
    }

    /// A counter that changes whenever the surface contents change. 0 without a surface.
    pub fn seed(&self) -> u32 {
        // SAFETY: the surface is a live IOSurface for the duration of the call.
        self.surface().map_or(0, |s| unsafe { IOSurfaceGetSeed(&*s) })
    }

    /// Framebuffer size in pixels.
    pub fn size(&self) -> Result<(u32, u32)> {
        let surface = self.surface().ok_or_else(|| Error::NoDisplay("no framebuffer surface yet".into()))?;
        // SAFETY: the surface is a live IOSurface for the duration of the calls.
        let (w, h) = unsafe { (IOSurfaceGetWidth(&*surface), IOSurfaceGetHeight(&*surface)) };
        Ok((w as u32, h as u32))
    }

    /// Copies the current framebuffer contents.
    pub fn capture(&self) -> Result<Frame> {
        let surface = self.surface().ok_or_else(|| Error::NoDisplay("no framebuffer surface yet".into()))?;
        let s: *const AnyObject = &*surface;
        // SAFETY: `s` is a live IOSurface (retained by `surface`) for all calls below.
        let format = unsafe { IOSurfaceGetPixelFormat(s) };
        if format != FORMAT_BGRA {
            return Err(Error::NoDisplay(format!("unsupported pixel format {:?}", String::from_utf8_lossy(&format.to_be_bytes()))));
        }
        // SAFETY: as above; a null seed pointer is allowed.
        if unsafe { IOSurfaceLock(s, LOCK_READ_ONLY, std::ptr::null_mut()) } != 0 {
            return Err(Error::NoDisplay("cannot lock framebuffer".into()));
        }
        // SAFETY: the surface is locked, so its geometry and base address are stable.
        let (width, height, stride, len, base) = unsafe {
            (IOSurfaceGetWidth(s), IOSurfaceGetHeight(s), IOSurfaceGetBytesPerRow(s), IOSurfaceGetAllocSize(s), IOSurfaceGetBaseAddress(s))
        };
        let rgba = if base.is_null() {
            None
        } else {
            // SAFETY: while locked, `base` points to `len` readable bytes owned by the surface; the
            // slice does not outlive the lock.
            bgra_to_rgba(unsafe { std::slice::from_raw_parts(base.cast::<u8>(), len) }, width, height, stride)
        };
        // SAFETY: balances the successful lock above.
        unsafe { IOSurfaceUnlock(s, LOCK_READ_ONLY, std::ptr::null_mut()) };
        let rgba = rgba.ok_or_else(|| Error::NoDisplay("framebuffer geometry is inconsistent".into()))?;
        Ok(Frame { width: width as u32, height: height as u32, rgba })
    }
}

fn display_class(descriptor: &AnyObject) -> Option<u16> {
    let state = rt::object_prop(descriptor, sel!(state)).ok()?;
    rt::send_u16(&state, sel!(displayClass))
}
