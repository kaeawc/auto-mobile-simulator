//! The device's framebuffer: a shared surface the simulated OS renders into.

use std::ffi::c_void;

use block2::{Block, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{class, msg_send, sel};
use sim_core::frame::Frame;
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
    /// Finds the main display among the device's I/O ports: the class-0 framebuffer, else the
    /// first one (Apple TV renders only to its TVOut port).
    pub(crate) fn main(device: &AnyObject) -> Result<Self> {
        let ports = framebuffer_ports(device)?;
        let main = ports.iter().position(|d| display_class(d) == Some(0)).unwrap_or(0);
        ports
            .into_iter()
            .nth(main)
            .map(|descriptor| Self { descriptor })
            .ok_or_else(|| Error::NoDisplay("no framebuffer port; is the device booted?".into()))
    }

    /// Finds an external display (an iPhone's TVOut screen, the CarPlay display once attached):
    /// a framebuffer that isn't class 0 and has rendered a surface.
    pub(crate) fn external(device: &AnyObject) -> Result<Self> {
        framebuffer_ports(device)?
            .into_iter()
            .find(|d| display_class(d).is_some_and(|c| c != 0) && rt::object_prop(d, sel!(framebufferSurface)).is_ok())
            .map(|descriptor| Self { descriptor })
            .ok_or_else(|| Error::NoDisplay("no external display has rendered yet; attach CarPlay first".into()))
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

    /// Calls `on_frame` whenever the device presents a frame, until the returned watch drops. It is
    /// the port's damage-rectangle callback (Xcode 9 on; modern runtimes call it once per frame,
    /// with no rectangles), what idb's lazy video stream follows. `on_frame` runs on a service
    /// thread while the service waits: keep it to a flag and a wakeup.
    pub fn watch_frames(&self, on_frame: impl Fn() + Send + Sync + 'static) -> Result<FrameWatch> {
        let register = sel!(registerCallbackWithUUID:damageRectanglesCallback:);
        rt::require(&self.descriptor, register)?;
        rt::require(&self.descriptor, sel!(unregisterDamageRectanglesCallbackWithUUID:))?;
        // SAFETY: `+[NSUUID UUID]` returns a new random UUID.
        let uuid: Obj = unsafe { msg_send![class!(NSUUID), UUID] };
        let block = RcBlock::new(move |_rects: *mut AnyObject| on_frame());
        type RegisterFn = unsafe extern "C" fn(*const AnyObject, Sel, *const AnyObject, *const Block<dyn Fn(*mut AnyObject)>);
        // SAFETY: the method's signature is (NSUUID *, void (^)(NSArray *)) -> void; the descriptor
        // is a remote proxy, so it's called through `objc_msgSend`, not `msg_send!`.
        let send: RegisterFn = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
        // SAFETY: the descriptor responds to the selector (checked); the service copies the block,
        // and the watch keeps ours alive until it unregisters.
        unsafe { send(&*self.descriptor, register, &*uuid, &*block) };
        Ok(FrameWatch { descriptor: self.descriptor.clone(), uuid, _block: block })
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

/// A registered frame callback ([`Display::watch_frames`]); dropping it unregisters the callback.
pub struct FrameWatch {
    descriptor: Obj,
    uuid: Obj,
    _block: RcBlock<dyn Fn(*mut AnyObject)>,
}

impl Drop for FrameWatch {
    fn drop(&mut self) {
        type UnregisterFn = unsafe extern "C" fn(*const AnyObject, Sel, *const AnyObject);
        // SAFETY: the method's signature is (NSUUID *) -> void; called through `objc_msgSend` as
        // the descriptor is a remote proxy.
        let send: UnregisterFn = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
        // SAFETY: the descriptor responds to the selector (checked at registration); `uuid` is the
        // registration's.
        unsafe { send(&*self.descriptor, sel!(unregisterDamageRectanglesCallbackWithUUID:), &*self.uuid) };
    }
}

/// The descriptors of the device's framebuffer ports, in port order.
fn framebuffer_ports(device: &AnyObject) -> Result<Vec<Obj>> {
    let io = rt::object_prop(device, sel!(io))?;
    if rt::responds(&io, sel!(updateIOPorts)) {
        // SAFETY: `updateIOPorts` takes no arguments and returns void (checked to exist).
        unsafe { msg_send![&*io, updateIOPorts] }
    }
    let ports = rt::object_prop(&io, sel!(ioPorts))?;
    Ok(rt::array_items(&ports)
        .iter()
        .filter_map(|port| rt::object_prop(port, sel!(descriptor)).ok())
        .filter(|d| rt::responds(d, sel!(framebufferSurface)))
        .collect())
}

fn display_class(descriptor: &AnyObject) -> Option<u16> {
    let state = rt::object_prop(descriptor, sel!(state)).ok()?;
    rt::send_u16(&state, sel!(displayClass))
}
