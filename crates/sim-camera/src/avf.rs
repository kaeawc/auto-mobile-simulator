//! macOS cameras through AVFoundation, loaded on first use: `AVCaptureDeviceDiscoverySession`
//! lists built-in, external, Continuity and Desk View cameras and every CoreMediaIO camera
//! extension (OBS Virtual Camera, Camo, mmhmm); `authorizationStatusForMediaType:` reads the
//! camera permission without prompting; `Webcam` captures BGRA frames through an
//! `AVCaptureSession` and a video data output.
//!
//! Classes and constants are looked up at runtime, so an older macOS without one (Continuity
//! Camera's device type is macOS 14+) skips it instead of failing to load.
#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_void};
use std::ptr::{self, NonNull};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, Bool, NSObject, NSObjectProtocol, Sel};
use objc2::{AnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_foundation::{NSError, NSString};

use crate::catalog::{Format, HostCamera, classify_apple};
use crate::error::{Error, Result};
use crate::frame::{CameraFrame, FrameSpec, Tick};
use crate::source::Source;
use crate::tcc::Authorization;

type Obj = Retained<AnyObject>;

const AVFOUNDATION: &CStr = c"/System/Library/Frameworks/AVFoundation.framework/AVFoundation";
const COREMEDIA: &CStr = c"/System/Library/Frameworks/CoreMedia.framework/CoreMedia";
const COREVIDEO: &CStr = c"/System/Library/Frameworks/CoreVideo.framework/CoreVideo";

/// The device types to discover, newest first; each is skipped when this macOS lacks it.
/// `ExternalUnknown` is the pre-macOS 14 name of `External`.
const DEVICE_TYPES: &[&CStr] = &[
    c"AVCaptureDeviceTypeBuiltInWideAngleCamera",
    c"AVCaptureDeviceTypeExternal",
    c"AVCaptureDeviceTypeContinuityCamera",
    c"AVCaptureDeviceTypeDeskViewCamera",
];
const EXTERNAL_UNKNOWN: &CStr = c"AVCaptureDeviceTypeExternalUnknown";

const FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
const LOCK_READ_ONLY: u64 = 1;
/// How long a capture waits for its first frame, or tolerates no new one, before it reports why.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_sync_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_release(object: *mut c_void);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Dimensions {
    width: i32,
    height: i32,
}

/// The C functions and constants capture needs, resolved once. Constants are `NSString *`
/// (toll-free bridged `CFStringRef`) values, which live as long as the process; held as addresses
/// so the table is `Sync`.
struct Api {
    video: usize,
    device_types: Vec<usize>,
    pixel_format_key: usize,
    format_dimensions: unsafe extern "C" fn(*const c_void) -> Dimensions,
    format_subtype: unsafe extern "C" fn(*const c_void) -> u32,
    sample_image: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    lock: unsafe extern "C" fn(*mut c_void, u64) -> i32,
    unlock: unsafe extern "C" fn(*mut c_void, u64) -> i32,
    base: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    bytes_per_row: unsafe extern "C" fn(*mut c_void) -> usize,
    width: unsafe extern "C" fn(*mut c_void) -> usize,
    height: unsafe extern "C" fn(*mut c_void) -> usize,
    pixel_format: unsafe extern "C" fn(*mut c_void) -> u32,
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(Clone::clone)
}

fn open(path: &CStr) -> Result<NonNull<c_void>> {
    // SAFETY: `path` is NUL-terminated; dlopen has no other preconditions.
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW) };
    NonNull::new(handle).ok_or_else(|| Error::Unavailable(format!("can't load {}", path.to_string_lossy())))
}

fn symbol(lib: NonNull<c_void>, name: &CStr) -> Option<NonNull<c_void>> {
    // SAFETY: the handle came from a successful dlopen and `name` is NUL-terminated.
    NonNull::new(unsafe { libc::dlsym(lib.as_ptr(), name.as_ptr()) })
}

/// The value of an exported `NSString *const` (or `CFStringRef`) constant.
fn constant(lib: NonNull<c_void>, name: &CStr) -> Option<usize> {
    let sym = symbol(lib, name)?;
    // SAFETY: callers name pointer-sized string constants; read the pointer the symbol holds.
    let value = unsafe { sym.as_ptr().cast::<*const c_void>().read() };
    (!value.is_null()).then_some(value as usize)
}

