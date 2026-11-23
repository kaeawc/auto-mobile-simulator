//! Service context, device sets and devices.

use std::process::Command;
use std::ptr;

use block2::{Block, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{msg_send, sel};
use objc2_foundation::{NSError, NSString};
use sim_core::device::{DeviceInfo, DeviceState, DeviceTypeInfo, RuntimeInfo, Selection, select};
use sim_core::form_factor::{apple_family_form_factor, apple_kind};
use sim_core::hid::Orientation;
use sim_core::platform::Platform;

use crate::display::Display;
use crate::error::{Error, Result};
use crate::hid::Hid;
use crate::rt::{self, CGSize, Library, Obj};

const CORE_SIMULATOR: &str = "/Library/Developer/PrivateFrameworks/CoreSimulator.framework/CoreSimulator";

/// The active developer directory: `DEVELOPER_DIR`, else `xcode-select -p` (run once per process:
/// every context, API connection, capture and boot thread asks).
pub fn developer_dir() -> Result<String> {
    if let Ok(dir) = std::env::var("DEVELOPER_DIR")
        && !dir.is_empty()
    {
        return Ok(dir);
    }
    static SELECTED: std::sync::OnceLock<std::result::Result<String, String>> = std::sync::OnceLock::new();
    SELECTED
        .get_or_init(|| {
            let out = Command::new("/usr/bin/xcode-select").arg("-p").output().map_err(|e| format!("xcode-select -p: {e}"))?;
            let dir = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if out.status.success() && !dir.is_empty() { Ok(dir) } else { Err("xcode-select -p returned nothing".to_owned()) }
        })
        .clone()
        .map_err(Error::Unavailable)
}

/// Status bar overrides. Raw values follow the service's enums: data network 0 = hidden, 1 = Wi-Fi,
/// 2 = 3G, 3 = 4G, 4 = LTE, 5 = LTE-A, 6 = LTE+, 7 = 5G, 8 = 5G+, 9 = 5G UWB, 10 = 5G UC; Wi-Fi and
/// cellular modes 0 = not supported, 1 = searching, 2 = failed, 3 = active; battery state
/// 0 = charging, 1 = charged, 2 = discharging.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusBar {
    pub time: Option<String>,
    pub data_network: Option<u64>,
    pub wifi: Option<(u64, u64)>,
    pub cellular: Option<(u64, u64)>,
    pub operator: Option<String>,
    pub battery: Option<(u64, u64)>,
}

/// Output of a process run inside the device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub pid: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Light or dark UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

/// A connection to the simulator service and its default device set.
pub struct Context {
    context: Obj,
    set: Obj,
    kit: Library,
}

impl Context {
    /// Connects to the service for the active developer directory.
    pub fn new() -> Result<Self> {
        let dir = developer_dir()?;
        Library::open(CORE_SIMULATOR)?;
        let kit = Library::open(&format!("{dir}/Library/PrivateFrameworks/SimulatorKit.framework/SimulatorKit"))?;
        let cls = rt::class(c"SimServiceContext")?;
        if cls.class_method(sel!(sharedServiceContextForDeveloperDir:error:)).is_none() {
            return Err(Error::Unavailable("sharedServiceContextForDeveloperDir:error:".into()));
        }
        // SAFETY: the class method exists (checked) and takes (NSString, NSError**), returning an object.
        let context: std::result::Result<Obj, Retained<NSError>> =
            unsafe { msg_send![cls, sharedServiceContextForDeveloperDir: &*rt::nsstring(&dir), error: _] };
        let context = context.map_err(|e| rt::service_error("connect", &e))?;
        rt::require(&context, sel!(defaultDeviceSetWithError:))?;
        // SAFETY: the method exists (checked) and takes NSError**, returning the device set.
        let set: std::result::Result<Obj, Retained<NSError>> = unsafe { msg_send![&*context, defaultDeviceSetWithError: _] };
        let set = set.map_err(|e| rt::service_error("open device set", &e))?;
        Ok(Self { context, set, kit })
    }

    /// All devices in the default set, sorted by runtime then name.
    pub fn devices(&self) -> Result<Vec<Device>> {
        let list = rt::object_prop(&self.set, sel!(devices))?;
        let mut devices: Vec<Device> = rt::array_items(&list).into_iter().map(|obj| Device { obj, kit: self.kit }).collect();
        devices.sort_by_cached_key(|d| {
            let i = d.info();
            (i.runtime_id, i.name, i.udid)
        });
        Ok(devices)
    }

