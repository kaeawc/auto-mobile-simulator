//! Streams: a device's screen as H.264 over WHIP, started, watched and stopped through the method
//! table (`stream_start`, `stream_status`, `stream_stop`, `streams`; docs/streaming.md, "Streams
//! from the API").
//!
//! A stream is a worker thread that runs `simee-encode`'s loop on the device's frames and, on the
//! first access unit (an IDR), connects a `simee-rtc` publisher offering that IDR's own
//! `profile-level-id`, then sends every unit. Keyframe requests (PLI/FIR) force IDRs; bandwidth
//! estimates retarget the encoder; a peer that disconnects ends the stream.
//!
//! Streams outlive the call (and the connection) that started them, so they live in one
//! process-wide registry, each with a lease: `stream_status` renews it, and a stream nobody renews
//! for [`LEASE_TTL`] is stopped, so a client that crashed doesn't leave its stream running.
//! Stopping (by `stream_stop` or a lapsed lease) ends the encode loop and sends the WHIP DELETE.
//! There is one stream per device and destination.
//!
//! With `input: true` the publisher offers the `simee-input` data channel, and its events reach
//! the device's own input methods (`touch`, `key`, `type`, `button`, `rotate`, `fold`, `swipe`)
//! through the stream's [`DisplayMap`], on a thread of their own so the publisher's never waits
//! on a device.
//!
//! The encoder, the publisher, the device input and the clock are traits, so the tests run every
//! method on fakes with no network and no device.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sim_core::device::{DeviceInfo, DeviceState};
use sim_core::platform::Platform;
use sim_encode::EncoderConfig;
use sim_encode::pacing::Pacing;
use sim_encode::stream::{AccessUnit, Controls, Flow};
use sim_rtc::datachannel::{Button, ButtonPhase, InputEvent, KeyPhase, Orientation, Posture, TouchPhase};
use sim_rtc::{DisplayMap, InputConfig, InputSession, InputSink, KeyframeKind, ProfileLevelId, PublisherConfig, PublisherEvent, Sent};

use crate::methods::Session;
use crate::params::{need_str, opt_bool, str_param};

/// The stream methods, in the table's order.
pub(crate) const METHODS: [&str; 4] = ["stream_start", "stream_status", "stream_stop", "streams"];

/// How long a stream runs without a `stream_status` renewing its lease.
pub(crate) const LEASE_TTL: Duration = Duration::from_secs(30);
/// How long `stream_start` waits for the first frame and the WHIP connection: the WHIP request
/// (15 s at most), ICE and DTLS (10 s), and the device's first frame.
const START_TIMEOUT: Duration = Duration::from_secs(40);
/// The idle heartbeat: a still screen is re-encoded this often, so a late viewer gets a picture.
const HEARTBEAT: Duration = Duration::from_millis(500);
/// A wheel event becomes a swipe this long.
const WHEEL_SWIPE_MS: u64 = 100;
/// A button held down and up through the channel is pressed for at least, and at most, this long.
const BUTTON_HOLD_MS: (u64, u64) = (10, 10_000);

pub(crate) fn is_method(method: &str) -> bool {
    METHODS.contains(&method)
}

/// Runs one stream method on the process's streams.
pub(crate) fn call(s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    global().call(method, p, &mut |p, screen| lookup(s, p, screen))
}

// ---------------------------------------------------------------------------------------------
// What the streams run on: the encoder, the publisher, the device's input, the clock.

/// The device a stream shows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Target {
    pub platform: Platform,
    pub udid: String,
    pub name: String,
    pub state: DeviceState,
    /// An Android device's AVD directory (its display socket).
    pub avd_dir: Option<PathBuf>,
    /// The size `touch` and `swipe` take, in pixels (asked for only with `input`).
    pub screen: Option<(u32, u32)>,
}

/// The encode loop on a device's frames.
pub(crate) trait Media: Send + Sync {
    /// Why this host can't stream at all, if it can't.
    fn available(&self) -> Result<(), String>;
    /// Runs the loop for `target` on this thread until `controls` stops it, `sink` says stop, or
    /// the source or the encoder fails.
    fn run(
        &self,
        target: &Target,
        config: &EncoderConfig,
        pacing: Pacing,
        controls: &Controls,
        sink: &mut dyn FnMut(AccessUnit) -> sim_encode::Result<Flow>,
    ) -> Result<(), String>;
}

/// A live WHIP session.
pub(crate) trait Publication: Send {
    fn send(&mut self, access_unit: &[u8], pts: Duration) -> sim_rtc::Result<Sent>;
    /// The WHIP resource (the DELETE target).
    fn resource(&self) -> String;
    /// Stops the media and sends the WHIP DELETE.
    fn close(self: Box<Self>) -> sim_rtc::Result<()>;
}

/// Opens WHIP sessions.
pub(crate) trait Whip: Send + Sync {
    fn connect(
        &self,
        config: PublisherConfig,
        input: Option<InputSession>,
        on_event: Box<dyn FnMut(PublisherEvent) + Send>,
    ) -> sim_rtc::Result<Box<dyn Publication>>;
}

/// A device's input methods, called on the stream's input thread (where it is made).
pub(crate) trait DeviceInput {
    fn call(&mut self, method: &str, params: &Value) -> Result<Value, String>;
}

/// Makes a [`DeviceInput`] on the thread that will use it.
pub(crate) type MakeInput = Arc<dyn Fn() -> Box<dyn DeviceInput> + Send + Sync>;

pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// The method table, on a session of the input thread's own.
struct ApiInput(Session);

impl DeviceInput for ApiInput {
    fn call(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        crate::methods::call(&mut self.0, method, params)
    }
}

/// `simee-rtc`'s publisher.
struct RtcWhip;

impl Whip for RtcWhip {
    fn connect(
        &self,
        config: PublisherConfig,
        input: Option<InputSession>,
        on_event: Box<dyn FnMut(PublisherEvent) + Send>,
    ) -> sim_rtc::Result<Box<dyn Publication>> {
        let publisher = match input {
            Some(input) => sim_rtc::Publisher::connect_with_input(config, input, on_event)?,
            None => sim_rtc::Publisher::connect(config, on_event)?,
        };
        Ok(Box::new(publisher))
    }
}

impl Publication for sim_rtc::Publisher {
    fn send(&mut self, access_unit: &[u8], pts: Duration) -> sim_rtc::Result<Sent> {
        sim_rtc::Publisher::send(self, access_unit, pts)
    }

    fn resource(&self) -> String {
        sim_rtc::Publisher::resource(self).to_string()
    }

    fn close(self: Box<Self>) -> sim_rtc::Result<()> {
        sim_rtc::Publisher::close(*self)
    }
}

/// VideoToolbox on the device's own frames: the iOS framebuffer's IOSurface, or emu's frame ring.
struct VtMedia;

impl Media for VtMedia {
    fn available(&self) -> Result<(), String> {
        if sim_encode::available() {
            Ok(())
        } else {
            Err("unsupported: streaming needs an H.264 encoder, which only macOS hosts have so far (VideoToolbox; a Linux encoder is #263)"
                .into())
        }
    }

