//! Objective-C runtime helpers shared by the service, display and HID modules.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr::NonNull;

use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{class, msg_send};
use objc2_foundation::{NSError, NSString};

use crate::error::{Error, Result};

/// A retained Objective-C object of unknown class.
pub(crate) type Obj = Retained<AnyObject>;

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_NOW: c_int = 0x2;

/// `CGSize`, as returned by value from the service.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CGSize {
    pub width: f64,
    pub height: f64,
}

// SAFETY: `#[repr(C)]` struct of two doubles, matching CoreGraphics' `CGSize` on 64-bit hosts.
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

/// `CGPoint`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CGPoint {
    pub x: f64,
    pub y: f64,
}

// SAFETY: `#[repr(C)]` struct of two doubles, matching CoreGraphics' `CGPoint` on 64-bit hosts.
unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

// SAFETY: a pointer to `CGPoint` encodes as a pointer to its struct encoding.
unsafe impl RefEncode for CGPoint {
    const ENCODING_REF: Encoding = Encoding::Pointer(&<Self as Encode>::ENCODING);
}

/// A loaded dynamic library.
#[derive(Clone, Copy)]
pub(crate) struct Library(NonNull<c_void>);

impl Library {
    /// Loads the library at `path`.
    pub(crate) fn open(path: &str) -> Result<Self> {
        let c = CString::new(path).map_err(|_| Error::Invalid(format!("path contains NUL: {path}")))?;
        // SAFETY: `c` is a valid NUL-terminated string; dlopen has no other preconditions.
        let handle = unsafe { dlopen(c.as_ptr(), RTLD_NOW) };
        NonNull::new(handle).map(Self).ok_or_else(|| Error::Unavailable(format!("cannot load {path}")))
    }

    /// Looks up an exported symbol.
    pub(crate) fn symbol(self, name: &CStr) -> Result<NonNull<c_void>> {
        // SAFETY: the handle came from a successful dlopen and `name` is NUL-terminated.
        let ptr = unsafe { dlsym(self.0.as_ptr(), name.as_ptr()) };
        NonNull::new(ptr).ok_or_else(|| Error::Unavailable(format!("symbol {}", name.to_string_lossy())))
    }
}

/// Looks up a class by name.
pub(crate) fn class(name: &CStr) -> Result<&'static AnyClass> {
    AnyClass::get(name).ok_or_else(|| Error::Unavailable(format!("class {}", name.to_string_lossy())))
}

/// Whether `obj` responds to `sel`.
pub(crate) fn responds(obj: &AnyObject, sel: Sel) -> bool {
    // SAFETY: every NSObject implements `respondsToSelector:` returning BOOL.
    unsafe { msg_send![obj, respondsToSelector: sel] }
}

/// Fails with [`Error::Unavailable`] unless `obj` responds to `sel`.
pub(crate) fn require(obj: &AnyObject, sel: Sel) -> Result<()> {
    if responds(obj, sel) { Ok(()) } else { Err(Error::Unavailable(format!("method {}", sel.name().to_string_lossy()))) }
}