    /// Resolves a UDID, a name, or `booted`.
    pub fn device(&self, query: &str) -> Result<Device> {
        self.resolve(query).map(|(device, _)| device)
    }

    /// Resolves a UDID, a name, or `booted`, with the device's info (read while selecting).
    pub fn resolve(&self, query: &str) -> Result<(Device, DeviceInfo)> {
        // Unsorted, and each device's info read once (it's several Objective-C calls): `select`
        // doesn't depend on order.
        let list = rt::object_prop(&self.set, sel!(devices))?;
        let mut devices: Vec<Device> = rt::array_items(&list).into_iter().map(|obj| Device { obj, kit: self.kit }).collect();
        let mut infos: Vec<DeviceInfo> = devices.iter().map(Device::info).collect();
        match select(&infos, query) {
            Selection::One(i) => Ok((devices.swap_remove(i), infos.swap_remove(i))),
            Selection::None => Err(Error::NotFound(query.to_owned())),
            Selection::Ambiguous => Err(Error::Ambiguous(query.to_owned())),
        }
    }

    /// Installed runtimes.
    pub fn runtimes(&self) -> Result<Vec<RuntimeInfo>> {
        Ok(self.runtime_objects()?.iter().map(|r| runtime_info(r)).collect())
    }

    /// Known device types.
    pub fn device_types(&self) -> Result<Vec<DeviceTypeInfo>> {
        Ok(self.device_type_objects()?.iter().map(|t| device_type_info(t)).collect())
    }

    fn runtime_objects(&self) -> Result<Vec<Obj>> {
        Ok(rt::array_items(&*rt::object_prop(&self.context, sel!(supportedRuntimes))?))
    }

    fn device_type_objects(&self) -> Result<Vec<Obj>> {
        Ok(rt::array_items(&*rt::object_prop(&self.context, sel!(supportedDeviceTypes))?))
    }

    /// Creates a device. `device_type` and `runtime` match by identifier or name.
    pub fn create(&self, name: &str, device_type: &str, runtime: &str) -> Result<Device> {
        let matches = |obj: &AnyObject, q: &str| {
            rt::string_prop(obj, sel!(identifier)).eq_ignore_ascii_case(q) || rt::string_prop(obj, sel!(name)).eq_ignore_ascii_case(q)
        };
        let ty =
            self.device_type_objects()?.into_iter().find(|t| matches(t, device_type)).ok_or_else(|| Error::NotFound(device_type.into()))?;
        let rt_obj = self.runtime_objects()?.into_iter().find(|r| matches(r, runtime)).ok_or_else(|| Error::NotFound(runtime.into()))?;
        rt::require(&self.set, sel!(createDeviceWithType:runtime:name:error:))?;
        // SAFETY: the method exists (checked) and takes (SimDeviceType, SimRuntime, NSString, NSError**).
        let created: std::result::Result<Obj, Retained<NSError>> =
            unsafe { msg_send![&*self.set, createDeviceWithType: &*ty, runtime: &*rt_obj, name: &*rt::nsstring(name), error: _] };
        Ok(Device { obj: created.map_err(|e| rt::service_error("create", &e))?, kit: self.kit })
    }

    /// Deletes a device and its data.
    pub fn delete(&self, device: &Device) -> Result<()> {
        rt::require(&self.set, sel!(deleteDevice:error:))?;
        // SAFETY: the method exists (checked) and takes (SimDevice, NSError**), returning BOOL.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*self.set, deleteDevice: &*device.obj, error: _] };
        done.map_err(|e| rt::service_error("delete", &e))
    }
}

fn runtime_info(r: &AnyObject) -> RuntimeInfo {
    RuntimeInfo {
        identifier: rt::string_prop(r, sel!(identifier)),
        name: rt::string_prop(r, sel!(name)),
        version: rt::string_prop(r, sel!(versionString)),
        build: rt::string_prop(r, sel!(buildVersionString)),
        // SAFETY: `available` is a BOOL getter on SimRuntime (guarded by the responds check).
        available: rt::responds(r, sel!(available)) && unsafe { msg_send![r, available] },
        platform: Platform::Ios,
    }
}

