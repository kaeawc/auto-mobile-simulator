//! Kernel values by `sysctlbyname`, for the host-health sampler (`sim-host`): one syscall per read,
//! no process spawned. A name the kernel doesn't know (or a size that doesn't fit) is `None`.

use std::ffi::{CStr, CString, c_char, c_void};

use objc2::runtime::{AnyClass, AnyObject};
use objc2::{msg_send, sel};

/// The raw bytes of the sysctl `name`, `None` when it doesn't exist or can't be read.
pub fn sysctl(name: &str) -> Option<Vec<u8>> {
    let name = CString::new(name).ok()?;
    let mut len: libc::size_t = 0;
    // SAFETY: `name` is a NUL-terminated string; a null `oldp` with a valid `oldlenp` asks only for
    // the size; no new value is set (`newp` null, `newlen` 0).
    let rc = unsafe { libc::sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || len == 0 {
        return None;
    }
    // A value can grow between the two calls: leave room, then trim to what was written.
    let mut buf = vec![0u8; len + 16];
    let mut cap: libc::size_t = buf.len();
    // SAFETY: `buf` is `cap` writable bytes, `cap` is updated to the bytes written; no value is set.
    let rc = unsafe { libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut cap, std::ptr::null_mut(), 0) };
    if rc != 0 {
        return None;
    }
    buf.truncate(cap);
    Some(buf)
}

/// `NSProcessInfo.processInfo.thermalState`: 0 nominal, 1 fair, 2 serious, 3 critical. `None`
/// when the class or selector is missing (never a crash).
pub fn thermal_state() -> Option<i64> {
    let info = process_info()?;
    // SAFETY: `info` is a live NSProcessInfo (a process-lifetime singleton); `thermalState` takes
    // no arguments and returns an NSInteger.
    Some(unsafe { msg_send![info, thermalState] })
}

/// `NSProcessInfo.processInfo.isLowPowerModeEnabled` (macOS 12+), `None` when unavailable.
pub fn low_power_mode() -> Option<bool> {
    let info = process_info()?;
    // SAFETY: as `thermal_state`; `isLowPowerModeEnabled` returns a BOOL.
    Some(unsafe { msg_send![info, isLowPowerModeEnabled] })
}

fn process_info() -> Option<&'static AnyObject> {
    let class = AnyClass::get(c"NSProcessInfo")?;
    // SAFETY: `processInfo` is a class method with no arguments returning the shared instance,
    // which lives for the process's lifetime.
    let info: Option<&'static AnyObject> = unsafe { msg_send![class, processInfo] };
    info.filter(|i| i.class().responds_to(sel!(thermalState)))
}

/// The power sources IOKit reports (`IOPSCopyPowerSourcesInfo`): no `pmset`, no process spawned.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PowerInfo {
    /// The providing source: `AC Power`, `Battery Power` or `UPS Power`; `None` when unreadable.
    pub providing: Option<String>,
    /// An internal battery is present.
    pub has_battery: bool,
    /// Charge, percent of the maximum capacity.
    pub percent: Option<u8>,
    pub charging: Option<bool>,
    /// Minutes until empty on battery, when the system has an estimate.
    pub minutes_remaining: Option<u32>,
}

type CfRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CfRef);
    fn CFArrayGetCount(a: CfRef) -> isize;
    fn CFArrayGetValueAtIndex(a: CfRef, i: isize) -> CfRef;
    fn CFDictionaryGetValue(d: CfRef, key: CfRef) -> CfRef;
    fn CFStringCreateWithCString(alloc: CfRef, s: *const c_char, enc: u32) -> CfRef;
    fn CFStringGetCString(s: CfRef, buf: *mut c_char, size: isize, enc: u32) -> u8;
    fn CFNumberGetValue(n: CfRef, ty: isize, out: *mut c_void) -> u8;
    fn CFBooleanGetValue(b: CfRef) -> u8;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CfRef;
    fn IOPSCopyPowerSourcesList(info: CfRef) -> CfRef;
    fn IOPSGetPowerSourceDescription(info: CfRef, ps: CfRef) -> CfRef;
    fn IOPSGetProvidingPowerSourceType(info: CfRef) -> CfRef;
}

