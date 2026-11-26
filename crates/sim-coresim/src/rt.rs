//! Objective-C runtime helpers shared by the service, display and HID modules.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr::NonNull;

use block2::ManualBlockEncoding;
use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{class, msg_send, sel};
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

/// Block signatures. A block handed to a remote proxy (the screen adapter and framebuffer
/// descriptors are ROCKit proxies under CoreSimulator 1174) is forwarded by its signature, and
/// ROCKit throws an Objective-C exception, which aborts the process, for a block without one
/// (`carplay` on iOS 18.0, `Display::watch_frames` on iOS 26.5); local objects ignore it. Every
/// block given to the service carries one.
///
/// `void (^)(id)`.
pub(crate) struct ObjBlock;

// SAFETY: a block returning void and taking one object: `v`, a 16-byte frame, the block at 0 and
// the object at 8 (what clang emits for `^(id o) {}`).
unsafe impl ManualBlockEncoding for ObjBlock {
    type Arguments = (*mut AnyObject,);
    type Return = ();
    const ENCODING_CSTR: &'static CStr = c"v16@?0@8";
}

/// `void (^)(NSError *)`.
pub(crate) struct ErrorBlock;

// SAFETY: as `ObjBlock`; an `NSError *` encodes as an object.
unsafe impl ManualBlockEncoding for ErrorBlock {
    type Arguments = (*mut NSError,);
    type Return = ();
    const ENCODING_CSTR: &'static CStr = c"v16@?0@8";
}

/// `void (^)(id, NSError *)`.
pub(crate) struct ObjErrorBlock;

// SAFETY: a block returning void and taking two objects: a 24-byte frame, the block at 0, the
// objects at 8 and 16 (what clang emits for `^(id o, NSError *e) {}`).
unsafe impl ManualBlockEncoding for ObjErrorBlock {
    type Arguments = (*mut AnyObject, *mut NSError);
    type Return = ();
    const ENCODING_CSTR: &'static CStr = c"v24@?0@8@16";
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
    let mut message = error.localizedDescription().to_string();
    // The service's own description is generic ("failed to complete the requested operation"); the
    // runtime's reason, when there is one, is the underlying error ("Runtime does not support
    // userInterfaceStyle" on tvOS).
    if let Some(underlying) = underlying_error(error) {
        let reason = string_prop(&underlying, sel!(localizedDescription));
        if !reason.is_empty() && reason != message {
            message = format!("{message} ({reason})");
        }
    }
    Error::Service { op, message }
}

/// An error's `NSUnderlyingErrorKey` entry, when it is an error.
fn underlying_error(error: &NSError) -> Option<Obj> {
    let info = optional_prop(error, sel!(userInfo))?;
    if !responds(&info, sel!(objectForKey:)) {
        return None;
    }
    // SAFETY: `objectForKey:` takes a key object and returns an object or nil (checked to exist).
    let value: Option<Obj> = unsafe { msg_send![&*info, objectForKey: &*nsstring("NSUnderlyingError")] };
    value.filter(|v| responds(v, sel!(localizedDescription)))
}

/// Converts a raw out-parameter error (possibly null) into [`Error::Service`].
pub(crate) fn raw_error(op: &'static str, error: *mut NSError) -> Error {
    // SAFETY: the service wrote either nil or a valid autoreleased NSError into the out-parameter.
    match unsafe { error.as_ref() } {
        Some(e) => service_error(op, e),
        None => Error::Service { op, message: "the service reported failure without an error".into() },
    }
}

#[cfg(test)]
mod tests {
    use block2::{ManualBlockEncoding, RcBlock};
    use objc2::encode::EncodeArguments;

    use super::*;

    /// The signature clang writes for a block returning void: `v`, the frame size, the block
    /// itself at 0, then each (pointer-sized) argument at its offset.
    fn void_block_signature<E: ManualBlockEncoding<Return = ()>>() -> String {
        let args = <E::Arguments as EncodeArguments>::ENCODINGS;
        let each: String = args.iter().enumerate().map(|(i, arg)| format!("{arg}{}", 8 * (i + 1))).collect();
        format!("v{}@?0{each}", 8 * (args.len() + 1))
    }

    #[test]
    fn block_signatures_match_their_argument_types() {
        assert_eq!(ObjBlock::ENCODING_CSTR.to_str(), Ok(void_block_signature::<ObjBlock>().as_str()));
        assert_eq!(ErrorBlock::ENCODING_CSTR.to_str(), Ok(void_block_signature::<ErrorBlock>().as_str()));
        assert_eq!(ObjErrorBlock::ENCODING_CSTR.to_str(), Ok(void_block_signature::<ObjErrorBlock>().as_str()));
        assert_eq!(void_block_signature::<ObjErrorBlock>(), "v24@?0@8@16");
    }

    #[test]
    fn encoded_blocks_still_call_their_closures() {
        let block = RcBlock::with_encoding::<_, _, _, ObjErrorBlock>(|o: *mut AnyObject, e: *mut NSError| {
            assert!(o.is_null() && e.is_null());
        });
        block.call((std::ptr::null_mut(), std::ptr::null_mut()));
    }
}