fn device_type_info(t: &AnyObject) -> DeviceTypeInfo {
    let size: CGSize = if rt::responds(t, sel!(mainScreenSize)) {
        // SAFETY: `mainScreenSize` returns CGSize by value (checked to exist).
        unsafe { msg_send![t, mainScreenSize] }
    } else {
        CGSize::default()
    };
    let scale: f32 = if rt::responds(t, sel!(mainScreenScale)) {
        // SAFETY: `mainScreenScale` returns float (checked to exist).
        unsafe { msg_send![t, mainScreenScale] }
    } else {
        0.0
    };
    let product_family = rt::string_prop(t, sel!(productFamily));
    DeviceTypeInfo {
        identifier: rt::string_prop(t, sel!(identifier)),
        name: rt::string_prop(t, sel!(name)),
        model: rt::string_prop(t, sel!(modelIdentifier)),
        product_family: product_family.clone(),
        screen_width: size.width,
        screen_height: size.height,
        screen_scale: f64::from(scale),
        platform: Platform::Ios,
        form_factor: apple_family_form_factor(&product_family),
    }
}

/// One simulated device.
pub struct Device {
    pub(crate) obj: Obj,
    kit: Library,
}

/// A registered state handler ([`Device::watch_state`]); dropping it unregisters the handler.
pub struct StateWatch {
    device: Obj,
    handle: u64,
    _block: RcBlock<dyn Fn(*mut AnyObject)>,
}

impl Drop for StateWatch {
    fn drop(&mut self) {
        // SAFETY: the device responds to the selector (checked at registration), which takes
        // (unsigned long long, NSError **) and returns BOOL; the handle is the registration's.
        let done: std::result::Result<(), Retained<NSError>> =
            unsafe { msg_send![&*self.device, unregisterNotificationHandler: self.handle, error: _] };
        done.ok();
    }
}

const MACH_SEND_MSG: i32 = 0x1;
const MACH_SEND_TIMEOUT: i32 = 0x10;

unsafe extern "C" {
    static mach_task_self_: u32;
    fn mach_msg(msg: *mut std::ffi::c_void, option: i32, send_size: u32, rcv_size: u32, rcv_name: u32, timeout: u32, notify: u32) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
}

/// Runs a `BOOL`-returning, `NSError**`-taking device method by name.
macro_rules! device_call {
    ($self:ident, $op:literal, $($sel:ident : $arg:expr),+) => {{
        rt::require(&$self.obj, sel!($($sel:)+ error:))?;
        // SAFETY: the method exists (checked); argument types match the selector's signature, which
        // objc2 additionally verifies against the runtime encoding in debug builds.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*$self.obj, $($sel: $arg,)+ error: _] };
        done.map_err(|e| rt::service_error($op, &e))
    }};
}

impl Device {
    /// A snapshot of the device's identity and state.
    pub fn info(&self) -> DeviceInfo {
        let d = &*self.obj;
        // SAFETY: `UDID` returns an NSUUID and `UUIDString` its string form.
        let udid = rt::object_prop(d, sel!(UDID)).map(|u| rt::string_prop(&u, sel!(UUIDString))).unwrap_or_default();
        let runtime_id = rt::string_prop(d, sel!(runtimeIdentifier));
        let device_type_id = rt::string_prop(d, sel!(deviceTypeIdentifier));
        let kind = apple_kind(&device_type_id, &runtime_id);
        DeviceInfo {
            udid,
            name: rt::string_prop(d, sel!(name)),
            state: self.state(),
            runtime_id,
            device_type_id,
            // SAFETY: `available` is a BOOL getter on SimDevice (guarded by the responds check).
            available: rt::responds(d, sel!(available)) && unsafe { msg_send![d, available] },
            platform: Platform::Ios,
            form_factor: None,
            sub_platform: None,
        }
        .with_kind(kind)
    }

    /// Current lifecycle state.
    pub fn state(&self) -> DeviceState {
        if !rt::responds(&self.obj, sel!(state)) {
            return DeviceState::Unknown;
        }
        // SAFETY: `state` returns an unsigned long long (checked to exist).
        DeviceState::from_raw(unsafe { msg_send![&*self.obj, state] })
    }

    /// When the device's current (or last) boot began, in seconds since the reference date; `None`
    /// when the service doesn't say. A reboot changes it.
    pub fn last_booted_at(&self) -> Option<f64> {
        let date = rt::optional_prop(&self.obj, sel!(lastBootedAt))?;
        // SAFETY: an NSDate's `timeIntervalSinceReferenceDate` takes no arguments and returns a double.
        unsafe { rt::send_scalar(&date, sel!(timeIntervalSinceReferenceDate)) }
    }

