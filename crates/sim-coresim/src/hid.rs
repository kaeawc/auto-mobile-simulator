//! HID input: touches, hardware buttons, keyboard keys, and each form factor's own inputs (the
//! Siri Remote and its touch surface, the Digital Crown, watch gestures, the CarPlay screen).

use std::ffi::{CStr, c_void};
use std::ptr;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{msg_send, sel};
use objc2_foundation::NSError;
use sim_core::form_factor::FormFactor;
use sim_core::geometry::Point;
use sim_core::gesture::Phase;
use sim_core::hid::{Button, Direction, WatchGesture, crown_steps};

use crate::error::{Error, Result};
use crate::rt::{self, CGPoint, Library, Obj};

/// The service's raw direction value for keys.
fn raw(direction: Direction) -> u32 {
    match direction {
        Direction::Down => 1,
        Direction::Up => 2,
    }
}

/// Opaque HID message allocated by the message builders and freed by the client after sending.
type Message = *mut c_void;

type MouseFn = unsafe extern "C" fn(*const CGPoint, *const CGPoint, u32, u64, u64, f64, f64) -> Message;
type ButtonFn = unsafe extern "C" fn(u32, u32, u32) -> Message;
type KeyboardFn = unsafe extern "C" fn(u32, u32) -> Message;
type CrownFn = unsafe extern "C" fn(f64) -> Message;
type GestureFn = unsafe extern "C" fn(u32) -> Message;
type TrackpadFn = unsafe extern "C" fn(CGPoint, u32) -> Message;
type CreateCarPlayFn = unsafe extern "C" fn(bool) -> Message;
type RemoveCarPlayFn = unsafe extern "C" fn() -> Message;
type SendFn = unsafe extern "C" fn(*const AnyObject, Sel, Message, bool, *const AnyObject, *const c_void);

/// Touch digitizer target.
const TARGET_TOUCH: u32 = 0x32;
/// The CarPlay screen's digitizer (Simulator.app's target for TVOut screens). Never compute a
/// per-screen target for it: those abort backboardd.
const TARGET_CARPLAY: u32 = 1;
/// The Siri Remote's touch surface.
const TARGET_REMOTE_TOUCH: u32 = 0x16;

/// The touch-surface message's layout (Xcode 26.6 and 27.x), checked before any field is patched:
/// its builder makes a "moved" contact, and a swipe needs began and ended ones too.
mod trackpad {
    pub const SIZE: usize = 0x160;
    pub const EVENT_SIZE_OFFSET: usize = 0x18;
    pub const EVENT_SIZE: u32 = 0xa0;
    pub const EVENT_COUNT_OFFSET: usize = 0x1c;
    pub const EVENT_COUNT: u8 = 2;
    /// The first and second event's masks; the builder sets both to "position".
    pub const MASKS: [usize; 2] = [0x38, 0xd8];
    pub const POSITION_MASK: u32 = 4;
    /// Both events' touch flags; the builder sets each to 1.
    pub const TOUCH_FLAGS: [usize; 4] = [0x64, 0x68, 0x104, 0x108];
    pub const BEGAN_MASKS: [u32; 2] = [0x23, 3];
    pub const ENDED_MASKS: [u32; 2] = [0x21, 1];
}

unsafe extern "C" {
    fn malloc_size(ptr: *const c_void) -> usize;
    #[link_name = "free"]
    fn libc_free(ptr: *mut c_void);
}

/// A HID session with one booted device.
pub struct Hid {
    client: Obj,
    kit: Library,
    form_factor: Option<FormFactor>,
    mouse: MouseFn,
    button: ButtonFn,
    keyboard: KeyboardFn,
}

impl Hid {
    pub(crate) fn connect(device: &AnyObject, kit: Library, form_factor: Option<FormFactor>) -> Result<Self> {
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
        Ok(Self { client, kit, form_factor, mouse, button, keyboard })
    }