/// A C function, as the function pointer type `F`.
fn function<F: Copy>(lib: NonNull<c_void>, name: &CStr) -> Result<F> {
    let sym = symbol(lib, name).ok_or_else(|| Error::Unavailable(format!("symbol {}", name.to_string_lossy())))?;
    assert_eq!(size_of::<F>(), size_of::<*mut c_void>());
    // SAFETY: `F` is the `extern "C"` signature of the named function (each call site spells it
    // out), and function pointers are pointer-sized on Apple targets.
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&sym.as_ptr()) })
}

fn load() -> Result<Api> {
    let av = open(AVFOUNDATION)?;
    let cm = open(COREMEDIA)?;
    let cv = open(COREVIDEO)?;
    let mut device_types: Vec<usize> = DEVICE_TYPES.iter().filter_map(|name| constant(av, name)).collect();
    if constant(av, c"AVCaptureDeviceTypeExternal").is_none() {
        device_types.extend(constant(av, EXTERNAL_UNKNOWN));
    }
    Ok(Api {
        video: constant(av, c"AVMediaTypeVideo").ok_or_else(|| Error::Unavailable("AVMediaTypeVideo".into()))?,
        device_types,
        pixel_format_key: constant(cv, c"kCVPixelBufferPixelFormatTypeKey")
            .ok_or_else(|| Error::Unavailable("kCVPixelBufferPixelFormatTypeKey".into()))?,
        format_dimensions: function(cm, c"CMVideoFormatDescriptionGetDimensions")?,
        format_subtype: function(cm, c"CMFormatDescriptionGetMediaSubType")?,
        sample_image: function(cm, c"CMSampleBufferGetImageBuffer")?,
        lock: function(cv, c"CVPixelBufferLockBaseAddress")?,
        unlock: function(cv, c"CVPixelBufferUnlockBaseAddress")?,
        base: function(cv, c"CVPixelBufferGetBaseAddress")?,
        bytes_per_row: function(cv, c"CVPixelBufferGetBytesPerRow")?,
        width: function(cv, c"CVPixelBufferGetWidth")?,
        height: function(cv, c"CVPixelBufferGetHeight")?,
        pixel_format: function(cv, c"CVPixelBufferGetPixelFormatType")?,
    })
}

/// A constant's object.
fn object(address: usize) -> &'static AnyObject {
    // SAFETY: `address` is a constant string read by `load`, valid for the process' lifetime.
    unsafe { &*(address as *const AnyObject) }
}

fn class(name: &CStr) -> Result<&'static AnyClass> {
    AnyClass::get(name).ok_or_else(|| Error::Unavailable(format!("class {}", name.to_string_lossy())))
}

fn responds(obj: &AnyObject, sel: Sel) -> bool {
    // SAFETY: every NSObject implements `respondsToSelector:` returning BOOL.
    unsafe { msg_send![obj, respondsToSelector: sel] }
}

/// A string property; `None` when missing or nil.
fn string(obj: &AnyObject, sel: Sel) -> Option<String> {
    if !responds(obj, sel) {
        return None;
    }
    // SAFETY: callers pass getters returning `NSString *`, nil or not.
    let value: Option<Obj> = unsafe { msg_send![obj, performSelector: sel] };
    let s = value?.downcast::<NSString>().ok()?.to_string();
    (!s.is_empty()).then_some(s)
}

/// Sends a no-argument message whose return is a scalar or a CF pointer, through `objc_msgSend`
/// directly: objc2's checked `msg_send!` rejects CF types (`^{opaqueCMFormatDescription=}`) as
/// `*const c_void`.
///
/// # Safety
/// `sel` must name a no-argument method of `obj` whose return type is `T`.
unsafe fn send<T: Copy>(obj: &AnyObject, sel: Sel) -> T {
    type Getter<T> = unsafe extern "C" fn(*const AnyObject, Sel) -> T;
    // SAFETY: `objc_msgSend` is called through the target method's signature (the caller's
    // contract).
    let f: Getter<T> = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
    // SAFETY: `obj` is a valid object and responds to `sel` (the caller's contract).
    unsafe { f(obj, sel) }
}

fn array(array: &AnyObject) -> Vec<Obj> {
    // SAFETY: callers pass NSArray instances; `count` returns NSUInteger.
    let count: usize = unsafe { msg_send![array, count] };
    // SAFETY: indices are in bounds; `objectAtIndex:` returns a non-nil element.
    (0..count).map(|i| unsafe { msg_send![array, objectAtIndex: i] }).collect()
}