    /// Calls `on_change` whenever the device's state changes (booting, booted, shutting down,
    /// shut down), until the returned watch drops: the device's `device_state` notifications, the
    /// signal idb drops its HID connection on. `on_change` runs on a service queue: keep it short.
    pub fn watch_state(&self, on_change: impl Fn() + Send + Sync + 'static) -> Result<StateWatch> {
        let register = sel!(registerNotificationHandler:);
        rt::require(&self.obj, register)?;
        rt::require(&self.obj, sel!(unregisterNotificationHandler:error:))?;
        let block = RcBlock::new(move |info: *mut AnyObject| {
            // A notification that can't be read counts as a change: reconnecting is always safe.
            let state = crate::autoreleasepool(|| {
                // SAFETY: the service passes nil or a valid NSDictionary for the duration of the call.
                unsafe { info.as_ref() }.and_then(|d| rt::dict_string(d, "notification")).is_none_or(|n| n == "device_state")
            });
            if state {
                on_change();
            }
        });
        type RegisterFn = unsafe extern "C" fn(*const AnyObject, Sel, *const Block<dyn Fn(*mut AnyObject)>) -> u64;
        // SAFETY: the method's signature is (void (^)(NSDictionary *)) -> unsigned long long.
        let send: RegisterFn = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
        // SAFETY: the device responds to the selector (checked); the service copies the block, and
        // the watch keeps ours alive until it unregisters.
        let handle = unsafe { send(&*self.obj, register, &*block) };
        Ok(StateWatch { device: self.obj.clone(), handle, _block: block })
    }

    /// Whether the device reports its boot sequence finished (data migration done, SpringBoard up).
    /// `None` when the service does not expose boot status.
    pub fn boot_finished(&self) -> Option<bool> {
        let info = rt::object_prop(&self.obj, sel!(bootStatus)).ok()?;
        if !rt::responds(&info, sel!(isTerminalStatus)) {
            return None;
        }
        type Getter = unsafe extern "C" fn(*const AnyObject, objc2::runtime::Sel) -> bool;
        // SAFETY: `isTerminalStatus` takes no arguments and returns BOOL (checked to exist).
        let send: Getter = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
        // SAFETY: `info` is a valid object that responds to the selector.
        Some(unsafe { send(&*info, sel!(isTerminalStatus)) })
    }

    /// The device type's main screen size in pixels and its scale factor.
    pub fn device_type(&self) -> Result<DeviceTypeInfo> {
        Ok(device_type_info(&*rt::object_prop(&self.obj, sel!(deviceType))?))
    }

    /// Boots the device; returns once the service reports it booted.
    pub fn boot(&self) -> Result<()> {
        self.boot_with_env(&[])
    }

    /// Boots the device with extra environment for its launchd (`env` boot option).
    pub fn boot_with_env(&self, env: &[(String, String)]) -> Result<()> {
        let options = rt::mutable_dictionary();
        if !env.is_empty() {
            rt::dict_set(&options, "env", &rt::string_dictionary(env));
        }
        device_call!(self, "boot", bootWithOptions: &*options)
    }

    /// Shuts the device down.
    pub fn shutdown(&self) -> Result<()> {
        rt::require(&self.obj, sel!(shutdownWithError:))?;
        // SAFETY: the method exists (checked) and takes NSError**, returning BOOL.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*self.obj, shutdownWithError: _] };
        done.map_err(|e| rt::service_error("shutdown", &e))
    }

    /// Erases all content and settings. The device must be shut down.
    pub fn erase(&self) -> Result<()> {
        rt::require(&self.obj, sel!(eraseContentsAndSettingsWithError:))?;
        // SAFETY: the method exists (checked) and takes NSError**, returning BOOL.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*self.obj, eraseContentsAndSettingsWithError: _] };
        done.map_err(|e| rt::service_error("erase", &e))
    }

    /// Opens a URL on the device.
    pub fn open_url(&self, url: &str) -> Result<()> {
        // SAFETY: `+[NSURL URLWithString:]` takes an NSString and returns nil for malformed input.
        let parsed: Option<Obj> = unsafe { msg_send![objc2::class!(NSURL), URLWithString: &*rt::nsstring(url)] };
        let parsed = parsed.ok_or_else(|| Error::Invalid(format!("malformed URL: {url}")))?;
        device_call!(self, "open URL", openURL: &*parsed)
    }