/// Creates an `NSString`.
pub(crate) fn nsstring(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// Sends a no-argument, object-returning message chosen at runtime. `None` when it returns nil.
fn send_getter(obj: &AnyObject, sel: Sel) -> Option<Obj> {
    type Getter = unsafe extern "C" fn(*const AnyObject, Sel) -> *mut AnyObject;
    // SAFETY: `objc_msgSend` must be called through the signature of the target method; callers
    // only pass selectors of no-argument methods returning an object pointer.
    let send: Getter = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
    // SAFETY: `obj` is a valid object and responds to `sel` (callers check).
    let raw = unsafe { send(obj, sel) };
    // SAFETY: getters return nil or a valid (autoreleased or borrowed) object, which we retain.
    unsafe { Retained::retain(raw) }
}

/// Sends a no-argument message returning `unsigned short`, bypassing objc2's signature check:
/// I/O port state objects are remote proxies that forward messages their class does not declare.
pub(crate) fn send_u16(obj: &AnyObject, sel: Sel) -> Option<u16> {
    // SAFETY: callers pass getters returning `unsigned short`.
    unsafe { send_scalar(obj, sel) }
}

/// Sends a no-argument message returning a scalar (integer or `CGSize`-like struct of doubles),
/// through `objc_msgSend` like [`send_u16`]. `None` when `obj` doesn't respond.
///
/// # Safety
/// `sel` must name a no-argument method whose return type is `T` (as returned in registers).
pub(crate) unsafe fn send_scalar<T: Copy>(obj: &AnyObject, sel: Sel) -> Option<T> {
    if !responds(obj, sel) {
        return None;
    }
    type Getter<T> = unsafe extern "C" fn(*const AnyObject, Sel) -> T;
    // SAFETY: `objc_msgSend` is called through the target's signature (the caller's contract).
    let send: Getter<T> = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
    // SAFETY: `obj` is a valid object that responds to `sel` (checked).
    Some(unsafe { send(obj, sel) })
}

/// An exported symbol of any image already loaded into the process (`dlsym(RTLD_DEFAULT, …)`).
pub(crate) fn default_symbol(name: &CStr) -> Result<NonNull<c_void>> {
    // RTLD_DEFAULT on Darwin.
    let default = -2isize as *mut c_void;
    // SAFETY: RTLD_DEFAULT is a valid pseudo-handle and `name` is NUL-terminated.
    let ptr = unsafe { dlsym(default, name.as_ptr()) };
    NonNull::new(ptr).ok_or_else(|| Error::Unavailable(format!("symbol {}", name.to_string_lossy())))
}

/// Reads a string-valued property; empty when missing, nil or not a string.
pub(crate) fn string_prop(obj: &AnyObject, sel: Sel) -> String {
    if !responds(obj, sel) {
        return String::new();
    }
    send_getter(obj, sel).and_then(|v| v.downcast::<NSString>().ok()).map(|s| s.to_string()).unwrap_or_default()
}

/// Reads an object-valued property that may be nil.
pub(crate) fn optional_prop(obj: &AnyObject, sel: Sel) -> Option<Obj> {
    if responds(obj, sel) { send_getter(obj, sel) } else { None }
}

/// Reads an object-valued property.
pub(crate) fn object_prop(obj: &AnyObject, sel: Sel) -> Result<Obj> {
    require(obj, sel)?;
    send_getter(obj, sel).ok_or_else(|| Error::Unavailable(format!("{} returned nil", sel.name().to_string_lossy())))
}

/// Copies the elements of an `NSArray` (or anything with `count` / `objectAtIndex:`).
pub(crate) fn array_items(array: &AnyObject) -> Vec<Obj> {
    // SAFETY: callers pass NSArray instances; `count` returns NSUInteger.
    let count: usize = unsafe { msg_send![array, count] };
    // SAFETY: indices are in bounds; `objectAtIndex:` returns a non-nil element.
    (0..count).map(|i| unsafe { msg_send![array, objectAtIndex: i] }).collect()
}

/// An empty `NSDictionary`.
pub(crate) fn empty_dictionary() -> Obj {
    // SAFETY: `+[NSDictionary dictionary]` returns a new empty dictionary.
    unsafe { msg_send![class!(NSDictionary), dictionary] }
}

/// An `NSNumber` holding a signed integer.
pub(crate) fn nsnumber(v: i64) -> Obj {
    // SAFETY: `+[NSNumber numberWithLongLong:]` takes a long long and returns a non-nil number.
    unsafe { msg_send![class!(NSNumber), numberWithLongLong: v] }
}

/// An `NSNumber` holding a BOOL.
pub(crate) fn nsbool(v: bool) -> Obj {
    // SAFETY: `+[NSNumber numberWithBool:]` takes a BOOL and returns a non-nil number.
    unsafe { msg_send![class!(NSNumber), numberWithBool: v] }
}

/// An `NSArray` of strings.
pub(crate) fn string_array(items: &[String]) -> Obj {
    // SAFETY: `+[NSMutableArray array]` returns a new empty mutable array.
    let array: Obj = unsafe { msg_send![class!(NSMutableArray), array] };
    for item in items {
        // SAFETY: `addObject:` takes a non-nil object.
        let _: () = unsafe { msg_send![&*array, addObject: &*nsstring(item)] };
    }
    array
}

/// A mutable dictionary to fill with [`dict_set`].
pub(crate) fn mutable_dictionary() -> Obj {
    // SAFETY: `+[NSMutableDictionary dictionary]` returns a new empty mutable dictionary.
    unsafe { msg_send![class!(NSMutableDictionary), dictionary] }
}

/// Sets `dict[key] = value` on a mutable dictionary.
pub(crate) fn dict_set(dict: &AnyObject, key: &str, value: &AnyObject) {
    // SAFETY: `setObject:forKey:` takes a non-nil object and a copyable key (NSString).
    let _: () = unsafe { msg_send![dict, setObject: value, forKey: &*nsstring(key)] };
}

/// The string under `key` in an `NSDictionary`; `None` when missing or not a string.
pub(crate) fn dict_string(dict: &AnyObject, key: &str) -> Option<String> {
    // SAFETY: `objectForKey:` takes a key object and returns nil or the value.
    let value: Option<Obj> = unsafe { msg_send![dict, objectForKey: &*nsstring(key)] };
    value.and_then(|v| v.downcast::<NSString>().ok()).map(|s| s.to_string())
}

/// A dictionary of string pairs.
pub(crate) fn string_dictionary(pairs: &[(String, String)]) -> Obj {
    let dict = mutable_dictionary();
    for (k, v) in pairs {
        dict_set(&dict, k, &nsstring(v));
    }
    dict
}

/// Parses JSON into Foundation objects.
pub(crate) fn json_object(json: &str) -> Result<Obj> {
    // SAFETY: `dataWithBytes:length:` copies `len` bytes from a valid pointer.
    let data: Obj = unsafe { msg_send![class!(NSData), dataWithBytes: json.as_ptr().cast::<std::ffi::c_void>(), length: json.len()] };
    let mut error: *mut NSError = std::ptr::null_mut();
    // SAFETY: takes NSData, options (0) and an NSError** out-parameter; returns nil on failure.
    let obj: Option<Obj> =
        unsafe { msg_send![class!(NSJSONSerialization), JSONObjectWithData: &*data, options: 0usize, error: &mut error] };
    obj.ok_or_else(|| Error::Invalid(format!("bad JSON: {}", raw_error("parse", error))))
}

/// A file `NSURL` for `path`.
pub(crate) fn file_url(path: &str) -> Obj {
    // SAFETY: `+[NSURL fileURLWithPath:]` takes an NSString and returns a non-nil URL.
    unsafe { msg_send![class!(NSURL), fileURLWithPath: &*nsstring(path)] }
}

/// Converts a service `NSError` into [`Error::Service`].
pub(crate) fn service_error(op: &'static str, error: &NSError) -> Error {
    Error::Service { op, message: error.localizedDescription().to_string() }
}

/// Converts a raw out-parameter error (possibly null) into [`Error::Service`].
pub(crate) fn raw_error(op: &'static str, error: *mut NSError) -> Error {
    // SAFETY: the service wrote either nil or a valid autoreleased NSError into the out-parameter.
    match unsafe { error.as_ref() } {
        Some(e) => service_error(op, e),
        None => Error::Service { op, message: "the service reported failure without an error".into() },
    }
}