    #[cfg(target_os = "macos")]
    fn run(
        &self,
        target: &Target,
        config: &EncoderConfig,
        pacing: Pacing,
        controls: &Controls,
        sink: &mut dyn FnMut(AccessUnit) -> sim_encode::Result<Flow>,
    ) -> Result<(), String> {
        let text = |e: sim_encode::Error| e.to_string();
        let mut encoder = sim_encode::VtEncoder::new(config.clone()).map_err(text)?;
        match target.platform {
            Platform::Ios => {
                // The context, the display and the source stay on this thread.
                let context = sim_coresim::Context::new().map_err(|e| e.to_string())?;
                let display = context.device(&target.udid).and_then(|d| d.display()).map_err(|e| e.to_string())?;
                let mut source = sim_encode::IosSource::new(display).map_err(text)?;
                sim_encode::stream::run(&mut source, &mut encoder, pacing, controls, sink).map(drop).map_err(text)
            }
            Platform::Android => {
                let dir = target.avd_dir.as_ref().ok_or_else(|| format!("{}: no AVD directory", target.name))?;
                let mut source = sim_encode::AndroidSource::connect(dir, 0).map_err(text)?;
                sim_encode::stream::run(&mut source, &mut encoder, pacing, controls, sink).map(drop).map_err(text)
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn run(
        &self,
        _: &Target,
        _: &EncoderConfig,
        _: Pacing,
        _: &Controls,
        _: &mut dyn FnMut(AccessUnit) -> sim_encode::Result<Flow>,
    ) -> Result<(), String> {
        self.available()
    }
}

/// The device behind the `device` parameter, through `info` (and `screen`, with `screen`).
fn lookup(s: &mut Session, p: &Value, screen: bool) -> Result<Target, String> {
    let mut query = json!({"device": str_param(p, "device").unwrap_or("booted")});
    if let Some(platform) = p.get("platform") {
        query["platform"] = platform.clone();
    }
    let info = crate::methods::call(s, "info", &query)?;
    let device: DeviceInfo = serde_json::from_value(info["device"].clone()).map_err(|e| format!("info: {e}"))?;
    let avd_dir = info.get("avd_dir").and_then(Value::as_str).map(PathBuf::from);
    let screen = match screen && device.state == DeviceState::Booted {
        true => {
            let size = crate::methods::call(s, "screen", &json!({"device": device.udid, "platform": device.platform.as_str()}))?;
            let side = |k: &str| size[k].as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| format!("screen has no {k}: {size}"));
            Some((side("width")?, side("height")?))
        }
        false => None,
    };
    Ok(Target { platform: device.platform, udid: device.udid, name: device.name, state: device.state, avd_dir, screen })
}

static GLOBAL: OnceLock<Arc<Streams>> = OnceLock::new();

/// The process's streams, on VideoToolbox, `simee-rtc` and the method table, with the thread that
/// stops streams whose lease lapsed.
fn global() -> &'static Arc<Streams> {
    GLOBAL.get_or_init(|| {
        let make: MakeInput = Arc::new(|| Box::new(ApiInput(Session::default())));
        let streams = Arc::new(Streams::new(Arc::new(SystemClock), Arc::new(VtMedia), Arc::new(RtcWhip), make));
        let reaper = streams.clone();
        // Without it a lapsed lease is still seen (and its stream stopped) by the next stream call.
        let _ = std::thread::Builder::new().name("simee-stream-leases".into()).spawn(move || reaper.reap_forever());
        streams
    })
}

// ---------------------------------------------------------------------------------------------
// Parameters.

/// What `stream_start` asks for. Its `Debug` never prints the token.
#[derive(Clone, PartialEq)]
pub(crate) struct Options {
    pub whip_url: String,
    pub token: Option<String>,
    pub settings: Settings,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("whip_url", &self.whip_url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("settings", &self.settings)
            .finish()
    }
}

/// The encoding and input settings, with their defaults (what the schema advertises).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub fps: u32,
    /// The longest side of the stream; `None` (`max_size: 0`) is full size.
    pub max_size: Option<u32>,
    pub bitrate: u32,
    pub input: bool,
}

/// An optional whole-number parameter, refused when it is something else.
fn opt_int(p: &Value, key: &str) -> Result<Option<u64>, String> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| format!("`{key}` must be a whole number, not {v}")),
    }
}

/// `stream_start`'s settings: fps (30), max_size (1280; 0 is full size), bitrate (2.5 Mbit/s),
/// input (false); audio must be false for now.
pub(crate) fn settings(p: &Value) -> Result<Settings, String> {
    if opt_bool(p, "audio")? == Some(true) {
        return Err("`audio` isn't streamed yet (an Opus track is #265): leave it out or pass false".into());
    }
    let fps = opt_int(p, "fps")?.unwrap_or(30);
    let max_size = opt_int(p, "max_size")?.unwrap_or(1280);
    let bitrate = opt_int(p, "bitrate")?.unwrap_or(2_500_000);
    let narrow = |key: &str, n: u64| u32::try_from(n).map_err(|_| format!("`{key}` {n} is too large"));
    let settings = Settings {
        fps: narrow("fps", fps)?,
        max_size: Some(narrow("max_size", max_size)?).filter(|&m| m > 0),
        bitrate: narrow("bitrate", bitrate)?,
        input: opt_bool(p, "input")?.unwrap_or(false),
    };
    encoder_config(&settings).validate().map_err(|e| e.to_string())?;
    Ok(settings)
}

/// `stream_start`'s parameters; the URL is checked as the publisher would check it (a scheme it
/// speaks, plaintext only to loopback), before anything starts.
pub(crate) fn options(p: &Value) -> Result<Options, String> {
    let settings = settings(p)?;
    let whip_url = need_str(p, "whip_url")?.to_owned();
    let token = str_param(p, "token").filter(|t| !t.is_empty()).map(str::to_owned);
    let whip = sim_rtc::WhipOptions { bearer_token: token.clone(), ..sim_rtc::WhipOptions::default() };
    let client = sim_rtc::WhipClient::new(&whip_url, whip).map_err(|e| format!("bad whip_url `{whip_url}`: {e}"))?;
    // One stream per destination: the URL as the client reads it.
    Ok(Options { whip_url: client.endpoint().to_string(), token, settings })
}

fn encoder_config(s: &Settings) -> EncoderConfig {
    let default = EncoderConfig::default();
    EncoderConfig {
        fps: s.fps,
        max_size: s.max_size,
        bitrate_bps: s.bitrate,
        max_bitrate_bps: default.max_bitrate_bps.map(|m| m.max(s.bitrate)),
        ..default
    }
}

/// What a failed WHIP connection says, with the token's refusal called out.
fn whip_error(url: &str, e: &sim_rtc::Error) -> String {
    match e {
        sim_rtc::Error::Status { status: status @ (401 | 403), method, .. } => {
            format!("WHIP {url} refused the credentials ({method} answered {status}): pass a valid `token`")
        }
        other => format!("WHIP {url}: {other}"),
    }
}

// ---------------------------------------------------------------------------------------------
// One stream's progress, shared by its worker, its publisher's events and the methods.