fn fourcc(code: u32) -> String {
    code.to_be_bytes().iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' }).collect()
}

pub(crate) fn authorization_status() -> Result<Authorization> {
    // The calls autorelease objects: drain them here, not when this (long-lived) thread exits.
    autoreleasepool(|_| authorization_status_inner())
}

fn authorization_status_inner() -> Result<Authorization> {
    let api = api()?;
    let device = class(c"AVCaptureDevice")?;
    // SAFETY: `+authorizationStatusForMediaType:` takes an AVMediaType string and returns an
    // AVAuthorizationStatus (NSInteger); it never prompts.
    let raw: isize = unsafe { msg_send![device, authorizationStatusForMediaType: object(api.video)] };
    Authorization::from_av_status(raw).ok_or_else(|| Error::Unavailable(format!("unknown AVAuthorizationStatus {raw}")))
}

pub(crate) fn request_access() -> Result<Authorization> {
    let status = authorization_status()?;
    if status != Authorization::NotDetermined {
        return Ok(status);
    }
    let api = api()?;
    let device = class(c"AVCaptureDevice")?;
    let (tx, rx) = mpsc::channel::<bool>();
    let block = RcBlock::new(move |granted: Bool| {
        let _ = tx.send(granted.as_bool());
    });
    // SAFETY: `+requestAccessForMediaType:completionHandler:` takes an AVMediaType and a
    // `void (^)(BOOL)` block, which it copies and calls once on an arbitrary queue.
    let _: () = unsafe { msg_send![device, requestAccessForMediaType: object(api.video), completionHandler: &*block] };
    rx.recv_timeout(Duration::from_secs(600)).map_err(|_| Error::Capture("no answer to the camera prompt".into()))?;
    authorization_status()
}

pub(crate) fn cameras() -> Result<Vec<HostCamera>> {
    let api = api()?;
    let discovery = class(c"AVCaptureDeviceDiscoverySession")?;
    autoreleasepool(|_| {
        // SAFETY: `+[NSMutableArray array]` returns a new empty mutable array.
        let types: Option<Obj> = unsafe { msg_send![class(c"NSMutableArray")?, array] };
        let types = types.ok_or_else(|| Error::Unavailable("NSMutableArray".into()))?;
        for &t in &api.device_types {
            // SAFETY: `addObject:` takes a non-nil object (a device type constant).
            let _: () = unsafe { msg_send![&*types, addObject: object(t)] };
        }
        // SAFETY: takes an NSArray of device types, a media type and a position
        // (AVCaptureDevicePositionUnspecified, 0); returns a session or nil.
        let session: Option<Obj> =
            unsafe { msg_send![discovery, discoverySessionWithDeviceTypes: &*types, mediaType: object(api.video), position: 0isize] };
        let Some(session) = session else { return Ok(Vec::new()) };
        // SAFETY: `devices` returns an NSArray of AVCaptureDevice.
        let devices: Option<Obj> = unsafe { msg_send![&*session, devices] };
        Ok(devices.map(|d| array(&d)).unwrap_or_default().iter().filter_map(|d| describe(api, d)).collect())
    })
}

fn describe(api: &Api, device: &AnyObject) -> Option<HostCamera> {
    let id = string(device, sel!(uniqueID))?;
    let name = string(device, sel!(localizedName)).unwrap_or_else(|| id.clone());
    let manufacturer = string(device, sel!(manufacturer));
    let model = string(device, sel!(modelID));
    let device_type = string(device, sel!(deviceType)).unwrap_or_default();
    // SAFETY: `transportType` is a no-argument getter returning int32_t (checked to exist).
    let transport = responds(device, sel!(transportType)).then(|| unsafe { send::<i32>(device, sel!(transportType)) } as u32);
    let texts = [name.as_str(), manufacturer.as_deref().unwrap_or(""), model.as_deref().unwrap_or("")];
    let (kind, is_virtual) = classify_apple(&device_type, transport.unwrap_or(0), &texts);
    Some(HostCamera {
        id,
        name,
        kind,
        is_virtual,
        manufacturer,
        model,
        transport: transport.filter(|&t| t != 0).map(fourcc),
        path: None,
        formats: formats(api, device),
    })
}