    /// Installs the `.app` bundle at `path`.
    pub fn install(&self, path: &str) -> Result<()> {
        let absolute = std::path::absolute(path).map_err(|e| Error::Invalid(format!("{path}: {e}")))?;
        if !absolute.exists() {
            return Err(Error::Invalid(format!("no such app bundle: {}", absolute.display())));
        }
        device_call!(self, "install", installApplication: &*rt::file_url(&absolute.to_string_lossy()), withOptions: &*rt::empty_dictionary())
    }

    /// Uninstalls an app by bundle identifier.
    pub fn uninstall(&self, bundle_id: &str) -> Result<()> {
        device_call!(self, "uninstall", uninstallApplication: &*rt::nsstring(bundle_id), withOptions: &*rt::empty_dictionary())
    }

    /// Launches an app; returns its process id.
    pub fn launch(&self, bundle_id: &str) -> Result<i32> {
        rt::require(&self.obj, sel!(launchApplicationWithID:options:error:))?;
        let mut error: *mut NSError = ptr::null_mut();
        // SAFETY: the method exists (checked) and takes (NSString, NSDictionary, NSError**), returning int.
        let pid: i32 = unsafe {
            msg_send![&*self.obj, launchApplicationWithID: &*rt::nsstring(bundle_id), options: &*rt::empty_dictionary(), error: &mut error]
        };
        if pid > 0 { Ok(pid) } else { Err(rt::raw_error("launch", error)) }
    }

    /// Terminates an app by bundle identifier.
    pub fn terminate(&self, bundle_id: &str) -> Result<()> {
        device_call!(self, "terminate", terminateApplicationWithID: &*rt::nsstring(bundle_id))
    }

    /// Bundle identifiers of installed apps.
    pub fn installed_apps(&self) -> Result<Vec<String>> {
        rt::require(&self.obj, sel!(installedAppsWithError:))?;
        // SAFETY: the method exists (checked) and takes NSError**, returning an NSDictionary keyed by bundle id.
        let apps: std::result::Result<Obj, Retained<NSError>> = unsafe { msg_send![&*self.obj, installedAppsWithError: _] };
        let apps = apps.map_err(|e| rt::service_error("list apps", &e))?;
        // SAFETY: NSDictionary responds to `allKeys`, returning an NSArray.
        let keys: Obj = unsafe { msg_send![&*apps, allKeys] };
        let mut ids: Vec<String> = rt::array_items(&keys).iter().map(|k| rt::string_prop(k, sel!(description))).collect();
        ids.sort();
        Ok(ids)
    }

    /// Switches between light and dark UI.
    pub fn set_appearance(&self, appearance: Appearance) -> Result<()> {
        let style: isize = match appearance {
            Appearance::Light => 1,
            Appearance::Dark => 2,
        };
        device_call!(self, "set appearance", setUIInterfaceStyle: style)
    }

    /// Sets the simulated location.
    pub fn set_location(&self, latitude: f64, longitude: f64) -> Result<()> {
        device_call!(self, "set location", setLocationWithLatitude: latitude, andLongitude: longitude)
    }

    /// Grants or revokes a privacy permission (`all`, `photos`, `camera`, ...) for an app.
    pub fn set_privacy(&self, service: &str, bundle_id: &str, granted: bool) -> Result<()> {
        device_call!(self, "set privacy", setPrivacyAccessForService: &*rt::nsstring(service), bundleID: &*rt::nsstring(bundle_id), granted: granted)
    }

    /// Posts a Darwin notification inside the device.
    pub fn post_notification(&self, name: &str) -> Result<()> {
        device_call!(self, "post notification", postDarwinNotification: &*rt::nsstring(name))
    }

    /// Triggers a simulated memory warning.
    pub fn memory_warning(&self) -> Result<()> {
        rt::require(&self.obj, sel!(simulateMemoryWarningWithError:))?;
        // SAFETY: the method exists (checked) and takes NSError**, returning BOOL.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*self.obj, simulateMemoryWarningWithError: _] };
        done.map_err(|e| rt::service_error("memory warning", &e))
    }

    /// The device's data directory on the host (its whole filesystem lives under it).
    pub fn data_path(&self) -> String {
        rt::string_prop(&self.obj, sel!(dataPath))
    }