#[derive(Default)]
struct Progress {
    /// Each stage once, in the order reached.
    stages: Vec<(&'static str, Instant)>,
    encoded: u64,
    heartbeats: u64,
    keyframes: u64,
    bytes: u64,
    sent: u64,
    pli: u64,
    fir: u64,
    /// The publisher's latest bandwidth estimate.
    estimate: Option<u64>,
    input_events: u64,
    input_errors: u64,
    last_input_error: Option<String>,
    resource: Option<String>,
    /// Why it is ending or ended: the first reason given wins.
    reason: Option<String>,
    ended: bool,
    /// The WHIP DELETE's outcome, once sent.
    deleted: Option<Result<(), String>>,
}

struct Shared {
    progress: Mutex<Progress>,
    controls: Controls,
    clock: Arc<dyn Clock>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Progress> {
        self.progress.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn stage(&self, name: &'static str) {
        let now = self.clock.now();
        let mut p = self.lock();
        if !p.stages.iter().any(|(s, _)| *s == name) {
            p.stages.push((name, now));
        }
    }

    /// Ends the stream for `reason` (unless another reason came first).
    fn stop(&self, reason: impl Into<String>) {
        self.lock().reason.get_or_insert_with(|| reason.into());
        self.controls.stop();
    }

    fn encoded(&self, unit: &AccessUnit) {
        self.stage("first_encoded_frame");
        let mut p = self.lock();
        p.encoded += 1;
        p.heartbeats += u64::from(unit.heartbeat);
        p.keyframes += u64::from(unit.keyframe);
        p.bytes += unit.data.len() as u64;
    }

    fn on_event(&self, event: PublisherEvent) {
        match event {
            PublisherEvent::KeyframeRequested(kind) => {
                let mut p = self.lock();
                match kind {
                    KeyframeKind::Pli => p.pli += 1,
                    KeyframeKind::Fir => p.fir += 1,
                }
                drop(p);
                self.controls.request_keyframe();
            }
            PublisherEvent::BitrateEstimate { bps, .. } => {
                self.lock().estimate = Some(bps);
                self.controls.set_bitrate(bps);
            }
            PublisherEvent::Disconnected => self.stop("the WHIP peer disconnected"),
            PublisherEvent::Connected => {}
        }
    }

    fn input_done(&self, result: &Result<Value, String>) {
        let mut p = self.lock();
        p.input_events += 1;
        if let Err(e) = result {
            p.input_errors += 1;
            p.last_input_error = Some(e.clone());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The registry.

struct Entry {
    id: String,
    target: Target,
    whip_url: String,
    input: bool,
    requested: Instant,
    lease_until: Instant,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

/// Every stream of this process.
pub(crate) struct Streams {
    clock: Arc<dyn Clock>,
    media: Arc<dyn Media>,
    whip: Arc<dyn Whip>,
    make_input: MakeInput,
    ttl: Duration,
    start_timeout: Duration,
    entries: Mutex<HashMap<String, Entry>>,
    /// Wakes the lease thread when a stream is added.
    added: Condvar,
    next: AtomicU64,
}

/// Looks up the device a call names; `true` asks for its screen size too.
pub(crate) type Lookup<'a> = dyn FnMut(&Value, bool) -> Result<Target, String> + 'a;

impl Streams {
    pub(crate) fn new(clock: Arc<dyn Clock>, media: Arc<dyn Media>, whip: Arc<dyn Whip>, make_input: MakeInput) -> Streams {
        Streams {
            clock,
            media,
            whip,
            make_input,
            ttl: LEASE_TTL,
            start_timeout: START_TIMEOUT,
            entries: Mutex::default(),
            added: Condvar::new(),
            next: AtomicU64::new(1),
        }
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn call(&self, method: &str, p: &Value, lookup: &mut Lookup<'_>) -> Result<Value, String> {
        self.reap();
        match method {
            "stream_start" => self.start(p, lookup),
            "stream_status" => self.status(need_str(p, "stream_id")?),
            "stream_stop" => self.stop(need_str(p, "stream_id")?, "stopped by stream_stop"),
            "streams" => {
                let udid = match p.get("device") {
                    None | Some(Value::Null) => None,
                    Some(_) => Some(lookup(p, false)?.udid),
                };
                let now = self.clock.now();
                let entries = self.entries();
                let mut list: Vec<&Entry> = entries.values().filter(|e| udid.as_ref().is_none_or(|u| *u == e.target.udid)).collect();
                list.sort_by_key(|e| e.requested);
                Ok(list.into_iter().map(|e| self.describe(e, now)).collect())
            }
            other => Err(format!("unknown stream method `{other}`")),
        }
    }

    fn start(&self, p: &Value, lookup: &mut Lookup<'_>) -> Result<Value, String> {
        let options = options(p)?;
        self.media.available()?;
        let target = lookup(p, options.settings.input)?;
        if target.state != DeviceState::Booted {
            return Err(format!("{} isn't booted ({:?}): boot it before streaming it", target.name, target.state));
        }
        let screen = match (options.settings.input, target.screen) {
            (true, None) => return Err(format!("{}: no screen size to map input onto", target.name)),
            (_, screen) => screen,
        };
        let now = self.clock.now();
        let shared = Arc::new(Shared { progress: Mutex::default(), controls: Controls::default(), clock: self.clock.clone() });
        shared.stage("requested");
        let id = {
            let mut entries = self.entries();
            let same = |e: &Entry| e.target.udid == target.udid && e.whip_url == options.whip_url;
            if let Some(e) = entries.values().find(|e| same(e) && !e.shared.lock().ended) {
                return Err(format!(
                    "{} is already streaming to {} as {}: stop it (stream_stop) or stream to another URL",
                    target.name, options.whip_url, e.id
                ));
            }
            // An ended stream to the same place is replaced (its worker is done).
            entries.retain(|_, e| !same(e));
            let id = format!("stream-{}", self.next.fetch_add(1, Ordering::Relaxed));
            entries.insert(
                id.clone(),
                Entry {
                    id: id.clone(),
                    target: target.clone(),
                    whip_url: options.whip_url.clone(),
                    input: options.settings.input,
                    requested: now,
                    // Not lapsing while it starts: the start renews it.
                    lease_until: now + self.start_timeout + self.ttl,
                    shared: shared.clone(),
                    worker: None,
                },
            );
            self.added.notify_all();
            id
        };
        let (ready_tx, ready_rx) = sync_channel(1);
        let worker = Worker {
            target: target.clone(),
            options: options.clone(),
            screen,
            shared: shared.clone(),
            media: self.media.clone(),
            whip: self.whip.clone(),
            make_input: self.make_input.clone(),
        };
        let spawned = std::thread::Builder::new().name("simee-stream".into()).spawn(move || worker.run(ready_tx));
        match spawned {
            Ok(handle) => match self.entries().get_mut(&id) {
                Some(e) => e.worker = Some(handle),
                // Stopped already (by a lapsed lease or a stop that raced the start).
                None => drop(handle.join()),
            },
            Err(e) => {
                self.entries().remove(&id);
                return Err(format!("stream thread: {e}"));
            }
        }
        let outcome = ready_rx
            .recv_timeout(self.start_timeout)
            .unwrap_or_else(|_| Err(format!("no first frame and WHIP connection within {} s", self.start_timeout.as_secs())));
        match outcome {
            Ok(resource) => {
                if let Some(e) = self.entries().get_mut(&id) {
                    e.lease_until = self.clock.now() + self.ttl;
                }
                Ok(json!({
                "stream_id": id,
                "whip_resource": resource,
                "lease_ttl_s": self.ttl.as_secs(),
                "device": target.udid,
                }))
            }
            Err(e) => {
                shared.stop(e.clone());
                let entry = self.entries().remove(&id);
                if let Some(handle) = entry.and_then(|mut e| e.worker.take()) {
                    let _ = handle.join();
                }
                Err(e)
            }
        }
    }

    /// The stream's status; renews its lease unless it has ended.
    fn status(&self, id: &str) -> Result<Value, String> {
        let now = self.clock.now();
        let mut entries = self.entries();
        let e = entries.get_mut(id).ok_or_else(|| no_stream(id))?;
        if !e.shared.lock().ended {
            e.lease_until = now + self.ttl;
        }
        Ok(self.describe(e, now))
    }

    /// Stops a stream (its encode loop, then the WHIP DELETE) and forgets it: its last status, and
    /// whether the DELETE went through.
    fn stop(&self, id: &str, reason: &str) -> Result<Value, String> {
        let mut entry = self.entries().remove(id).ok_or_else(|| no_stream(id))?;
        entry.shared.stop(reason);
        if let Some(handle) = entry.worker.take() {
            let _ = handle.join();
        }
        let mut status = self.describe(&entry, self.clock.now());
        let deleted = entry.shared.lock().deleted.clone();
        status["deleted"] = json!(matches!(deleted, Some(Ok(()))));
        if let Some(Err(e)) = deleted {
            status["delete_error"] = json!(e);
        }
        Ok(status)
    }

    /// Stops every stream whose lease lapsed; ended streams are forgotten then too.
    pub(crate) fn reap(&self) {
        let now = self.clock.now();
        let expired: Vec<String> = self.entries().values().filter(|e| e.lease_until <= now).map(|e| e.id.clone()).collect();
        for id in expired {
            let _ = self.stop(&id, &format!("the lease lapsed: no stream_status for {} s", self.ttl.as_secs()));
        }
    }

    /// The lease thread: sleeps until the next lease could lapse, then reaps.
    fn reap_forever(&self) {
        let mut entries = self.entries();
        loop {
            let next = entries.values().map(|e| e.lease_until).min();
            entries = match next {
                None => self.added.wait(entries).unwrap_or_else(|p| p.into_inner()),
                Some(at) => {
                    let wait = at.saturating_duration_since(self.clock.now());
                    if wait.is_zero() {
                        drop(entries);
                        self.reap();
                        self.entries()
                    } else {
                        self.added.wait_timeout(entries, wait).map_or_else(|p| p.into_inner().0, |(g, _)| g)
                    }
                }
            };
        }
    }

    fn describe(&self, e: &Entry, now: Instant) -> Value {
        let p = e.shared.lock();
        let at = |name: &str| p.stages.iter().find(|(s, _)| *s == name).map(|(_, t)| *t);
        let ms = |t: Instant| (t.saturating_duration_since(e.requested).as_secs_f64() * 10_000.0).round() / 10.0;
        let stages: Vec<Value> = p.stages.iter().map(|(name, t)| json!({"stage": name, "ms": ms(*t)})).collect();
        // Rates over the time frames have been encoded, up to the end once it ended.
        let until = at("ended").unwrap_or(now);
        let span = at("first_encoded_frame").map(|f| until.saturating_duration_since(f).as_secs_f64()).filter(|s| *s > 0.0);
        let rate = |n: f64| span.map(|s| (n / s * 10.0).round() / 10.0);
        let state = match (p.ended, &p.resource) {
            (true, _) => "ended",
            (false, Some(_)) => "running",
            (false, None) => "starting",
        };
        let mut v = json!({
            "stream_id": e.id,
            "device": e.target.udid,
            "name": e.target.name,
            "platform": e.target.platform.as_str(),
            "whip_url": e.whip_url,
            "whip_resource": p.resource,
            "state": state,
            "input": e.input,
            "lease_ttl_s": self.ttl.as_secs(),
            "lease_expires_in_s": (e.lease_until.saturating_duration_since(now).as_secs_f64() * 10.0).round() / 10.0,
            "stages": stages,
            "stats": {
                "encoded": p.encoded,
                "sent": p.sent,
                "heartbeats": p.heartbeats,
                "keyframes": p.keyframes,
                "bytes": p.bytes,
                "fps": rate(p.encoded as f64),
                "bitrate_bps": rate(p.bytes as f64 * 8.0),
                "bitrate_estimate_bps": p.estimate,
                "pli": p.pli,
                "fir": p.fir,
                "input_events": p.input_events,
                "input_errors": p.input_errors,
            },
        });
        if let Some(reason) = &p.reason {
            v["reason"] = json!(reason);
        }
        if let Some(e) = &p.last_input_error {
            v["stats"]["last_input_error"] = json!(e);
        }
        v
    }
}

fn no_stream(id: &str) -> String {
    format!("no stream `{id}` (it was stopped, or its lease lapsed); `streams` lists the running ones")
}

// ---------------------------------------------------------------------------------------------
// The worker: the encode loop, the publisher, and the input thread.

struct Worker {
    target: Target,
    options: Options,
    screen: Option<(u32, u32)>,
    shared: Arc<Shared>,
    media: Arc<dyn Media>,
    whip: Arc<dyn Whip>,
    make_input: MakeInput,
}

impl Worker {
    /// Runs the stream to its end, telling `ready` the WHIP resource once connected (or why not).
    fn run(self, ready: SyncSender<Result<String, String>>) {
        let shared = self.shared.clone();
        let config = encoder_config(&self.options.settings);
        let (mut input, input_thread) = match (self.options.settings.input, self.screen) {
            (true, Some((w, h))) => {
                let map = Arc::new(Mutex::new(DisplayMap::new(w, h)));
                let (tx, rx) = channel();
                let thread = spawn_input(rx, self.make_input.clone(), map.clone(), shared.clone(), &self.target);
                let sink = DeviceSink::new(self.target.platform, map, tx, shared.clock.clone());
                (Some(InputSession::new(sink, InputConfig::default())), thread)
            }
            _ => (None, None),
        };
        let mut ready = Some(ready);
        let mut publication: Option<Box<dyn Publication>> = None;
        let url = self.options.whip_url.clone();
        let result = self.media.run(&self.target, &config, Pacing::new(config.fps, Some(HEARTBEAT)), &shared.controls, &mut |unit| {
            shared.encoded(&unit);
            if publication.is_none() {
                let p = match connect(&self.whip, &self.options, &config, &unit, input.take(), &shared) {
                    Ok(p) => p,
                    Err(e) => {
                        shared.lock().reason.get_or_insert(e.clone());
                        send_ready(&mut ready, Err(e.clone()));
                        return Err(sim_encode::Error::Encoder(e));
                    }
                };
                shared.lock().resource = Some(p.resource());
                shared.stage("whip_connected");
                send_ready(&mut ready, Ok(p.resource()));
                publication = Some(p);
            }
            let Some(p) = publication.as_mut() else { return Ok(Flow::Stop) };
            match p.send(&unit.data, unit.pts).map_err(|e| sim_encode::Error::Encoder(format!("WHIP {url}: {e}")))? {
                Sent::Queued { .. } => shared.lock().sent += 1,
                Sent::WaitingForKeyframe => {}
            }
            Ok(Flow::Continue)
        });
        let ended = match result {
            Ok(()) => "the encode loop ended".to_owned(),
            Err(e) => e.strip_prefix("encoder: ").unwrap_or(&e).to_owned(),
        };
        shared.lock().reason.get_or_insert(ended.clone());
        send_ready(&mut ready, Err(ended));
        if let Some(p) = publication {
            let deleted = p.close().map_err(|e| whip_error(&self.options.whip_url, &e));
            shared.lock().deleted = Some(deleted);
        }
        // Its release (anything still down) reaches the device before the input thread ends.
        drop(input);
        if let Some(thread) = input_thread {
            let _ = thread.join();
        }
        shared.stage("ended");
        shared.lock().ended = true;
    }
}

fn send_ready(ready: &mut Option<SyncSender<Result<String, String>>>, outcome: Result<String, String>) {
    if let Some(r) = ready.take() {
        let _ = r.try_send(outcome);
    }
}

/// Connects the publisher, offering the first IDR's own `profile-level-id` (its level follows the
/// frame size: a phone-sized picture is level 5.x, which the default `42e01f` offer would refuse).
fn connect(
    whip: &Arc<dyn Whip>,
    options: &Options,
    config: &EncoderConfig,
    first: &AccessUnit,
    input: Option<InputSession>,
    shared: &Arc<Shared>,
) -> Result<Box<dyn Publication>, String> {
    let profile = sim_rtc::h264::nal_units(&first.data)
        .into_iter()
        .find_map(ProfileLevelId::from_sps)
        .ok_or("the encoder's first access unit has no SPS")?;
    let mut rtc = PublisherConfig::new(options.whip_url.clone());
    rtc.whip.bearer_token = options.token.clone();
    rtc.profile = profile;
    rtc.start_bitrate_bps = Some(u64::from(config.bitrate_bps));
    rtc.max_bitrate_bps = u64::from(config.max_bitrate_bps.unwrap_or(config.bitrate_bps));
    let events = shared.clone();
    whip.connect(rtc, input, Box::new(move |e| events.on_event(e))).map_err(|e| whip_error(&options.whip_url, &e))
}

// ---------------------------------------------------------------------------------------------
// Input: the data channel's events, mapped onto the device and called on a thread of their own.

/// One call into the device's input methods.
type Call = (&'static str, Value);

/// Calls the device for each event the sink queued; after a rotation or a fold, reads the screen
/// size again so later points map onto the new shape.
fn spawn_input(
    rx: Receiver<Call>,
    make: MakeInput,
    map: Arc<Mutex<DisplayMap>>,
    shared: Arc<Shared>,
    target: &Target,
) -> Option<JoinHandle<()>> {
    let device = json!({"device": target.udid, "platform": target.platform.as_str()});
    std::thread::Builder::new()
        .name("simee-stream-input".into())
        .spawn(move || {
            let mut input = make();
            for (method, mut params) in rx {
                params["device"] = device["device"].clone();
                params["platform"] = device["platform"].clone();
                let result = input.call(method, &params);
                shared.input_done(&result);
                if result.is_ok() && matches!(method, "rotate" | "fold") {
                    let size = input.call("screen", &device).ok();
                    let side = |k: &str| size.as_ref().and_then(|s| s[k].as_u64()).and_then(|n| u32::try_from(n).ok());
                    if let (Some(w), Some(h)) = (side("width"), side("height")) {
                        *map.lock().unwrap_or_else(|p| p.into_inner()) = DisplayMap::new(w, h);
                    }
                }
            }
        })
        .ok()
}

/// The stream's [`InputSink`]: maps each event onto the device (pixels through the [`DisplayMap`])
/// and queues the call, so the publisher's thread never waits on the device. One touch at a time:
/// the device methods are single-pointer.
struct DeviceSink {
    platform: Platform,
    map: Arc<Mutex<DisplayMap>>,
    queue: Sender<Call>,
    clock: Arc<dyn Clock>,
    /// The pointer that is down on the device, and where it last was.
    pointer: Option<(u32, (f64, f64))>,
    /// Buttons held through the channel, since when.
    held: HashMap<Button, Instant>,
}

impl DeviceSink {
    fn new(platform: Platform, map: Arc<Mutex<DisplayMap>>, queue: Sender<Call>, clock: Arc<dyn Clock>) -> DeviceSink {
        DeviceSink { platform, map, queue, clock, pointer: None, held: HashMap::new() }
    }

    fn map(&self) -> DisplayMap {
        *self.map.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The device call for `event`, if it makes one; an `Err` is the viewer's `sink` error.
    fn translate(&mut self, event: &InputEvent) -> Result<Option<Call>, String> {
        Ok(Some(match event {
            InputEvent::Touch { pointer, phase, point } => {
                match self.pointer {
                    Some((down, _)) if down != *pointer => {
                        return Err(format!(
                            "pointer {pointer}: one touch at a time (pointer {down} is down; multi-touch isn't wired yet)"
                        ));
                    }
                    None if *phase != TouchPhase::Down => return Err(format!("pointer {pointer} isn't down on the device")),
                    _ => {}
                }
                let lift = matches!(phase, TouchPhase::Up | TouchPhase::Cancel);
                let (x, y) = match (self.map().to_device(point), self.pointer) {
                    (Ok(at), _) => at,
                    // A lift from a frame of another shape lifts where the finger was: never stuck.
                    (Err(_), Some((_, last))) if lift => last,
                    (Err(e), _) => return Err(e.to_string()),
                };
                self.pointer = if lift { None } else { Some((*pointer, (x, y))) };
                let phase = match phase {
                    TouchPhase::Down => "down",
                    TouchPhase::Move => "move",
                    TouchPhase::Up | TouchPhase::Cancel => "up",
                };
                ("touch", json!({"x": x, "y": y, "phase": phase}))
            }
            // `key` presses and releases; the release is the down's.
            InputEvent::Key { phase: KeyPhase::Down, usage } => ("key", json!({"usage": usage})),
            InputEvent::Key { phase: KeyPhase::Up, .. } => return Ok(None),
            InputEvent::Text(text) => ("type", json!({"text": text})),
            InputEvent::Button { button, phase } => {
                let name = button_name(self.platform, *button)?;
                match phase {
                    ButtonPhase::Press => ("button", json!({"button": name})),
                    ButtonPhase::Down => {
                        self.held.insert(*button, self.clock.now());
                        return Ok(None);
                    }
                    // Pressed on release, held as long as the viewer held it.
                    ButtonPhase::Up => {
                        let Some(since) = self.held.remove(button) else { return Ok(None) };
                        let ms = self.clock.now().saturating_duration_since(since).as_millis() as u64;
                        ("button", json!({"button": name, "hold_ms": ms.clamp(BUTTON_HOLD_MS.0, BUTTON_HOLD_MS.1)}))
                    }
                }
            }
            InputEvent::Rotate(orientation) => match self.platform {
                Platform::Ios => ("rotate", json!({"orientation": orientation_name(*orientation)})),
                Platform::Android => return Err("Android rotates 90° clockwise per `rotate`: a target orientation isn't wired yet".into()),
            },
            InputEvent::Fold(posture) => ("fold", json!({"posture": posture_name(*posture)})),
            // A scroll is a short swipe the other way: content follows the finger.
            InputEvent::Wheel { point, dx, dy } => {
                let map = self.map();
                let (x, y) = map.to_device(point).map_err(|e| e.to_string())?;
                let (ddx, ddy) = map.delta_to_device(*dx, *dy, point.frame);
                let clamp = |v: f64, side: u32| v.clamp(0.0, (f64::from(side) - 1.0).max(0.0));
                let (x2, y2) = (clamp(x - ddx, map.device.width), clamp(y - ddy, map.device.height));
                ("swipe", json!({"x1": x, "y1": y, "x2": x2, "y2": y2, "duration_ms": WHEEL_SWIPE_MS}))
            }
        }))
    }
}

impl InputSink for DeviceSink {
    fn input(&mut self, event: &InputEvent) -> Result<(), String> {
        if let Some(call) = self.translate(event)? {
            self.queue.send(call).map_err(|_| "the device input has stopped".to_owned())?;
        }
        Ok(())
    }
}

fn button_name(platform: Platform, button: Button) -> Result<&'static str, String> {
    Ok(match (button, platform) {
        (Button::Home, _) => "home",
        (Button::Power, _) => "lock",
        (Button::VolumeUp, _) => "volume-up",
        (Button::VolumeDown, _) => "volume-down",
        (Button::Back, _) => "back",
        (Button::AppSwitch, _) => "recents",
        (Button::Menu, _) => "menu",
        (Button::Assistant, Platform::Ios) => "siri",
        (Button::Assistant, Platform::Android) => return Err("the assistant button isn't wired on Android devices".into()),
    })
}

fn orientation_name(o: Orientation) -> &'static str {
    match o {
        Orientation::Portrait => "portrait",
        Orientation::LandscapeLeft => "landscape-left",
        Orientation::LandscapeRight => "landscape-right",
        Orientation::PortraitUpsideDown => "portrait-upside-down",
    }
}

fn posture_name(p: Posture) -> &'static str {
    match p {
        Posture::Closed => "closed",
        Posture::HalfOpen => "half-opened",
        Posture::Open => "opened",
        Posture::Flipped => "flipped",
        Posture::Tent => "tent",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_rtc::datachannel::{ErrorCode, FrameSize, Point, Reply};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    struct FakeClock(Mutex<Instant>);

    impl FakeClock {
        fn advance(&self, by: Duration) {
            *self.0.lock().expect("clock") += by;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *self.0.lock().expect("clock")
        }
    }

    /// A frame every 5 ms, in real time.
    struct FakeSource {
        next: Instant,
        newest: Option<u64>,
    }

    impl sim_encode::Source for FakeSource {
        type Picture = u64;

        fn wait(&mut self, deadline: Instant) -> sim_encode::Result<Option<Instant>> {
            let at = self.next.min(deadline);
            std::thread::sleep(at.saturating_duration_since(Instant::now()));
            if self.next > deadline {
                return Ok(None);
            }
            self.newest = Some(self.newest.map_or(0, |n| n + 1));
            self.next += ms(5);
            Ok(Some(at))
        }

        fn picture(&mut self) -> sim_encode::Result<Option<&u64>> {
            Ok(self.newest.as_ref())
        }
    }

    /// An IDR (SPS `42e034`, PPS, slice) when asked for one, else a P slice.
    const IDR: &[u8] = &[0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x34, 0x8c, 0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, 0, 0, 0, 1, 0x65, 0x88, 0x84];
    const P_SLICE: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02];

    struct FakeEncoder;

    impl sim_encode::Encoder<u64> for FakeEncoder {
        fn encode(&mut self, _: &u64, pts: Duration, keyframe: bool) -> sim_encode::Result<Vec<AccessUnit>> {
            let data = if keyframe { IDR } else { P_SLICE }.to_vec();
            Ok(vec![AccessUnit { data, pts, keyframe, heartbeat: false, encode_time: Duration::ZERO }])
        }

        fn set_bitrate(&mut self, _: u64) -> sim_encode::Result<()> {
            Ok(())
        }
    }

    /// `simee-encode`'s own loop on the fake source and encoder.
    #[derive(Default)]
    struct FakeMedia {
        unavailable: bool,
        configs: Mutex<Vec<(String, EncoderConfig)>>,
    }

    impl Media for FakeMedia {
        fn available(&self) -> Result<(), String> {
            if self.unavailable { Err("unsupported: no encoder on this host".into()) } else { Ok(()) }
        }

        fn run(
            &self,
            target: &Target,
            config: &EncoderConfig,
            pacing: Pacing,
            controls: &Controls,
            sink: &mut dyn FnMut(AccessUnit) -> sim_encode::Result<Flow>,
        ) -> Result<(), String> {
            self.configs.lock().expect("configs").push((target.udid.clone(), config.clone()));
            let mut source = FakeSource { next: Instant::now(), newest: None };
            sim_encode::stream::run(&mut source, &mut FakeEncoder, pacing, controls, sink).map(drop).map_err(|e| e.to_string())
        }
    }

    type Events = Arc<Mutex<Option<Box<dyn FnMut(PublisherEvent) + Send>>>>;

    /// What a test can reach of one fake WHIP session.
    #[derive(Clone)]
    struct FakeSession {
        profile: ProfileLevelId,
        token: Option<String>,
        input: Arc<Mutex<Option<InputSession>>>,
        events: Events,
    }

    impl FakeSession {
        fn fire(&self, event: PublisherEvent) {
            (self.events.lock().expect("events").as_mut().expect("connected"))(event);
        }

        /// Feeds one viewer message to the channel's session, as the publisher's thread would.
        fn viewer(&self, at: Instant, message: &str) -> Vec<Reply> {
            let mut replies = Vec::new();
            let mut input = self.input.lock().expect("input");
            input.as_mut().expect("an input channel").receive(at, false, message.as_bytes(), &mut replies);
            replies
        }
    }

    /// Answers every POST with a resource (or refuses it with `refuse`), with no network; logs
    /// each POST and DELETE.
    #[derive(Default)]
    struct FakeWhip {
        refuse: Option<u16>,
        clock: Option<Arc<FakeClock>>,
        log: Arc<Mutex<Vec<String>>>,
        sessions: Mutex<Vec<FakeSession>>,
    }

    impl FakeWhip {
        fn session(&self, n: usize) -> FakeSession {
            self.sessions.lock().expect("sessions")[n].clone()
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().expect("log").clone()
        }
    }

    impl Whip for FakeWhip {
        fn connect(
            &self,
            config: PublisherConfig,
            input: Option<InputSession>,
            on_event: Box<dyn FnMut(PublisherEvent) + Send>,
        ) -> sim_rtc::Result<Box<dyn Publication>> {
            self.log.lock().expect("log").push(format!("POST {}", config.endpoint));
            if let Some(status) = self.refuse {
                return Err(sim_rtc::Error::Status { status, method: "POST", body: "unauthorized".into() });
            }
            // ICE and DTLS take a while.
            if let Some(clock) = &self.clock {
                clock.advance(ms(50));
            }
            let mut sessions = self.sessions.lock().expect("sessions");
            let resource = format!("{}/session-{}", config.endpoint, sessions.len());
            let session = FakeSession {
                profile: config.profile,
                token: config.whip.bearer_token.clone(),
                input: Arc::new(Mutex::new(input)),
                events: Arc::new(Mutex::new(Some(on_event))),
            };
            sessions.push(session.clone());
            Ok(Box::new(FakePublication { resource, log: self.log.clone(), session }))
        }
    }

    struct FakePublication {
        resource: String,
        log: Arc<Mutex<Vec<String>>>,
        session: FakeSession,
    }

    impl Publication for FakePublication {
        fn send(&mut self, access_unit: &[u8], _: Duration) -> sim_rtc::Result<Sent> {
            Ok(Sent::Queued { packets: 1, keyframe: access_unit.get(4) == Some(&0x67) })
        }

        fn resource(&self) -> String {
            self.resource.clone()
        }

        fn close(self: Box<Self>) -> sim_rtc::Result<()> {
            // As the publisher's thread ends: the channel's session goes, lifting what is down.
            self.session.events.lock().expect("events").take();
            let input = self.session.input.lock().expect("input").take();
            drop(input);
            self.log.lock().expect("log").push(format!("DELETE {}", self.resource));
            Ok(())
        }
    }

    /// Records every call; `screen` answers the size the test sets.
    struct FakeInput {
        calls: Sender<(String, Value)>,
        screen: Arc<Mutex<(u32, u32)>>,
    }

    impl DeviceInput for FakeInput {
        fn call(&mut self, method: &str, params: &Value) -> Result<Value, String> {
            let _ = self.calls.send((method.to_owned(), params.clone()));
            if method == "screen" {
                let (w, h) = *self.screen.lock().expect("screen");
                return Ok(json!({"width": w, "height": h}));
            }
            Ok(Value::Null)
        }
    }

    struct Fixture {
        streams: Streams,
        clock: Arc<FakeClock>,
        whip: Arc<FakeWhip>,
        media: Arc<FakeMedia>,
        calls: Receiver<(String, Value)>,
        screen: Arc<Mutex<(u32, u32)>>,
    }

    fn fixture_with(whip: FakeWhip, media: FakeMedia) -> Fixture {
        let clock = Arc::new(FakeClock(Mutex::new(Instant::now())));
        let whip = Arc::new(FakeWhip { clock: Some(clock.clone()), ..whip });
        let media = Arc::new(media);
        let (tx, calls) = channel();
        let screen = Arc::new(Mutex::new((1179, 2556)));
        let size = screen.clone();
        let make: MakeInput = Arc::new(move || Box::new(FakeInput { calls: tx.clone(), screen: size.clone() }));
        let streams = Streams::new(clock.clone(), media.clone(), whip.clone(), make);
        Fixture { streams, clock, whip, media, calls, screen }
    }

    fn fixture() -> Fixture {
        fixture_with(FakeWhip::default(), FakeMedia::default())
    }

    fn device(udid: &str, platform: Platform, state: DeviceState) -> Target {
        Target { platform, udid: udid.into(), name: udid.into(), state, avd_dir: None, screen: Some((1179, 2556)) }
    }

    /// `iphone` (the default) and `pixel` are booted, `off` isn't.
    fn devices(p: &Value, _screen: bool) -> Result<Target, String> {
        match p["device"].as_str().unwrap_or("booted") {
            "pixel" => Ok(device("pixel", Platform::Android, DeviceState::Booted)),
            "off" => Ok(device("off", Platform::Ios, DeviceState::Shutdown)),
            "iphone" | "booted" => Ok(device("iphone", Platform::Ios, DeviceState::Booted)),
            other => Err(format!("no device matches `{other}`")),
        }
    }

    const URL: &str = "http://127.0.0.1:8889/sim/whip";

    impl Fixture {
        fn call(&self, method: &str, p: Value) -> Result<Value, String> {
            self.streams.call(method, &p, &mut devices)
        }

        fn start(&self, p: Value) -> Result<Value, String> {
            self.call("stream_start", p)
        }

        fn status(&self, id: &str) -> Value {
            self.call("stream_status", json!({"stream_id": id})).expect("status")
        }

        /// Waits (in real time) until the stream's status satisfies `ok`.
        fn wait_for(&self, id: &str, ok: impl Fn(&Value) -> bool) -> Value {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let s = self.status(id);
                if ok(&s) {
                    return s;
                }
                assert!(Instant::now() < deadline, "timed out; last status {s}");
                std::thread::sleep(ms(5));
            }
        }

        /// The next `n` device calls the input thread made (`screen` aside), without the device
        /// fields (each names the iPhone).
        fn device_calls(&self, n: usize) -> Vec<(String, Value)> {
            let mut got = Vec::new();
            while got.len() < n {
                let Ok((method, mut params)) = self.calls.recv_timeout(Duration::from_secs(5)) else {
                    panic!("only {got:?}");
                };
                let o = params.as_object_mut().expect("object");
                assert_eq!((o.remove("device"), o.remove("platform")), (Some(json!("iphone")), Some(json!("ios"))), "{method}");
                if method != "screen" {
                    got.push((method, params));
                }
            }
            got
        }
    }

    fn stage_names(status: &Value) -> Vec<String> {
        status["stages"].as_array().expect("stages").iter().map(|s| s["stage"].as_str().expect("stage").to_owned()).collect()
    }

    #[test]
    fn start_connects_on_the_first_idr_and_status_shows_the_stages_in_order() {
        let f = fixture();
        let started = f.start(json!({"device": "iphone", "whip_url": URL, "token": "s3cret"})).expect("start");
        assert_eq!(
            started,
            json!({"stream_id": "stream-1", "whip_resource": format!("{URL}/session-0"), "lease_ttl_s": 30, "device": "iphone"})
        );
        let session = f.whip.session(0);
        assert_eq!(session.profile, ProfileLevelId::new(0x42, 0xe0, 0x34), "the first IDR's own profile-level-id is offered");
        assert_eq!(session.token.as_deref(), Some("s3cret"));
        assert!(session.input.lock().expect("input").is_none(), "no data channel without `input`");
        let (udid, config) = f.media.configs.lock().expect("configs")[0].clone();
        assert_eq!((udid.as_str(), config.fps, config.max_size, config.bitrate_bps), ("iphone", 30, Some(1280), 2_500_000));

        let status = f.wait_for("stream-1", |s| s["stats"]["sent"].as_u64() >= Some(3));
        assert_eq!(stage_names(&status), ["requested", "first_encoded_frame", "whip_connected"]);
        let at: Vec<f64> = status["stages"].as_array().expect("stages").iter().map(|s| s["ms"].as_f64().expect("ms")).collect();
        assert_eq!(at, [0.0, 0.0, 50.0], "the connection took the fake 50 ms after the first frame");
        assert_eq!((&status["state"], &status["platform"], &status["whip_url"]), (&json!("running"), &json!("ios"), &json!(URL)));
        assert_eq!((status["stats"]["keyframes"].as_u64(), status["stats"]["pli"].as_u64()), (Some(1), Some(0)), "{status}");
        assert!(status["stats"]["fps"].as_f64().is_some_and(|f| f > 0.0) && status["stats"]["bytes"].as_u64() > Some(0));
        assert!(!status.to_string().contains("s3cret") && !started.to_string().contains("s3cret"), "the token is never echoed");
        f.call("stream_stop", json!({"stream_id": "stream-1"})).expect("stop");
    }

    #[test]
    fn a_lapsed_lease_ends_the_stream_and_deletes_its_resource() {
        let f = fixture();
        f.start(json!({"whip_url": URL})).expect("start");
        f.clock.advance(Duration::from_secs(25));
        assert_eq!(f.status("stream-1")["lease_expires_in_s"], 30.0, "status renews the lease");
        f.clock.advance(Duration::from_secs(25));
        f.streams.reap();
        assert_eq!(f.call("streams", json!({})).expect("streams").as_array().map(Vec::len), Some(1), "renewed: still running");
        assert_eq!(f.whip.log(), [format!("POST {URL}")]);
        f.clock.advance(Duration::from_secs(6));
        // Any stream call reaps first; the lease thread does the same on its own.
        assert_eq!(f.call("streams", json!({})).expect("streams"), json!([]));
        assert_eq!(f.whip.log(), [format!("POST {URL}"), format!("DELETE {URL}/session-0")]);
        let err = f.call("stream_status", json!({"stream_id": "stream-1"})).expect_err("gone");
        assert!(err.contains("no stream `stream-1`"), "{err}");
    }

    #[test]
    fn one_stream_per_device_and_destination() {
        let f = fixture();
        f.start(json!({"device": "iphone", "whip_url": URL})).expect("first");
        let err = f.start(json!({"device": "iphone", "whip_url": URL, "fps": 60})).expect_err("a second to the same place");
        assert!(err.contains("already streaming") && err.contains("stream-1"), "{err}");
        assert_eq!(f.whip.log().len(), 1, "refused before any WHIP request");
        let other_url = "http://127.0.0.1:8889/other/whip";
        assert_eq!(f.start(json!({"device": "iphone", "whip_url": other_url})).expect("another URL")["stream_id"], "stream-2");
        assert_eq!(f.start(json!({"device": "pixel", "whip_url": URL})).expect("another device")["stream_id"], "stream-3");
        let list = f.call("streams", json!({})).expect("streams");
        let ids: Vec<&str> = list.as_array().expect("list").iter().map(|s| s["stream_id"].as_str().expect("id")).collect();
        assert_eq!(ids, ["stream-1", "stream-2", "stream-3"]);
        let mine = f.call("streams", json!({"device": "pixel"})).expect("one device's");
        assert_eq!((mine.as_array().map(Vec::len), &mine[0]["platform"]), (Some(1), &json!("android")));
        assert!(f.call("streams", json!({"device": "nope"})).expect_err("unknown device").contains("no device matches"));
        f.call("stream_stop", json!({"stream_id": "stream-1"})).expect("stop");
        assert_eq!(f.start(json!({"device": "iphone", "whip_url": URL})).expect("free again")["stream_id"], "stream-4");
        for id in ["stream-2", "stream-3", "stream-4"] {
            f.call("stream_stop", json!({"stream_id": id})).expect("stop");
        }
    }

    #[test]
    fn stop_ends_the_loop_then_sends_the_delete() {
        let f = fixture();
        f.start(json!({"whip_url": URL})).expect("start");
        f.wait_for("stream-1", |s| s["stats"]["sent"].as_u64() >= Some(2));
        let stopped = f.call("stream_stop", json!({"stream_id": "stream-1"})).expect("stop");
        assert_eq!((&stopped["state"], &stopped["deleted"]), (&json!("ended"), &json!(true)), "{stopped}");
        assert_eq!(stopped["reason"], "stopped by stream_stop");
        assert_eq!(stage_names(&stopped), ["requested", "first_encoded_frame", "whip_connected", "ended"]);
        assert_eq!(f.whip.log(), [format!("POST {URL}"), format!("DELETE {URL}/session-0")]);
        assert!(f.call("stream_stop", json!({"stream_id": "stream-1"})).expect_err("twice").contains("no stream"));
        assert!(f.call("stream_status", json!({"stream_id": "stream-1"})).is_err());
        assert!(f.call("stream_stop", json!({})).expect_err("no id").contains("stream_id"));
    }

    #[test]
    fn refusals_leave_nothing_running() {
        let f = fixture();
        let refused = |p: Value, want: &str| {
            let err = f.start(p.clone()).expect_err(&p.to_string());
            assert!(err.contains(want), "{p}: {err}");
            assert!(!err.contains("s3cret"), "{err}");
        };
        refused(json!({"whip_url": URL, "audio": true}), "#265");
        refused(json!({}), "whip_url");
        refused(json!({"whip_url": "ws://127.0.0.1/whip"}), "bad whip_url");
        refused(json!({"whip_url": "https://127.0.0.1/whip"}), "https");
        refused(json!({"whip_url": "http://192.168.1.20:8889/sim/whip", "token": "s3cret"}), "plaintext");
        refused(json!({"whip_url": URL, "fps": 0}), "fps");
        refused(json!({"whip_url": URL, "fps": "fast"}), "whole number");
        refused(json!({"whip_url": URL, "max_size": 8}), "max_size");
        refused(json!({"whip_url": URL, "device": "off"}), "isn't booted");
        refused(json!({"whip_url": URL, "device": "nope"}), "no device matches");
        assert!(f.whip.log().is_empty(), "nothing reached WHIP");

        let denied = fixture_with(FakeWhip { refuse: Some(401), ..FakeWhip::default() }, FakeMedia::default());
        let err = denied.start(json!({"whip_url": URL, "token": "s3cret"})).expect_err("401");
        assert!(err.contains("refused the credentials") && err.contains("401") && err.contains("`token`"), "{err}");
        assert!(!err.contains("s3cret"), "{err}");
        assert_eq!(denied.call("streams", json!({})).expect("streams"), json!([]), "a failed start is forgotten");
        assert_eq!(denied.whip.log(), [format!("POST {URL}")], "no resource, so no DELETE");

        let linux = fixture_with(FakeWhip::default(), FakeMedia { unavailable: true, ..FakeMedia::default() });
        assert!(linux.start(json!({"whip_url": URL})).expect_err("no encoder").starts_with("unsupported"));
        assert_eq!(linux.call("streams", json!({})).expect("streams"), json!([]));
    }

    #[test]
    fn keyframe_requests_and_estimates_reach_the_encoder_and_the_stats() {
        let f = fixture();
        f.start(json!({"whip_url": URL})).expect("start");
        f.wait_for("stream-1", |s| s["stats"]["sent"].as_u64() >= Some(2));
        let session = f.whip.session(0);
        session.fire(PublisherEvent::KeyframeRequested(KeyframeKind::Pli));
        session.fire(PublisherEvent::BitrateEstimate { bps: 1_200_000, source: sim_rtc::BitrateSource::Twcc });
        let status = f.wait_for("stream-1", |s| s["stats"]["keyframes"].as_u64() >= Some(2));
        assert_eq!((&status["stats"]["pli"], &status["stats"]["bitrate_estimate_bps"]), (&json!(1), &json!(1_200_000)));
        session.fire(PublisherEvent::KeyframeRequested(KeyframeKind::Fir));
        let status = f.wait_for("stream-1", |s| s["stats"]["keyframes"].as_u64() >= Some(3));
        assert_eq!(status["stats"]["fir"], 1);
        f.call("stream_stop", json!({"stream_id": "stream-1"})).expect("stop");
    }

    #[test]
    fn a_peer_that_disconnects_ends_the_stream_which_stays_listed_until_its_lease_lapses() {
        let f = fixture();
        f.start(json!({"whip_url": URL})).expect("start");
        f.whip.session(0).fire(PublisherEvent::Disconnected);
        let ended = f.wait_for("stream-1", |s| s["state"] == "ended");
        assert_eq!(ended["reason"], "the WHIP peer disconnected");
        assert_eq!(f.whip.log().last(), Some(&format!("DELETE {URL}/session-0")));
        f.clock.advance(Duration::from_secs(10));
        assert_eq!(f.status("stream-1")["lease_expires_in_s"], 20.0, "an ended stream's lease isn't renewed");
        // The same destination can be streamed to again; the ended stream is replaced.
        assert_eq!(f.start(json!({"whip_url": URL})).expect("again")["stream_id"], "stream-2");
        assert_eq!(f.call("streams", json!({})).expect("streams").as_array().map(Vec::len), Some(1));
        f.call("stream_stop", json!({"stream_id": "stream-2"})).expect("stop");
    }

    #[test]
    fn viewer_input_reaches_the_device_through_the_display_map() {
        let f = fixture();
        f.start(json!({"whip_url": URL, "input": true})).expect("start");
        let session = f.whip.session(0);
        // A 1179x2556 iPhone streamed with its longest side at 1280 is a 590x1280 frame.
        assert_eq!(DisplayMap::new(1179, 2556).stream_size(Some(1280)), FrameSize::new(590, 1280));
        let t = Instant::now();
        let at = |n: u64| t + ms(n);
        let script = [
            r#"{"v":1,"kind":"touch","phase":"down","id":0,"x":0.5,"y":0.25,"w":590,"h":1280}"#,
            r#"{"v":1,"kind":"touch","phase":"move","id":0,"x":0.25,"y":0.5,"w":590,"h":1280}"#,
            r#"{"v":1,"kind":"touch","phase":"up","id":0}"#,
            r#"{"v":1,"kind":"key","phase":"down","usage":4}"#,
            r#"{"v":1,"kind":"key","phase":"up","usage":4}"#,
            r#"{"v":1,"kind":"text","text":"hi"}"#,
            r#"{"v":1,"kind":"button","button":"home"}"#,
            r#"{"v":1,"kind":"button","button":"power","phase":"down"}"#,
        ];
        for (i, message) in script.iter().enumerate() {
            assert_eq!(session.viewer(at(20 * i as u64), message), [], "{message}");
        }
        // The viewer holds power for 700 ms.
        f.clock.advance(ms(700));
        let tail = [
            r#"{"v":1,"kind":"button","button":"power","phase":"up"}"#,
            r#"{"v":1,"kind":"wheel","x":0.5,"y":0.5,"w":590,"h":1280,"dx":0,"dy":128}"#,
        ];
        for (i, message) in tail.iter().enumerate() {
            assert_eq!(session.viewer(at(200 + 20 * i as u64), message), [], "{message}");
        }
        let got = f.device_calls(8);
        let y2 = got[7].1["y2"].as_f64().expect("y2");
        assert!((y2 - (1278.0 - 255.6)).abs() < 1e-6, "a 128-pixel scroll of the 1280-high frame is 255.6 device pixels: {y2}");
        let want = [
            ("touch", json!({"x": 589.5, "y": 639.0, "phase": "down"})),
            ("touch", json!({"x": 294.75, "y": 1278.0, "phase": "move"})),
            ("touch", json!({"x": 294.75, "y": 1278.0, "phase": "up"})),
            ("key", json!({"usage": 4})),
            ("type", json!({"text": "hi"})),
            ("button", json!({"button": "home"})),
            ("button", json!({"button": "lock", "hold_ms": 700})),
            ("swipe", json!({"x1": 589.5, "y1": 1278.0, "x2": 589.5, "y2": y2, "duration_ms": 100})),
        ];
        let got: Vec<(&str, Value)> = got.iter().map(|(m, p)| (m.as_str(), p.clone())).collect();
        assert_eq!(got, want);

        // A rotation: the device answers its new size, and points map onto that shape.
        *f.screen.lock().expect("screen") = (2556, 1179);
        assert_eq!(session.viewer(at(300), r#"{"v":1,"kind":"rotate","orientation":"landscape_left"}"#), []);
        assert_eq!(f.device_calls(1), [("rotate".to_owned(), json!({"orientation": "landscape-left"}))]);
        let landscape = r#"{"v":1,"kind":"touch","phase":"down","id":1,"x":0.5,"y":0.5,"w":1280,"h":590}"#;
        let deadline = Instant::now() + Duration::from_secs(5);
        // Refused (a stale frame) until the input thread has read the new size.
        while !session.viewer(at(320), landscape).is_empty() {
            assert!(Instant::now() < deadline, "the map never took the new size");
            std::thread::sleep(ms(5));
        }
        assert_eq!(f.device_calls(1), [("touch".to_owned(), json!({"x": 1278.0, "y": 589.5, "phase": "down"}))]);
        // A second finger while one is down is the sink's refusal.
        let second = session.viewer(at(340), r#"{"v":1,"kind":"touch","phase":"down","id":2,"x":0.1,"y":0.1,"w":1280,"h":590}"#);
        assert!(
            matches!(&second[..], [Reply::Error { code: ErrorCode::Sink, message, .. }] if message.contains("one touch at a time")),
            "{second:?}"
        );
        // Stopping lifts the finger still down.
        f.call("stream_stop", json!({"stream_id": "stream-1"})).expect("stop");
        assert_eq!(f.device_calls(1), [("touch".to_owned(), json!({"x": 1278.0, "y": 589.5, "phase": "up"}))]);
    }

    #[test]
    fn a_point_from_a_frame_of_another_shape_is_refused() {
        let (tx, _rx) = channel();
        let map = Arc::new(Mutex::new(DisplayMap::new(2556, 1179)));
        let mut sink = DeviceSink::new(Platform::Ios, map, tx, Arc::new(SystemClock));
        let point = Point { x: 0.5, y: 0.5, frame: FrameSize::new(590, 1280) };
        let err = sink.translate(&InputEvent::Touch { pointer: 0, phase: TouchPhase::Down, point }).expect_err("stale");
        assert!(err.contains("590x1280"), "{err}");
        let err = sink.translate(&InputEvent::Touch { pointer: 0, phase: TouchPhase::Move, point }).expect_err("not down");
        assert!(err.contains("isn't down"), "{err}");
    }

    #[test]
    fn android_input_maps_what_it_can_and_refuses_the_rest() {
        let (tx, _rx) = channel();
        let map = Arc::new(Mutex::new(DisplayMap::new(1080, 2400)));
        let mut sink = DeviceSink::new(Platform::Android, map, tx, Arc::new(SystemClock));
        assert!(sink.translate(&InputEvent::Rotate(Orientation::LandscapeLeft)).expect_err("no target orientation").contains("90°"));
        assert!(sink.translate(&InputEvent::Button { button: Button::Assistant, phase: ButtonPhase::Press }).is_err());
        assert_eq!(sink.translate(&InputEvent::Fold(Posture::HalfOpen)).expect("fold"), Some(("fold", json!({"posture": "half-opened"}))));
        let recents = InputEvent::Button { button: Button::AppSwitch, phase: ButtonPhase::Press };
        assert_eq!(sink.translate(&recents).expect("recents"), Some(("button", json!({"button": "recents"}))));
        // A cancel lifts the finger where it was, even from a frame of another shape.
        let down = Point { x: 0.5, y: 0.5, frame: FrameSize::new(540, 1200) };
        let pressed = sink.translate(&InputEvent::Touch { pointer: 3, phase: TouchPhase::Down, point: down }).expect("down");
        assert_eq!(pressed, Some(("touch", json!({"x": 540.0, "y": 1200.0, "phase": "down"}))));
        let rotated = Point { x: 0.1, y: 0.1, frame: FrameSize::new(1200, 540) };
        let cancel = sink.translate(&InputEvent::Touch { pointer: 3, phase: TouchPhase::Cancel, point: rotated }).expect("cancel");
        assert_eq!(cancel, Some(("touch", json!({"x": 540.0, "y": 1200.0, "phase": "up"}))));
    }

    #[test]
    fn the_method_table_routes_the_stream_methods_here() {
        let mut s = Session::default();
        let err = crate::methods::call(&mut s, "stream_stop", &json!({"stream_id": "stream-0"})).expect_err("none");
        assert!(err.contains("no stream `stream-0`"), "{err}");
        let err = crate::methods::call(&mut s, "stream_start", &json!({"whip_url": URL, "audio": true})).expect_err("audio");
        assert!(err.contains("#265"), "{err}");
        assert!(crate::methods::call(&mut s, "streams", &json!({})).expect("streams").is_array());
        for m in METHODS {
            assert_eq!(crate::platforms(m), &Platform::ALL, "{m}");
            assert_eq!(crate::catalog::group(m), Some(crate::catalog::Group::Stream), "{m}");
            assert!(crate::output_schema(m).is_some() && crate::input_schema(m).is_some(), "{m}");
        }
        assert!(crate::catalog::open_world("stream_start") && crate::catalog::effect("stream_start").is_some_and(|e| e.destructive()));
        assert!(crate::catalog::effect("stream_status").is_some_and(|e| e.read_only()));
    }

    /// docs/api.md has each stream method's row as the table writes it, and docs/streaming.md
    /// describes each one.
    #[test]
    fn the_docs_agree_with_the_table() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        if !root.join("crates/sim-api/Cargo.toml").is_file() {
            eprintln!("skipped: not in the simee workspace, so docs/ isn't here");
            return;
        }
        let api = std::fs::read_to_string(root.join("docs/api.md")).expect("docs/api.md");
        let streaming = std::fs::read_to_string(root.join("docs/streaming.md")).expect("docs/streaming.md");
        for m in METHODS {
            let (_, params, doc) = crate::METHODS.iter().find(|(name, ..)| *name == m).expect("in the table");
            let row = format!("| `{m}` | Android, iOS | {} | {} |", params.replace('|', "\\|"), doc.replace('|', "\\|"));
            assert!(api.lines().any(|l| l == row), "docs/api.md needs the row:\n{row}");
            assert!(streaming.contains(&format!("`{m}`")), "docs/streaming.md doesn't describe `{m}`");
        }
        assert!(streaming.contains(&format!("{} s", LEASE_TTL.as_secs())) && streaming.contains("`stream` toolset"));
    }
}