fn formats(api: &Api, device: &AnyObject) -> Vec<Format> {
    if !responds(device, sel!(formats)) {
        return Vec::new();
    }
    // SAFETY: `formats` returns an NSArray of AVCaptureDeviceFormat.
    let list: Option<Obj> = unsafe { msg_send![device, formats] };
    let mut out: Vec<Format> = Vec::new();
    for format in list.map(|l| array(&l)).unwrap_or_default() {
        if !responds(&format, sel!(formatDescription)) {
            continue;
        }
        // SAFETY: `formatDescription` returns a CMFormatDescriptionRef owned by the format.
        let desc: *const c_void = unsafe { send(&format, sel!(formatDescription)) };
        if desc.is_null() {
            continue;
        }
        // SAFETY: `desc` is a live video format description (the format retains it).
        let (dims, subtype) = unsafe { ((api.format_dimensions)(desc), (api.format_subtype)(desc)) };
        let max_fps = max_fps(&format);
        let (width, height) = (dims.width.max(0) as u32, dims.height.max(0) as u32);
        let pixel_format = fourcc(subtype);
        match out.iter_mut().find(|f| (f.width, f.height, f.pixel_format.as_str()) == (width, height, pixel_format.as_str())) {
            Some(f) => f.max_fps = f.max_fps.into_iter().chain(max_fps).reduce(f64::max),
            None => out.push(Format { width, height, pixel_format, max_fps }),
        }
    }
    out
}

fn max_fps(format: &AnyObject) -> Option<f64> {
    if !responds(format, sel!(videoSupportedFrameRateRanges)) {
        return None;
    }
    // SAFETY: returns an NSArray of AVFrameRateRange.
    let ranges: Option<Obj> = unsafe { msg_send![format, videoSupportedFrameRateRanges] };
    // SAFETY: `maxFrameRate` returns Float64.
    ranges.map(|r| array(&r)).unwrap_or_default().iter().map(|r| unsafe { msg_send![&**r, maxFrameRate] }).reduce(f64::max)
}

/// A frame the delegate copied out, and when.
struct Arrived {
    width: u32,
    height: u32,
    bgra: Arc<[u8]>,
    at: Instant,
}

/// The newest frame from the camera.
#[derive(Default)]
struct Latest {
    frame: Mutex<Option<Arrived>>,
    arrived: Condvar,
}

struct Ivars {
    latest: Arc<Latest>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `SampleDelegate` doesn't implement
    // `Drop`; its ivars are `Send + Sync`.
    #[unsafe(super(NSObject))]
    #[name = "SimeeCameraSampleDelegate"]
    #[ivars = Ivars]
    struct SampleDelegate;

    impl SampleDelegate {
        /// `AVCaptureVideoDataOutputSampleBufferDelegate`: one BGRA frame, on the output's queue.
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, _output: *mut AnyObject, sample: *mut c_void, _connection: *mut AnyObject) {
            // A panic must not unwind into Objective-C (it would abort the process): drop the frame.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Ok(api) = api() else { return };
                if let Some((width, height, px)) = copy_sample(api, sample) {
                    let latest = &self.ivars().latest;
                    let arrived = Arrived { width, height, bgra: px.into(), at: Instant::now() };
                    *latest.frame.lock().unwrap_or_else(|p| p.into_inner()) = Some(arrived);
                    latest.arrived.notify_all();
                }
            }));
        }
    }

    // SAFETY: NSObjectProtocol asks nothing beyond NSObject's own implementation.
    unsafe impl NSObjectProtocol for SampleDelegate {}
);

impl SampleDelegate {
    fn new(latest: Arc<Latest>) -> Retained<SampleDelegate> {
        let this = SampleDelegate::alloc().set_ivars(Ivars { latest });
        // SAFETY: NSObject's `init` on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Copies a sample buffer's BGRA pixels into tight rows.
fn copy_sample(api: &Api, sample: *mut c_void) -> Option<(u32, u32, Vec<u8>)> {
    if sample.is_null() {
        return None;
    }
    // SAFETY: `sample` is the live CMSampleBuffer the delegate call was given.
    let pb = unsafe { (api.sample_image)(sample) };
    // SAFETY: `pb` is the sample's image buffer (a CVPixelBuffer), valid during the call.
    if pb.is_null() || unsafe { (api.pixel_format)(pb) } != FORMAT_BGRA {
        return None;
    }
    // SAFETY: as above; read-only lock, released below.
    if unsafe { (api.lock)(pb, LOCK_READ_ONLY) } != 0 {
        return None;
    }
    // SAFETY: the buffer is locked, so its base address and geometry are valid until unlock.
    let (base, stride, w, h) = unsafe { ((api.base)(pb), (api.bytes_per_row)(pb), (api.width)(pb), (api.height)(pb)) };
    // A buffer reporting no pixels, or rows shorter than its width, is dropped, not sliced.
    let usable = !base.is_null()
        && w > 0
        && h > 0
        && w <= u32::MAX as usize
        && h <= u32::MAX as usize
        && stride > 0
        && stride <= u32::MAX as usize
        && w.checked_mul(4).is_some_and(|row| stride >= row);
    let out = usable.then(|| {
        // SAFETY: a locked BGRA buffer holds `h` rows of `stride` bytes at `base`.
        let src = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), stride * h) };
        let mut px = Vec::with_capacity(w * h * 4);
        for row in src.chunks_exact(stride) {
            px.extend_from_slice(&row[..w * 4]);
        }
        (w as u32, h as u32, px)
    });
    // SAFETY: balances the lock above.
    unsafe { (api.unlock)(pb, LOCK_READ_ONLY) };
    out
}