const UTF8: u32 = 0x0800_0100;
const NUMBER_SINT64: isize = 4;

/// A CFString as a Rust string.
fn cf_string(s: CfRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let mut buf = [0 as c_char; 128];
    // SAFETY: `s` is a CFString (the caller got it from IOKit), `buf` is 128 writable bytes.
    let ok = unsafe { CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, UTF8) };
    // SAFETY: on success `buf` holds a NUL-terminated string.
    (ok != 0).then(|| unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
}

/// The value of `key` in a power source description (a CFDictionary), unowned.
fn dict_get(dict: CfRef, key: &CStr) -> CfRef {
    // SAFETY: `key` is NUL-terminated; the created string is released after the lookup; `dict` is
    // a live CFDictionary owned by the power sources info.
    unsafe {
        let k = CFStringCreateWithCString(std::ptr::null(), key.as_ptr(), UTF8);
        if k.is_null() {
            return std::ptr::null();
        }
        let v = CFDictionaryGetValue(dict, k);
        CFRelease(k);
        v
    }
}

fn dict_int(dict: CfRef, key: &CStr) -> Option<i64> {
    let n = dict_get(dict, key);
    if n.is_null() {
        return None;
    }
    let mut out: i64 = 0;
    // SAFETY: `n` is a CFNumber (the power source keys used here are numbers); `out` is a valid i64.
    let ok = unsafe { CFNumberGetValue(n, NUMBER_SINT64, (&mut out as *mut i64).cast()) };
    (ok != 0).then_some(out)
}

fn dict_bool(dict: CfRef, key: &CStr) -> Option<bool> {
    let b = dict_get(dict, key);
    // SAFETY: `b` is a CFBoolean (the power source keys used here are booleans).
    if b.is_null() { None } else { Some(unsafe { CFBooleanGetValue(b) } != 0) }
}

/// Power sources from IOKit; the default (no battery, nothing known) when IOKit gives nothing.
pub fn power_sources() -> PowerInfo {
    let mut out = PowerInfo::default();
    // SAFETY: the Copy functions return owned references, released below; the Get functions return
    // borrowed ones valid while `info` lives; every pointer is null-checked before use.
    unsafe {
        let info = IOPSCopyPowerSourcesInfo();
        if info.is_null() {
            return out;
        }
        out.providing = cf_string(IOPSGetProvidingPowerSourceType(info));
        let list = IOPSCopyPowerSourcesList(info);
        if !list.is_null() {
            for i in 0..CFArrayGetCount(list) {
                let desc = IOPSGetPowerSourceDescription(info, CFArrayGetValueAtIndex(list, i));
                if desc.is_null() || cf_string(dict_get(desc, c"Type")).as_deref() != Some("InternalBattery") {
                    continue;
                }
                out.has_battery = true;
                let (cur, max) = (dict_int(desc, c"Current Capacity"), dict_int(desc, c"Max Capacity"));
                out.percent = match (cur, max) {
                    (Some(c), Some(m)) if m > 0 => Some((c * 100 / m).clamp(0, 100) as u8),
                    _ => None,
                };
                out.charging = dict_bool(desc, c"Is Charging");
                out.minutes_remaining = dict_int(desc, c"Time to Empty").and_then(|m| u32::try_from(m).ok());
            }
            CFRelease(list);
        }
        CFRelease(info);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thermal_and_power_read_on_this_host() {
        assert!(thermal_state().is_some_and(|s| (0..=3).contains(&s)));
        assert!(low_power_mode().is_some());
        let p = power_sources();
        assert!(p.providing.is_some(), "{p:?}");
    }

    #[test]
    fn reads_a_known_sysctl_and_refuses_an_unknown_one() {
        let ncpu = sysctl("hw.ncpu").expect("hw.ncpu");
        assert_eq!(ncpu.len(), 4);
        assert!(sysctl("simee.no.such.sysctl").is_none());
    }
}