    /// The host path of the device's OS runtime root (the `/` that in-device programs see).
    pub fn runtime_root(&self) -> Result<String> {
        let runtime = rt::object_prop(&self.obj, sel!(runtime))?;
        let root = rt::string_prop(&runtime, sel!(root));
        if root.is_empty() { Err(Error::Unavailable("runtime root".into())) } else { Ok(root) }
    }

    /// Overrides status bar items. `None` leaves an item alone.
    pub fn override_status_bar(&self, o: &StatusBar) -> Result<()> {
        if let Some(time) = &o.time {
            device_call!(self, "status bar time", overrideStatusBarTimeString: &*rt::nsstring(time))?;
        }
        if let Some(net) = o.data_network {
            device_call!(self, "status bar network", overrideStatusBarDataNetworkType: net)?;
        }
        if let Some((mode, bars)) = o.wifi {
            device_call!(self, "status bar wifi", overrideStatusBarWiFiMode: mode, bars: bars)?;
        }
        if let Some((mode, bars)) = o.cellular {
            let operator = o.operator.clone().unwrap_or_default();
            device_call!(self, "status bar cellular", overrideStatusBarCellularMode: mode, operatorName: &*rt::nsstring(&operator), bars: bars)?;
        }
        if let Some((state, level)) = o.battery {
            device_call!(self, "status bar battery", overrideStatusBarBatteryState: state, batteryLevel: level, showNotCharging: false)?;
        }
        Ok(())
    }

    /// Removes every status bar override.
    pub fn clear_status_bar(&self) -> Result<()> {
        device_call!(self, "clear status bar", clearStatusBarOverrides: u64::MAX)
    }

    /// Delivers a push notification payload (APNs JSON) to an app.
    pub fn push(&self, bundle_id: &str, json: &str) -> Result<()> {
        let payload = rt::json_object(json)?;
        device_call!(self, "push", sendPushNotificationForBundleID: &*rt::nsstring(bundle_id), jsonPayload: &*payload)
    }

    /// Deletes every keychain item.
    pub fn reset_keychain(&self) -> Result<()> {
        rt::require(&self.obj, sel!(resetKeychainWithError:))?;
        // SAFETY: the method exists (checked) and takes NSError**, returning BOOL.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*self.obj, resetKeychainWithError: _] };
        done.map_err(|e| rt::service_error("reset keychain", &e))
    }

    /// Sets the Dynamic Type size (0 = extra small … 6 = XXXL, 7… = accessibility sizes).
    pub fn set_content_size(&self, category: i64) -> Result<()> {
        device_call!(self, "content size", setContentSizeCategory: category as isize)
    }

    /// Turns Increase Contrast on or off.
    pub fn set_increase_contrast(&self, on: bool) -> Result<()> {
        device_call!(self, "increase contrast", setIncreaseContrastEnabled: on)
    }

    /// Hides or shows the Dynamic Island.
    pub fn set_dynamic_island_suppressed(&self, on: bool) -> Result<()> {
        device_call!(self, "dynamic island", setDynamicIslandSuppressed: on)
    }

    /// Turns the display backlight on or off.
    pub fn set_backlight(&self, on: bool) -> Result<()> {
        device_call!(self, "backlight", setDisplayBacklightActive: on)
    }

    /// Sets the software keyboard language (e.g. `en_US`).
    pub fn set_keyboard_language(&self, language: &str) -> Result<()> {
        device_call!(self, "keyboard language", setKeyboardLanguage: &*rt::nsstring(language))
    }

    /// Connects or disconnects the hardware keyboard (when disconnected the software keyboard shows).
    pub fn set_hardware_keyboard(&self, on: bool) -> Result<()> {
        device_call!(self, "hardware keyboard", setHardwareKeyboardEnabled: on, keyboardType: 0u8)
    }

    /// Reads a Core Animation render-server debug option.
    pub fn ca_debug(&self, option: u32) -> bool {
        if !rt::responds(&self.obj, sel!(getCADebugOption:)) {
            return false;
        }
        // SAFETY: `getCADebugOption:` takes an unsigned int and returns BOOL (checked to exist).
        unsafe { msg_send![&*self.obj, getCADebugOption: option] }
    }

    /// Sets a Core Animation render-server debug option (see `sim_core::debug::CA_OPTIONS`).
    pub fn set_ca_debug(&self, option: u32, on: bool) -> Result<()> {
        rt::require(&self.obj, sel!(setCADebugOption:enabled:))?;
        // SAFETY: the method exists (checked) and takes (unsigned int, BOOL), returning void.
        let _: () = unsafe { msg_send![&*self.obj, setCADebugOption: option, enabled: on] };
        Ok(())
    }