extern "C" fn drained(_: *mut c_void) {}

/// A host camera capturing through AVFoundation, at the camera's own format, scaled to each
/// request.
pub(crate) struct Webcam {
    name: String,
    device: Obj,
    session: Obj,
    output: Obj,
    _delegate: Retained<SampleDelegate>,
    queue: *mut c_void,
    latest: Arc<Latest>,
}

// SAFETY: AVCaptureSession, its outputs and devices may be messaged from any thread (AVFoundation
// recommends starting and stopping sessions off the main thread); one capture thread owns this
// and is the only one that messages them.
unsafe impl Send for Webcam {}

impl Webcam {
    pub(crate) fn open(camera: &HostCamera, spec: FrameSpec) -> Result<Webcam> {
        // Session setup autoreleases objects (and the NSError out-parameter): drain them here, not
        // when this (long-lived) thread exits.
        autoreleasepool(|_| Self::open_inner(camera, spec))
    }

    fn open_inner(camera: &HostCamera, _spec: FrameSpec) -> Result<Webcam> {
        let api = api()?;
        let unique_id = NSString::from_str(&camera.id);
        // SAFETY: `+deviceWithUniqueID:` takes an NSString and returns a device or nil.
        let device: Option<Obj> = unsafe { msg_send![class(c"AVCaptureDevice")?, deviceWithUniqueID: &*unique_id] };
        let device = device.ok_or_else(|| Error::NotFound { query: camera.id.clone(), available: Vec::new() })?;
        let mut error: *mut NSError = ptr::null_mut();
        // SAFETY: takes a device and an NSError** out-parameter; returns an input or nil.
        let input: Option<Obj> = unsafe { msg_send![class(c"AVCaptureDeviceInput")?, deviceInputWithDevice: &*device, error: &mut error] };
        let input = input.ok_or_else(|| {
            // SAFETY: on failure the out-parameter holds nil or an autoreleased NSError.
            let reason = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_default();
            Error::Capture(format!("can't open {}: {reason}", camera.name))
        })?;
        // SAFETY: `+new` returns a new session.
        let session: Option<Obj> = unsafe { msg_send![class(c"AVCaptureSession")?, new] };
        let session = session.ok_or_else(|| Error::Unavailable("AVCaptureSession".into()))?;
        // SAFETY: `canAddInput:` takes an input and returns BOOL; `addInput:` is only sent when
        // it may be (otherwise it raises).
        if !unsafe { msg_send![&*session, canAddInput: &*input] } {
            return Err(Error::Capture(format!("{} is busy or can't be captured", camera.name)));
        }
        // SAFETY: checked above.
        let _: () = unsafe { msg_send![&*session, addInput: &*input] };

        // SAFETY: `+new` returns a new video data output.
        let output: Option<Obj> = unsafe { msg_send![class(c"AVCaptureVideoDataOutput")?, new] };
        let output = output.ok_or_else(|| Error::Unavailable("AVCaptureVideoDataOutput".into()))?;
        // SAFETY: `+numberWithUnsignedInt:` returns a number; `+dictionaryWithObject:forKey:`
        // takes a value and a copyable key (the CFString constant, toll-free bridged).
        let settings: Option<Obj> = unsafe {
            let number: Obj = msg_send![class(c"NSNumber")?, numberWithUnsignedInt: FORMAT_BGRA];
            msg_send![class(c"NSDictionary")?, dictionaryWithObject: &*number, forKey: object(api.pixel_format_key)]
        };
        let settings = settings.ok_or_else(|| Error::Unavailable("NSDictionary".into()))?;
        // SAFETY: `setVideoSettings:` takes a dictionary; `setAlwaysDiscardsLateVideoFrames:` a BOOL.
        unsafe {
            let _: () = msg_send![&*output, setVideoSettings: &*settings];
            let _: () = msg_send![&*output, setAlwaysDiscardsLateVideoFrames: true];
        }
        let latest = Arc::new(Latest::default());
        let delegate = SampleDelegate::new(latest.clone());
        // SAFETY: a NUL-terminated label and a null (serial) attribute.
        let queue = unsafe { dispatch_queue_create(c"simee.camera".as_ptr(), ptr::null()) };
        // SAFETY: takes the delegate and a dispatch queue (an object); the output retains the queue.
        let _: () = unsafe { msg_send![&*output, setSampleBufferDelegate: &*delegate, queue: queue.cast_const().cast::<AnyObject>()] };
        let webcam = Webcam { name: camera.name.clone(), device, session, output, _delegate: delegate, queue, latest };
        // SAFETY: `canAddOutput:` returns BOOL; `addOutput:` only when it may be.
        if !unsafe { msg_send![&*webcam.session, canAddOutput: &*webcam.output] } {
            return Err(Error::Capture(format!("{} can't deliver video frames", camera.name)));
        }
        // SAFETY: checked above; `startRunning` blocks until the session runs or fails.
        unsafe {
            let _: () = msg_send![&*webcam.session, addOutput: &*webcam.output];
            let _: () = msg_send![&*webcam.session, startRunning];
        }
        Ok(webcam)
    }

