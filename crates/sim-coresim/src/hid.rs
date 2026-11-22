//! HID input: touches, hardware buttons and keyboard keys.

use std::ffi::c_void;
use std::ptr;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{msg_send, sel};
use objc2_foundation::NSError;
use sim_core::geometry::Point;
use sim_core::gesture::Phase;
use sim_core::hid::Button;

use crate::error::{Error, Result};
use crate::rt::{self, CGPoint, Library, Obj};

/// Key or button direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Down,
    Up,
}

impl Direction {
    fn raw(self) -> u32 {
        match self {
            Self::Down => 1,
            Self::Up => 2,
        }
    }
}

/// Opaque HID message allocated by the message builders and freed by the client after sending.
type Message = *mut c_void;

type MouseFn = unsafe extern "C" fn(*const CGPoint, *const CGPoint, u32, u64, u64, f64, f64) -> Message;
type ButtonFn = unsafe extern "C" fn(u32, u32, u32) -> Message;
type KeyboardFn = unsafe extern "C" fn(u32, u32) -> Message;
type SendFn = unsafe extern "C" fn(*const AnyObject, Sel, Message, bool, *const AnyObject, *const c_void);

/// Touch digitizer target.
const TARGET_TOUCH: u32 = 0x32;
/// Hardware button target.
const TARGET_HARDWARE: u32 = 0x33;

/// A HID session with one booted device.
pub struct Hid {
    client: Obj,
    mouse: MouseFn,
    button: ButtonFn,
    keyboard: KeyboardFn,
}

impl Hid {
    pub(crate) fn connect(device: &AnyObject, kit: Library) -> Result<Self> {
        let cls = rt::class(c"SimulatorKit.SimDeviceLegacyHIDClient")?;
        if cls.instance_method(sel!(initWithDevice:error:)).is_none()
            || cls.instance_method(sel!(sendWithMessage:freeWhenDone:completionQueue:completion:)).is_none()
        {
            return Err(Error::Unavailable("HID client methods".into()));
        }
        // SAFETY: `alloc` returns an uninitialized instance which the init call below consumes.
        let alloc: objc2::rc::Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
        // SAFETY: the initializer exists (checked) and takes (SimDevice, NSError**).
        let client: std::result::Result<Obj, Retained<NSError>> = unsafe { msg_send![alloc, initWithDevice: device, error: _] };
        let client = client.map_err(|e| rt::service_error("connect HID", &e))?;
        // SAFETY: the symbol is a C function whose signature was established from the framework's
        // exported builder: (point*, point*, target, type, edge, width, height) -> malloc'd message.
        // The button and keyboard builders below are (source, direction, target) and (usage, direction).
        let mouse = unsafe { std::mem::transmute::<*mut c_void, MouseFn>(kit.symbol(c"IndigoHIDMessageForMouseNSEvent")?.as_ptr()) };
        // SAFETY: see above.
        let button = unsafe { std::mem::transmute::<*mut c_void, ButtonFn>(kit.symbol(c"IndigoHIDMessageForButton")?.as_ptr()) };
        // SAFETY: see above.
        let keyboard =
            unsafe { std::mem::transmute::<*mut c_void, KeyboardFn>(kit.symbol(c"IndigoHIDMessageForKeyboardArbitrary")?.as_ptr()) };
        Ok(Self { client, mouse, button, keyboard })
    }

    fn send(&self, message: Message) -> Result<()> {
        if message.is_null() {
            return Err(Error::Service { op: "HID", message: "message builder returned nothing".into() });
        }
        // SAFETY: `objc_msgSend` is called through the method's real signature
        // (message*, BOOL, dispatch queue, block); nil queue and completion are accepted.
        let send: SendFn = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
        // SAFETY: the client responds to the selector (checked at connect); `message` is a valid
        // malloc'd message whose ownership passes to the client (`freeWhenDone`).
        unsafe {
            send(&*self.client, sel!(sendWithMessage:freeWhenDone:completionQueue:completion:), message, true, ptr::null(), ptr::null())
        };
        Ok(())
    }

    /// Sends one touch sample. `at` is normalized to `0..=1` on both axes, origin top-left.
    /// Moves sent less than ~16 ms apart are coalesced by the service and silently dropped.
    pub fn touch(&self, at: Point, phase: Phase) -> Result<()> {
        // NSEventType: left mouse down / up / dragged.
        let event_type = match phase {
            Phase::Down => 1,
            Phase::Up => 2,
            Phase::Move => 6,
        };
        let point = CGPoint { x: at.x.clamp(0.0, 1.0), y: at.y.clamp(0.0, 1.0) };
        // SAFETY: `point` outlives the call; a null second point means a single touch; size (1, 1)
        // makes the builder treat the point as already normalized.
        let message = unsafe { (self.mouse)(&point, ptr::null(), TARGET_TOUCH, event_type, 0, 1.0, 1.0) };
        if message.is_null() && phase == Phase::Move {
            return Ok(());
        }
        self.send(message)
    }

    /// Presses or releases a hardware button.
    pub fn button(&self, button: Button, direction: Direction) -> Result<()> {
        // SAFETY: plain integer arguments; returns a malloc'd message or null.
        self.send(unsafe { (self.button)(button.source(), direction.raw(), TARGET_HARDWARE) })
    }

    /// Presses or releases a keyboard key by HID usage.
    pub fn key(&self, usage: u32, direction: Direction) -> Result<()> {
        // SAFETY: plain integer arguments; returns a malloc'd message or null.
        self.send(unsafe { (self.keyboard)(usage, direction.raw()) })
    }
}