    /// Sets a Darwin notification's state value inside the device.
    pub fn set_notification_state(&self, name: &str, state: u64) -> Result<()> {
        device_call!(self, "set notification state", darwinNotificationSetState: state, name: &*rt::nsstring(name))
    }

    /// Spawns `path` inside the device with `args` (argv\[1..]) and `env`, wiring stdout/stderr to
    /// host file descriptors. Returns the pid. The child is not waited for: read its output pipe to EOF.
    pub fn spawn(&self, path: &str, args: &[String], env: &[(String, String)], stdout: Option<i32>, stderr: Option<i32>) -> Result<i32> {
        rt::require(&self.obj, sel!(spawnWithPath:options:terminationHandler:error:))?;
        let options = rt::mutable_dictionary();
        let mut argv = vec![path.to_owned()];
        argv.extend_from_slice(args);
        rt::dict_set(&options, "arguments", &rt::string_array(&argv));
        rt::dict_set(&options, "environment", &rt::string_dictionary(env));
        if let Some(fd) = stdout {
            rt::dict_set(&options, "stdout", &rt::nsnumber(i64::from(fd)));
        }
        if let Some(fd) = stderr {
            rt::dict_set(&options, "stderr", &rt::nsnumber(i64::from(fd)));
        }
        type Spawn = unsafe extern "C" fn(
            *const AnyObject,
            objc2::runtime::Sel,
            *const AnyObject,
            *const AnyObject,
            *const std::ffi::c_void,
            *mut *mut NSError,
        ) -> i32;
        // SAFETY: `objc_msgSend` is called through the method's signature
        // (NSString, NSDictionary, block, NSError**) -> int; a nil termination block is accepted.
        let send: Spawn = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
        let mut error: *mut NSError = ptr::null_mut();
        let path = rt::nsstring(path);
        // SAFETY: all object arguments are live; `error` is a valid out-pointer.
        let pid = unsafe {
            send(
                &*self.obj,
                sel!(spawnWithPath:options:terminationHandler:error:),
                (&*path as &NSString as *const NSString).cast(),
                &*options,
                ptr::null(),
                &mut error,
            )
        };
        if pid > 0 { Ok(pid) } else { Err(rt::raw_error("spawn", error)) }
    }