    /// Why frames aren't arriving: the permission, a disconnected camera, or neither.
    fn stalled(&self) -> Error {
        autoreleasepool(|_| self.stalled_inner())
    }

    fn stalled_inner(&self) -> Error {
        if let Ok(status) = authorization_status()
            && let Some(fix) = status.fix()
        {
            return Error::Permission { status, fix: fix.to_string() };
        }
        // SAFETY: `isConnected` returns BOOL.
        let connected: bool = unsafe { msg_send![&*self.device, isConnected] };
        if !connected {
            return Error::Capture(format!("{} was disconnected", self.name));
        }
        Error::Capture(format!("no frames from {} for {} s", self.name, FRAME_TIMEOUT.as_secs()))
    }
}

impl Source for Webcam {
    fn frame(&mut self, spec: FrameSpec, tick: Tick) -> Result<CameraFrame> {
        let deadline = Instant::now() + FRAME_TIMEOUT;
        let mut guard = self.latest.frame.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(arrived) = guard.as_ref() {
                if arrived.at.elapsed() > FRAME_TIMEOUT {
                    drop(guard);
                    return Err(self.stalled());
                }
                let frame = CameraFrame { width: arrived.width, height: arrived.height, bgra: arrived.bgra.clone(), tick };
                drop(guard);
                return Ok(frame.fit(spec.width, spec.height));
            }
            let now = Instant::now();
            if now >= deadline {
                drop(guard);
                return Err(self.stalled());
            }
            guard = self.latest.arrived.wait_timeout(guard, deadline - now).map(|(g, _)| g).unwrap_or_else(|p| p.into_inner().0);
        }
    }
}

impl Drop for Webcam {
    fn drop(&mut self) {
        // SAFETY: `stopRunning` blocks until the session stops; a nil delegate and queue detach
        // the delegate; the empty synchronous block then waits out a callback already running on
        // the queue, after which the queue is released (the output holds its own reference).
        unsafe {
            let _: () = msg_send![&*self.session, stopRunning];
            let none: *const AnyObject = ptr::null();
            let _: () = msg_send![&*self.output, setSampleBufferDelegate: none, queue: none];
            dispatch_sync_f(self.queue, ptr::null_mut(), drained);
            dispatch_release(self.queue);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourccs_are_big_endian() {
        assert_eq!(fourcc(u32::from_be_bytes(*b"420v")), "420v");
        assert_eq!(fourcc(crate::catalog::TRANSPORT_VIRTUAL), "virt");
        assert_eq!(fourcc(0), "????");
    }

    #[test]
    fn avfoundation_loads_and_discovers_device_types() {
        let api = api().unwrap_or_else(|e| panic!("{e}"));
        assert!(!api.device_types.is_empty(), "at least the built-in camera type");
    }
}