    /// An optional message builder, looked up when first needed: a runtime without it fails that
    /// one input with [`Error::Unavailable`].
    fn builder(&self, name: &CStr) -> Result<*mut c_void> {
        self.kit.symbol(name).map(|p| p.as_ptr())
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

    fn touch_target(&self, target: u32, at: Point, phase: Phase) -> Result<()> {
        // NSEventType: left mouse down / up / dragged.
        let event_type = match phase {
            Phase::Down => 1,
            Phase::Up => 2,
            Phase::Move => 6,
        };
        let point = CGPoint { x: at.x.clamp(0.0, 1.0), y: at.y.clamp(0.0, 1.0) };
        // SAFETY: `point` outlives the call; a null second point means a single touch; size (1, 1)
        // makes the builder treat the point as already normalized.
        let message = unsafe { (self.mouse)(&point, ptr::null(), target, event_type, 0, 1.0, 1.0) };
        if message.is_null() && phase == Phase::Move {
            return Ok(());
        }
        self.send(message)
    }

    /// Sends one touch sample. `at` is normalized to `0..=1` on both axes, origin top-left.
    /// Moves sent less than ~16 ms apart are coalesced by the service and silently dropped.
    pub fn touch(&self, at: Point, phase: Phase) -> Result<()> {
        self.touch_target(TARGET_TOUCH, at, phase)
    }

    /// One touch sample on the CarPlay screen (normalized like [`Hid::touch`]). The CarPlay HID
    /// service must exist first ([`Hid::carplay_service`]).
    pub fn carplay_touch(&self, at: Point, phase: Phase) -> Result<()> {
        self.touch_target(TARGET_CARPLAY, at, phase)
    }

    /// Creates (or removes) the guest's CarPlay HID service, which the CarPlay screen's touches go
    /// to. Once per boot, after the CarPlay display is attached.
    pub fn carplay_service(&self, on: bool) -> Result<()> {
        let message = if on {
            // SAFETY: the builder takes BOOL (has a touch screen) and returns a malloc'd message.
            let create =
                unsafe { std::mem::transmute::<*mut c_void, CreateCarPlayFn>(self.builder(c"IndigoHIDMessageToCreateCarPlayService")?) };
            // SAFETY: plain argument; returns a message or null.
            unsafe { create(true) }
        } else {
            // SAFETY: the builder takes nothing and returns a malloc'd message.
            let remove =
                unsafe { std::mem::transmute::<*mut c_void, RemoveCarPlayFn>(self.builder(c"IndigoHIDMessageToRemoveCarPlayService")?) };
            // SAFETY: no arguments; returns a message or null.
            unsafe { remove() }
        };
        self.send(message)
    }

    /// Presses or releases a hardware button, the way this device's form factor has it: the Siri
    /// Remote's buttons on Apple TV (whose arrows are keyboard arrows), the crown's press on a watch.
    pub fn button(&self, button: Button, direction: Direction) -> Result<()> {
        if let Some(press) = button.apple_press(self.form_factor) {
            // SAFETY: plain integer arguments; returns a malloc'd message or null.
            return self.send(unsafe { (self.button)(press.source, press.value(direction), press.target) });
        }
        if self.form_factor == Some(FormFactor::Tv)
            && let Some(usage) = button.apple_remote_key()
        {
            return self.key(usage, direction);
        }
        let device = self.form_factor.map_or("this device", FormFactor::as_str);
        Err(Error::Invalid(format!("the {} button doesn't exist on an Apple {device}", button.as_str())))
    }

    /// Presses or releases a keyboard key by HID usage.
    pub fn key(&self, usage: u32, direction: Direction) -> Result<()> {
        // SAFETY: plain integer arguments; returns a malloc'd message or null.
        self.send(unsafe { (self.keyboard)(usage, raw(direction)) })
    }

    /// Turns the Digital Crown by `delta` crown units (Simulator.app's scroll-wheel delta; positive
    /// scrolls content up), split into bounded steps. Apple Watch only.
    pub fn crown(&self, delta: f64) -> Result<()> {
        self.require(FormFactor::Watch, "the Digital Crown")?;
        // Not IndigoHIDMessageForScrollEvent: sent to the digitizer, it aborts backboardd.
        // SAFETY: the builder takes the rotational delta (double) and returns a malloc'd message.
        let crown = unsafe { std::mem::transmute::<*mut c_void, CrownFn>(self.builder(c"IndigoHIDMessageForDigitalCrownEvent")?) };
        for step in crown_steps(delta) {
            // SAFETY: plain argument; returns a message or null.
            self.send(unsafe { crown(step) })?;
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        Ok(())
    }

    /// Performs a watch gesture (one message, no down/up). Apple Watch only.
    pub fn watch_gesture(&self, gesture: WatchGesture) -> Result<()> {
        self.require(FormFactor::Watch, "watch gestures")?;
        // SAFETY: the builder takes the gesture code and returns a malloc'd message.
        let build = unsafe { std::mem::transmute::<*mut c_void, GestureFn>(self.builder(c"IndigoHIDMessageForWatchGesture")?) };
        // SAFETY: plain argument; returns a message or null.
        self.send(unsafe { build(gesture.apple_code()) })
    }

    /// One contact sample on the Siri Remote's touch surface (`at` normalized to `0..=1`). The
    /// focus engine moves only for a whole began → moved… → ended sequence. Apple TV only.
    pub fn remote_touch(&self, at: Point, phase: Phase) -> Result<()> {
        self.require(FormFactor::Tv, "the Siri Remote's touch surface")?;
        // SAFETY: the builder takes (CGPoint by value, target) and returns a malloc'd message.
        let build = unsafe { std::mem::transmute::<*mut c_void, TrackpadFn>(self.builder(c"IndigoHIDMessageForTrackpadMoveEvent")?) };
        let point = CGPoint { x: at.x.clamp(0.0, 1.0), y: at.y.clamp(0.0, 1.0) };
        // SAFETY: plain arguments; returns a message or null.
        let message = unsafe { build(point, TARGET_REMOTE_TOUCH) };
        if message.is_null() {
            return Err(Error::Service { op: "HID", message: "touch-surface builder returned nothing".into() });
        }
        if let Err(e) = patch_trackpad(message, phase) {
            // SAFETY: the message is malloc'd and wasn't handed to the client.
            unsafe { libc_free(message) };
            return Err(e);
        }
        self.send(message)
    }

    fn require(&self, form_factor: FormFactor, what: &str) -> Result<()> {
        match self.form_factor {
            Some(f) if f == form_factor => Ok(()),
            Some(f) => {
                Err(Error::Invalid(format!("{what} needs an Apple {}; this device is an Apple {}", form_factor.as_str(), f.as_str())))
            }
            None => Err(Error::Invalid(format!("{what} needs an Apple {}; this device's form factor is unknown", form_factor.as_str()))),
        }
    }
}

/// Turns the builder's "moved" contact into the phase wanted, after checking the message has the
/// layout the fields are patched for (else [`Error::Unavailable`], never a blind write).
fn patch_trackpad(message: Message, phase: Phase) -> Result<()> {
    use trackpad::*;
    let base = message.cast::<u8>();
    // SAFETY: `message` is a live malloc'd block; `malloc_size` reports its usable size.
    let size = unsafe { malloc_size(message) };
    let read32 = |offset: usize| {
        // SAFETY: callers only read offsets below SIZE, which `size >= SIZE` (checked first) covers.
        unsafe { base.add(offset).cast::<u32>().read_unaligned() }
    };
    let known = size >= SIZE
        && read32(EVENT_SIZE_OFFSET) == EVENT_SIZE
        // SAFETY: in bounds, as above.
        && unsafe { base.add(EVENT_COUNT_OFFSET).read() } == EVENT_COUNT
        && MASKS.iter().all(|&o| read32(o) == POSITION_MASK)
        && TOUCH_FLAGS.iter().all(|&o| read32(o) == 1);
    if !known {
        return Err(Error::Unavailable("IndigoHIDMessageForTrackpadMoveEvent builds a message this version doesn't know".into()));
    }
    let write32 = |offset: usize, value: u32| {
        // SAFETY: offsets are below SIZE, inside the block (checked above); the block is ours until sent.
        unsafe { base.add(offset).cast::<u32>().write_unaligned(value) }
    };
    match phase {
        Phase::Move => {}
        Phase::Down => MASKS.iter().zip(BEGAN_MASKS).for_each(|(&o, v)| write32(o, v)),
        Phase::Up => {
            MASKS.iter().zip(ENDED_MASKS).for_each(|(&o, v)| write32(o, v));
            TOUCH_FLAGS.iter().for_each(|&o| write32(o, 0));
        }
    }
    Ok(())
}