    /// Runs `path` inside the device and collects its output. Resolves bare names against the
    /// runtime's `/usr/bin`, `/bin` and `/usr/sbin`.
    pub fn exec(&self, path: &str, args: &[String], env: &[(String, String)]) -> Result<ExecOutput> {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        // Spawned programs resolve paths on the host: prefix the runtime root, as the device sees it.
        let root = self.runtime_root()?;
        let inside: Vec<String> = if path.starts_with('/') {
            vec![path.to_owned()]
        } else {
            ["/usr/bin", "/bin", "/usr/sbin", "/sbin", "/usr/libexec"].iter().map(|d| format!("{d}/{path}")).collect()
        };
        let mut candidates: Vec<String> =
            inside.iter().map(|p| format!("{root}{p}")).filter(|p| std::path::Path::new(p).exists()).collect();
        // A simulator binary built on the host (e.g. a test tool) runs from its host path.
        if candidates.is_empty() && path.starts_with('/') && std::path::Path::new(path).is_file() {
            candidates.push(path.to_owned());
        }
        if candidates.is_empty() {
            return Err(Error::NotFound(format!("{path} in the device runtime")));
        }
        let (mut out_r, out_w) = std::io::pipe().map_err(|e| Error::Invalid(format!("pipe: {e}")))?;
        let (mut err_r, err_w) = std::io::pipe().map_err(|e| Error::Invalid(format!("pipe: {e}")))?;
        let mut result = Err(Error::NotFound(path.to_owned()));
        for candidate in &candidates {
            result = self.spawn(candidate, args, env, Some(out_w.as_raw_fd()), Some(err_w.as_raw_fd()));
            if result.is_ok() {
                break;
            }
        }
        drop((out_w, err_w));
        let pid = result?;
        let err_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            err_r.read_to_end(&mut buf).ok();
            buf
        });
        let mut stdout = Vec::new();
        out_r.read_to_end(&mut stdout).map_err(|e| Error::Invalid(format!("read: {e}")))?;
        let stderr = err_thread.join().unwrap_or_default();
        Ok(ExecOutput { pid, stdout: String::from_utf8_lossy(&stdout).into_owned(), stderr: String::from_utf8_lossy(&stderr).into_owned() })
    }

    /// Launches an app with launch arguments and environment variables, terminating a running
    /// instance first. Returns its pid.
    pub fn launch_with(&self, bundle_id: &str, args: &[String], env: &[(String, String)]) -> Result<i32> {
        rt::require(&self.obj, sel!(launchApplicationWithID:options:error:))?;
        let options = rt::mutable_dictionary();
        rt::dict_set(&options, "arguments", &rt::string_array(args));
        rt::dict_set(&options, "environment", &rt::string_dictionary(env));
        rt::dict_set(&options, "terminate_running_process", &rt::nsbool(true));
        let mut error: *mut NSError = ptr::null_mut();
        // SAFETY: the method exists (checked) and takes (NSString, NSDictionary, NSError**), returning int.
        let pid: i32 =
            unsafe { msg_send![&*self.obj, launchApplicationWithID: &*rt::nsstring(bundle_id), options: &*options, error: &mut error] };
        if pid > 0 { Ok(pid) } else { Err(rt::raw_error("launch", error)) }
    }

    /// The device's main display. The device must be booted.
    pub fn display(&self) -> Result<Display> {
        Display::main(&self.obj)
    }

    /// The external display (an iPhone's CarPlay screen once attached), not the main one.
    pub fn external_display(&self) -> Result<Display> {
        Display::external(&self.obj)
    }

    /// A HID input session. The device must be booted. Buttons and form-factor inputs follow the
    /// device's form factor (the Siri Remote on Apple TV, the crown on Apple Watch).
    pub fn hid(&self) -> Result<Hid> {
        Hid::connect(&self.obj, self.kit, self.info().form_factor)
    }

    /// Attaches (or detaches) the CarPlay display to this iPhone and creates (or removes) the
    /// CarPlay HID service its touches go to. iOS 26 runtimes and earlier: iOS 27 renders it black.
    pub fn set_carplay(&self, on: bool) -> Result<()> {
        let info = self.info();
        if info.form_factor != Some(sim_core::form_factor::FormFactor::Phone) {
            return Err(Error::Invalid(format!("CarPlay attaches to an iPhone; {} is {}", info.name, info.label())));
        }
        sim_core::carplay::runtime_supports(&info.runtime_id).map_err(Error::Invalid)?;
        let hid = self.hid()?;
        if on {
            crate::carplay::set_display(&self.obj, &self.data_path(), true)?;
            hid.carplay_service(true)
        } else {
            // The service may already be gone (a reboot removes it); detaching goes on regardless.
            let removed = hid.carplay_service(false);
            crate::carplay::set_display(&self.obj, &self.data_path(), false)?;
            removed
        }
    }

    /// Rotates the device's interface to `orientation`: a GSEvent to SpringBoard's
    /// PurpleWorkspacePort, what Simulator.app's Rotate Left/Right send.
    pub fn set_orientation(&self, orientation: Orientation) -> Result<()> {
        let sel = sel!(lookup:error:);
        rt::require(&self.obj, sel)?;
        let mut error: *mut NSError = std::ptr::null_mut();
        // SAFETY: `lookup:error:` takes (NSString, NSError**) and returns a mach_port_t (0 on failure).
        let port: u32 = unsafe { msg_send![&*self.obj, lookup: &*rt::nsstring("PurpleWorkspacePort"), error: &mut error] };
        if port == 0 {
            return Err(rt::raw_error("look up PurpleWorkspacePort", error));
        }
        let mut message = orientation.purple_message(port);
        // SAFETY: `message` is a complete 108-byte mach message (header included) in a 112-byte
        // buffer; a send-only call with a timeout doesn't touch receive arguments.
        let kr = unsafe { mach_msg(message.as_mut_ptr().cast(), MACH_SEND_MSG | MACH_SEND_TIMEOUT, 108, 0, 0, 2000, 0) };
        // SAFETY: the lookup gave this task a send right to `port`; release it.
        unsafe { mach_port_deallocate(mach_task_self_, port) };
        if kr != 0 {
            return Err(Error::Service { op: "rotate", message: format!("mach_msg to PurpleWorkspacePort failed ({kr:#x})") });
        }
        Ok(())
    }
}
