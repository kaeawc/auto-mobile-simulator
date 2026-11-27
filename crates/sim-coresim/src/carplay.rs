//! CarPlay: an iPhone simulator's external (TVOut) screen as the car display, attached the way
//! Simulator.app's I/O > External Displays > CarPlay does it, without its UI.
//!
//! 1. Write the StarkConfig (`extended_display.plist`) into the device's data directory.
//! 2. Find the TVOut screen through the framebuffer server's screen adapter (the I/O port whose
//!    descriptor enumerates screens) and, if it's already on as a plain display, turn it off first:
//!    otherwise the guest keeps it as an AirPlay display.
//! 3. Set its mode at 800×480 with CarPlay properties, which powers it on; the guest re-reads the
//!    StarkConfig and makes it the car display.
//!
//! The screen objects are remote proxies, so every message goes through `objc_msgSend` with its
//! real signature, after a `respondsToSelector:` check.

use std::ffi::CStr;
use std::sync::mpsc;
use std::time::Duration;

use block2::{Block, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::sel;
use objc2_foundation::NSError;
use sim_core::carplay;

use crate::error::{Error, Result};
use crate::rt::{self, CGSize, Obj};

/// The screen type CarPlay uses: TVOut (not the wireless screen, 3).
const SCREEN_TYPE_TVOUT: u64 = 1;
/// How long a completion handler gets to answer.
const WAIT: Duration = Duration::from_secs(20);
/// How long the screen stays off before CarPlay reconnects (less and iOS 26.5 may keep the old
/// display and never render CarPlay).
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

type ErrorBlock = Block<dyn Fn(*mut NSError)>;

/// A completion handler that reports its error (or `None`) on `tx`.
fn error_block(tx: mpsc::Sender<Option<String>>) -> RcBlock<dyn Fn(*mut NSError)> {
    RcBlock::with_encoding::<_, _, _, rt::ErrorBlock>(move |error: *mut NSError| {
        // SAFETY: the service passes nil or a valid NSError for the duration of the call.
        let message = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
        let _ = tx.send(message);
    })
}

fn wait(rx: &mpsc::Receiver<Option<String>>, op: &'static str) -> Result<()> {
    match rx.recv_timeout(WAIT) {
        Ok(None) => Ok(()),
        Ok(Some(message)) => Err(Error::Service { op, message }),
        Err(_) => Err(Error::Service { op, message: format!("no answer within {}s", WAIT.as_secs()) }),
    }
}

/// The TVOut screen: its `SimScreen` proxy, and whether it is on and already a CarPlay screen.
struct Screen {
    obj: Obj,
    powered: bool,
    carplay: bool,
}

fn screens_of(adapter: &AnyObject) -> Result<Vec<Obj>> {
    let enumerate = sel!(enumerateScreensWithCompletionQueue:completionHandler:);
    let (tx, rx) = mpsc::channel::<std::result::Result<Vec<usize>, String>>();
    let block = RcBlock::with_encoding::<_, _, _, rt::ObjErrorBlock>(move |screens: *mut AnyObject, error: *mut NSError| {
        // SAFETY: the service passes nil or valid objects for the duration of the call.
        let result = match unsafe { error.as_ref() } {
            Some(e) => Err(e.localizedDescription().to_string()),
            // Each screen is retained here and handed over as an address: `Retained` isn't `Send`.
            // SAFETY: as above; `screens` is nil or an NSArray.
            None => Ok(unsafe { screens.as_ref() }
                .map(|a| rt::array_items(a).into_iter().map(|s| Retained::into_raw(s) as usize).collect())
                .unwrap_or_default()),
        };
        let _ = tx.send(result);
    });
    type EnumerateFn = unsafe extern "C" fn(*const AnyObject, Sel, *const AnyObject, *const Block<dyn Fn(*mut AnyObject, *mut NSError)>);
    // SAFETY: the method's signature is (dispatch_queue_t, void (^)(NSArray *, NSError *)).
    let send: EnumerateFn = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
    // SAFETY: `adapter` responds to the selector (callers check); the block outlives the call and
    // the service copies it.
    unsafe { send(adapter, enumerate, rt::global_queue(), &*block) };
    let raw = match rx.recv_timeout(WAIT) {
        Ok(Ok(raw)) => raw,
        Ok(Err(message)) => return Err(Error::Service { op: "enumerate screens", message }),
        Err(_) => return Err(Error::Service { op: "enumerate screens", message: "no answer".into() }),
    };
    // SAFETY: each address is a +1 reference created by `Retained::into_raw` above.
    Ok(raw.into_iter().filter_map(|p| unsafe { Retained::from_raw(p as *mut AnyObject) }).collect())
}

fn tvout(device: &AnyObject) -> Result<Screen> {
    let io = rt::object_prop(device, sel!(io))?;
    if rt::responds(&io, sel!(updateIOPorts)) {
        // SAFETY: `updateIOPorts` takes no arguments and returns void (checked to exist).
        unsafe { objc2::msg_send![&*io, updateIOPorts] }
    }
    let ports = rt::object_prop(&io, sel!(ioPorts))?;
    let enumerate = sel!(enumerateScreensWithCompletionQueue:completionHandler:);
    let adapters: Vec<Obj> = rt::array_items(&ports)
        .iter()
        .filter_map(|p| rt::object_prop(p, sel!(descriptor)).ok())
        .filter(|d| rt::responds(d, enumerate))
        .collect();
    if adapters.is_empty() {
        return Err(Error::Unavailable("screen adapter (enumerateScreensWithCompletionQueue:completionHandler:)".into()));
    }
    for adapter in adapters {
        for screen in screens_of(&adapter)? {
            let Ok(props) = rt::object_prop(&screen, sel!(screenProperties)) else { continue };
            // SAFETY: `screenType` returns an unsigned 64-bit integer.
            let ty: Option<u64> = unsafe { rt::send_scalar(&props, sel!(screenType)) };
            if ty != Some(SCREEN_TYPE_TVOUT) {
                continue;
            }
            // SAFETY: `powerState` returns a 32-bit integer (1 = on).
            let power: Option<i32> = unsafe { rt::send_scalar(&props, sel!(powerState)) };
            let carplay = rt::optional_prop(&props, sel!(carPlayProperties)).is_some();
            return Ok(Screen { obj: screen, powered: power == Some(1), carplay });
        }
    }
    Err(Error::NoDisplay("the device has no external (TVOut) screen; CarPlay needs an iPhone".into()))
}

fn set_power(screen: &AnyObject, on: bool) -> Result<()> {
    let sel = sel!(setPowerState:completionQueue:completionHandler:);
    rt::require(screen, sel)?;
    let (tx, rx) = mpsc::channel();
    let block = error_block(tx);
    type PowerFn = unsafe extern "C" fn(*const AnyObject, Sel, i32, *const AnyObject, *const ErrorBlock);
    // SAFETY: the method's signature is (int, dispatch_queue_t, void (^)(NSError *)).
    let send: PowerFn = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
    // SAFETY: `screen` responds (checked); the block outlives the call and the service copies it.
    unsafe { send(screen, sel, i32::from(on), rt::global_queue(), &*block) };
    wait(&rx, "set the external screen's power")
}

/// The NSString behind one of CoreSimDeviceIO's exported CarPlay property keys.
fn property_key(name: &CStr) -> Result<Obj> {
    rt::string_symbol(name)
}

fn set_carplay_mode(screen: &AnyObject) -> Result<()> {
    let sel = sel!(setCurrentMode:pixelSize:carPlayProperties:completionQueue:completionHandler:);
    rt::require(screen, sel)?;
    let props = rt::object_prop(screen, sel!(screenProperties))?;
    let mode = match rt::optional_prop(&props, sel!(currentMode)) {
        Some(mode) => mode,
        None => rt::optional_prop(&props, sel!(supportedScreenModes))
            .and_then(|modes| rt::array_items(&modes).into_iter().next())
            .ok_or_else(|| Error::NoDisplay("the external screen advertises no mode".into()))?,
    };
    let properties = rt::mutable_dictionary();
    for key in [c"SimCarPlayPropertyKeyStark", c"SimCarPlayPropertyKeyHasTouchScreen"] {
        let key = property_key(key)?;
        // SAFETY: `setObject:forKey:` takes a non-nil object and a copyable key (NSString).
        let _: () = unsafe { objc2::msg_send![&*properties, setObject: &*rt::nsbool(true), forKey: &*key] };
    }
    let (tx, rx) = mpsc::channel();
    let block = error_block(tx);
    type ModeFn =
        unsafe extern "C" fn(*const AnyObject, Sel, *const AnyObject, CGSize, *const AnyObject, *const AnyObject, *const ErrorBlock);
    // SAFETY: the method's signature is (SimScreenMode, CGSize, NSDictionary, dispatch_queue_t,
    // void (^)(NSError *)).
    let send: ModeFn = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
    let size = CGSize { width: f64::from(carplay::WIDTH), height: f64::from(carplay::HEIGHT) };
    // SAFETY: `screen` responds (checked); every object argument is live for the call and the
    // block is copied by the service.
    unsafe { send(screen, sel, &*mode, size, &*properties, rt::global_queue(), &*block) };
    wait(&rx, "set the CarPlay mode")
}

fn write_config(data_path: &str, on: bool) -> Result<()> {
    let path = std::path::Path::new(data_path).join(carplay::CONFIG_PATH);
    let io = |e: std::io::Error| Error::Service { op: "write the CarPlay configuration", message: format!("{}: {e}", path.display()) };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    std::fs::write(&path, carplay::config_plist(on)).map_err(io)
}

/// Attaches (or detaches) the car display. The HID service for its touches is the caller's
/// ([`crate::Hid::carplay_service`]), created after this and removed before detaching.
pub(crate) fn set_display(device: &AnyObject, data_path: &str, on: bool) -> Result<()> {
    if !on {
        let screen = tvout(device)?;
        set_power(&screen.obj, false)?;
        return write_config(data_path, false);
    }
    write_config(data_path, true)?;
    let screen = tvout(device)?;
    if screen.powered && !screen.carplay {
        set_power(&screen.obj, false)?;
        std::thread::sleep(RECONNECT_DELAY);
    }
    set_carplay_mode(&screen.obj)
}
