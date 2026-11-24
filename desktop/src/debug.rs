//! The debug panel: device toggles, network, storage, processes and logs. Every operation goes
//! through the same API methods the unix socket serves, run on a worker thread so the UI never
//! waits on the device or host tools.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use rune::tok::{self, Theme, radius, type_};
use rune::{Color, El, Icon, ScrollSpec, col, el, icon, row, text};
use serde_json::{Value, json};
use sim_api::{Session, call_method};
use sim_core::debug::{CA_OPTIONS, CONTENT_SIZES};
use sim_debug::net::{Connection, Traffic};
use sim_debug::process::Process;
use sim_debug::storage::App;

use crate::icons;
use crate::{Action, State};

/// Panel tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Device,
    Network,
    Storage,
    Processes,
    Logs,
}

impl Tab {
    pub const ALL: [Tab; 5] = [Tab::Device, Tab::Network, Tab::Storage, Tab::Processes, Tab::Logs];

    fn icon(self) -> &'static Icon {
        match self {
            Tab::Device => &icons::SLIDERS,
            Tab::Network => &icons::GLOBE,
            Tab::Storage => &icons::FOLDER,
            Tab::Processes => &icons::CPU,
            Tab::Logs => &icons::TERMINAL,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Tab::Device => "Device",
            Tab::Network => "Network",
            Tab::Storage => "Storage",
            Tab::Processes => "Processes",
            Tab::Logs => "Logs",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Tab::Device => "tab-device",
            Tab::Network => "tab-network",
            Tab::Storage => "tab-storage",
            Tab::Processes => "tab-processes",
            Tab::Logs => "tab-logs",
        }
    }
}

/// What a finished job updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    CaState,
    Processes,
    Connections,
    Traffic,
    Containers,
    Logs,
    NetState,
    Flows,
    RecordStarted,
    /// A command; the label goes to the status line.
    Done(&'static str),
}

/// A panel interaction, queued by handlers and performed in `poll`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugAction {
    Tab(Tab),
    Reload,
    /// Calls an API method for the selected device, then reloads `then` if given.
    Api {
        label: &'static str,
        method: &'static str,
        params: Value,
        then: Option<Tag>,
    },
    ToggleOverlay(u32),
    ToggleContrast,
    ToggleHardwareKeyboard,
    ToggleIsland,
    ToggleMute,
    ToggleRecording,
    TextSize(i32),
    Logs(&'static str),
    Reveal(String),
}

/// Everything the panel shows.
#[derive(Default)]
pub struct Debug {
    pub tab: Tab,
    pub overlays: Vec<(u32, bool)>,
    pub contrast: bool,
    pub hardware_keyboard: bool,
    pub island_hidden: bool,
    pub muted: bool,
    /// The running recording's capture id and file.
    pub recording: Option<(String, String)>,
    pub text_size: i32,
    pub processes: Vec<Process>,
    pub connections: Vec<Connection>,
    pub traffic: Vec<Traffic>,
    pub containers: Vec<App>,
    pub logs: Vec<String>,
    /// `network_state`: profile, proxy port, injection.
    pub net_state: Value,
    pub flows: Vec<sim_netshape::proxy::Flow>,
    pub busy: usize,
    pub data_path: Option<String>,
    worker: Option<Worker>,
    /// The number the next submitted job gets; results are applied in this order per tag.
    seq: u64,
    /// Per tag, the number of the newest result applied: an older one finishing later (a slow
    /// listing that started before a command's quick reload) is dropped, never shown.
    applied: Vec<(Tag, u64)>,
}

/// Finished jobs waiting for `poll`, with the number each was submitted under.
type Results = Arc<Mutex<Vec<(Tag, u64, Result<Value, String>)>>>;

type Jobs = mpsc::Sender<(Tag, u64, &'static str, Value)>;

/// Two lanes, each a thread with its own API session, so a slow listing (container sizes,
/// `nettop`, a log query) never holds up a toggle: commands and the reloads that must follow
/// them go on `quick`, the tabs' listings on `slow`. Each lane keeps its own order.
struct Worker {
    quick: Jobs,
    slow: Jobs,
    results: Results,
}

/// Which lane a job goes on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    Quick,
    Slow,
}

impl Tag {
    /// Commands (and the cheap overlay state) are quick; listings are slow.
    fn lane(self) -> Lane {
        match self {
            Tag::CaState | Tag::RecordStarted | Tag::Done(_) => Lane::Quick,
            Tag::Processes | Tag::Connections | Tag::Traffic | Tag::Containers | Tag::Logs | Tag::NetState | Tag::Flows => Lane::Slow,
        }
    }
}

type Call = fn(&mut Session, &str, &Value) -> Result<Value, String>;

fn spawn_lane(name: &str, call: Call, results: Results, waker: rune::Waker) -> Option<Jobs> {
    let (tx, rx) = mpsc::channel::<(Tag, u64, &'static str, Value)>();
    let spawned = std::thread::Builder::new().name(name.into()).spawn(move || {
        let mut session = Session::default();
        for (tag, seq, method, params) in rx {
            let result = call(&mut session, method, &params);
            if let Ok(mut r) = results.lock() {
                r.push((tag, seq, result));
            }
            waker();
        }
    });
    spawned.ok().map(|_| tx)
}

impl Debug {
    /// Starts the worker threads (not in headless scenes).
    pub fn start(&mut self, waker: rune::Waker) {
        self.start_with(waker, call_method);
    }

    fn start_with(&mut self, waker: rune::Waker, call: Call) {
        let results: Results = Arc::default();
        let quick = spawn_lane("simee-debug", call, results.clone(), waker.clone());
        let slow = spawn_lane("simee-debug-lists", call, results.clone(), waker);
        if let (Some(quick), Some(slow)) = (quick, slow) {
            self.worker = Some(Worker { quick, slow, results });
            self.text_size = 3;
        }
    }

    fn submit(&mut self, udid: &str, tag: Tag, method: &'static str, params: Value) {
        self.submit_on(tag.lane(), udid, tag, method, params);
    }

    fn submit_on(&mut self, lane: Lane, udid: &str, tag: Tag, method: &'static str, mut params: Value) {
        let Some(w) = &self.worker else { return };
        params["device"] = json!(udid);
        let jobs = if lane == Lane::Quick { &w.quick } else { &w.slow };
        self.seq += 1;
        if jobs.send((tag, self.seq, method, params)).is_ok() {
            self.busy += 1;
        }
    }

    /// Whether a result of `tag` submitted as `seq` is newer than the last one applied (and, if
    /// so, records it as applied). Commands' own results are always reported.
    fn newest(&mut self, tag: Tag, seq: u64) -> bool {
        if matches!(tag, Tag::Done(_)) {
            return true;
        }
        match self.applied.iter_mut().find(|(t, _)| *t == tag) {
            Some((_, last)) if *last > seq => false,
            Some((_, last)) => {
                *last = seq;
                true
            }
            None => {
                self.applied.push((tag, seq));
                true
            }
        }
    }

    /// Loads what the current tab shows.
    pub fn reload(&mut self, udid: &str) {
        match self.tab {
            Tab::Device => self.submit(udid, Tag::CaState, "ca_debug_state", json!({})),
            Tab::Network => {
                self.submit(udid, Tag::NetState, "network_state", json!({}));
                self.submit(udid, Tag::Flows, "network_flows", json!({}));
                self.submit(udid, Tag::Connections, "connections", json!({}));
                self.submit(udid, Tag::Traffic, "traffic", json!({}));
            }
            Tab::Storage => self.submit(udid, Tag::Containers, "containers", json!({"sizes": true})),
            Tab::Processes => self.submit(udid, Tag::Processes, "processes", json!({})),
            Tab::Logs => self.submit(udid, Tag::Logs, "logs", json!({"last": "30s"})),
        }
    }

    /// Performs one panel action; returns a status line for actions that finish immediately.
    pub fn perform(&mut self, udid: &str, action: DebugAction) -> Option<String> {
        match action {
            DebugAction::Tab(tab) => {
                self.tab = tab;
                self.reload(udid);
            }
            DebugAction::Reload => self.reload(udid),
            DebugAction::Api { label, method, params, then } => {
                self.submit(udid, Tag::Done(label), method, params);
                if let Some(tag) = then {
                    let (method, params) = match tag {
                        Tag::Containers => ("containers", json!({"sizes": true})),
                        Tag::Processes => ("processes", json!({})),
                        Tag::NetState => ("network_state", json!({})),
                        _ => ("ca_debug_state", json!({})),
                    };
                    // Behind the command it reloads after, on the same lane.
                    self.submit_on(Lane::Quick, udid, tag, method, params);
                }
            }
            DebugAction::ToggleOverlay(id) => {
                let on = !self.overlays.iter().any(|&(i, on)| i == id && on);
                self.submit(udid, Tag::Done("overlay"), "ca_debug", json!({"option": id.to_string(), "on": on}));
                self.submit(udid, Tag::CaState, "ca_debug_state", json!({}));
            }
            DebugAction::ToggleContrast => {
                self.contrast = !self.contrast;
                self.submit(udid, Tag::Done("contrast"), "increase_contrast", json!({"on": self.contrast}));
            }
            DebugAction::ToggleHardwareKeyboard => {
                self.hardware_keyboard = !self.hardware_keyboard;
                self.submit(udid, Tag::Done("hardware keyboard"), "hardware_keyboard", json!({"on": self.hardware_keyboard}));
            }
            DebugAction::ToggleIsland => {
                self.island_hidden = !self.island_hidden;
                self.submit(udid, Tag::Done("dynamic island"), "dynamic_island", json!({"suppressed": self.island_hidden}));
            }
            DebugAction::ToggleMute => {
                self.muted = !self.muted;
                self.submit(udid, Tag::Done("audio mute"), "audio_mute", json!({"on": self.muted}));
            }
            DebugAction::ToggleRecording => match self.recording.take() {
                Some((id, _)) => self.submit(udid, Tag::Done("recording saved"), "audio_capture_stop", json!({"capture_id": id})),
                None => {
                    let dir = std::env::var("HOME").map(|h| format!("{h}/Desktop")).unwrap_or_else(|_| "/tmp".into());
                    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                    let path = format!("{dir}/simee-audio-{secs}.wav");
                    self.recording = Some((String::new(), path.clone()));
                    self.submit(udid, Tag::RecordStarted, "audio_capture_start", json!({"path": path}));
                }
            },
            DebugAction::TextSize(delta) => {
                self.text_size = (self.text_size + delta).clamp(0, CONTENT_SIZES.len() as i32 - 1);
                self.submit(udid, Tag::Done("text size"), "content_size", json!({"size": self.text_size.to_string()}));
            }
            DebugAction::Logs(last) => self.submit(udid, Tag::Logs, "logs", json!({"last": last})),
            DebugAction::Reveal(rel) => {
                let Some(root) = &self.data_path else { return Some("device path unknown".into()) };
                let path = format!("{root}/{rel}");
                // Finder can take a while to answer: don't wait for it (a thread reaps the child).
                return Some(match std::process::Command::new("/usr/bin/open").arg("-R").arg(&path).spawn() {
                    Ok(mut child) => {
                        std::thread::spawn(move || child.wait());
                        format!("revealing {path}")
                    }
                    Err(_) => format!("could not reveal {path}"),
                });
            }
        }
        None
    }

    /// Applies finished jobs; returns a status line for the last command, if any.
    pub fn drain(&mut self) -> (bool, Option<String>) {
        let Some(w) = &self.worker else { return (false, None) };
        let mut done: Vec<_> = w.results.lock().map(|mut r| std::mem::take(&mut *r)).unwrap_or_default();
        // Two lanes finish out of order: apply in submission order, and skip what a newer result
        // of the same tag has already replaced.
        done.sort_by_key(|&(_, seq, _)| seq);
        let mut status = None;
        let changed = !done.is_empty();
        for (tag, seq, result) in done {
            self.busy = self.busy.saturating_sub(1);
            if !self.newest(tag, seq) {
                continue;
            }
            let value = match result {
                Ok(v) => v,
                // No proxy running just means no flows to show.
                Err(_) if tag == Tag::Flows => {
                    self.flows.clear();
                    continue;
                }
                Err(e) => {
                    status = Some(format!("{tag:?}: {e}"));
                    continue;
                }
            };
            match tag {
                Tag::CaState => {
                    self.overlays = value
                        .as_array()
                        .map(|a| a.iter().filter_map(|o| Some((o["id"].as_u64()? as u32, o["on"].as_bool()?))).collect())
                        .unwrap_or_default();
                }
                Tag::Processes => self.processes = parse(value),
                Tag::Connections => self.connections = parse(value),
                Tag::Traffic => self.traffic = parse(value),
                Tag::Containers => self.containers = parse(value),
                Tag::Logs => {
                    let lines: Vec<String> = parse(value);
                    self.logs = lines.into_iter().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
                }
                Tag::NetState => self.net_state = value,
                Tag::RecordStarted => {
                    if let (Some((id, path)), Some(cid)) = (self.recording.as_mut(), value["capture_id"].as_str()) {
                        *id = cid.to_owned();
                        status = Some(format!("recording to {path}"));
                    }
                }
                Tag::Flows => self.flows = parse(value),
                Tag::Done(label) => status = Some(format!("{label}: done")),
            }
        }
        (changed, status)
    }
}

fn parse<T: serde::de::DeserializeOwned + Default>(v: Value) -> T {
    serde_json::from_value(v).unwrap_or_default()
}

// ----- view -------------------------------------------------------------------------------------

fn act(a: DebugAction) -> impl Fn(&mut State) + 'static {
    move |s: &mut State| s.actions.push(Action::Debug(a.clone()))
}

fn api(label: &'static str, method: &'static str, params: Value) -> DebugAction {
    DebugAction::Api { label, method, params, then: None }
}

/// A button with an icon and an optional label. `key` makes it findable in tests.
pub fn icon_button(
    key: impl Into<String>,
    glyph: &'static Icon,
    label: &str,
    show_label: bool,
    t: Theme,
    on: impl Fn(&mut State) + 'static,
) -> El {
    let mut b = row()
        .key(key.into())
        .role("button")
        .label(label.to_string())
        .h(28.0)
        .px(if show_label { 10.0 } else { 7.0 })
        .gap(6.0)
        .center_y()
        .radius(radius::SM)
        .fill(t.control_off)
        .on_press(on)
        .child(icon(glyph, 15.0).color(t.text));
    if show_label {
        b = b.child(text(label.to_string()).font(type_::LABEL).color(t.text));
    }
    b
}

fn switch(on: bool, t: Theme) -> El {
    let knob = el().size(14.0, 14.0).radius(7.0).fill(Color::WHITE);
    let mut track = row().w(30.0).h(18.0).padding(2.0).radius(9.0).fill(if on { tok::accent(0, true) } else { t.control_off });
    if on {
        track = track.child(el().grow(1.0));
    }
    track.child(knob)
}

fn toggle_row(key: impl Into<String>, glyph: &'static Icon, label: &str, on: bool, t: Theme, a: DebugAction) -> El {
    row()
        .key(key.into())
        .role("switch")
        .label(label.to_string())
        .w_full()
        .h(30.0)
        .gap(8.0)
        .center_y()
        .on_press(act(a))
        .child(icon(glyph, 15.0).color(t.text_secondary))
        .child(text(label.to_string()).font(type_::BODY).color(t.text).grow(1.0).truncate())
        .child(switch(on, t))
}

fn section(title: &str, t: Theme) -> El {
    text(title.to_string()).font(type_::SECTION).color(t.text_secondary).padding(2.0)
}

/// Buttons in rows of three, so a group never overflows the panel.
fn wrap_row(children: Vec<El>) -> El {
    let mut rows = col().w_full().gap(6.0);
    let mut it = children.into_iter().peekable();
    while it.peek().is_some() {
        let mut r = row().gap(6.0);
        for c in it.by_ref().take(3) {
            r = r.child(c);
        }
        rows = rows.child(r);
    }
    rows
}

fn device_tab(s: &State, t: Theme) -> El {
    let d = &s.debug;
    let mut c = col().w_full().gap(6.0).child(section("RENDER OVERLAYS", t));
    for (id, key, label) in CA_OPTIONS {
        let on = d.overlays.iter().any(|&(i, on)| i == *id && on);
        c = c.child(toggle_row(format!("overlay-{key}"), &icons::LAYERS, label, on, t, DebugAction::ToggleOverlay(*id)));
    }
    let size_label = CONTENT_SIZES.get(d.text_size as usize).copied().unwrap_or("large").replace('-', " ");
    c = c
        .child(section("ACCESSIBILITY", t))
        .child(toggle_row("toggle-contrast", &icons::CONTRAST, "Increase contrast", d.contrast, t, DebugAction::ToggleContrast))
        .child(
            row()
                .w_full()
                .h(30.0)
                .gap(8.0)
                .center_y()
                .child(icon(&icons::TEXT_SIZE, 15.0).color(t.text_secondary))
                .child(text(format!("Text size: {size_label}")).key("text-size-label").font(type_::BODY).color(t.text).grow(1.0).truncate())
                .child(icon_button("text-smaller", &icons::MINUS, "Smaller text", false, t, act(DebugAction::TextSize(-1))))
                .child(icon_button("text-larger", &icons::PLUS, "Larger text", false, t, act(DebugAction::TextSize(1)))),
        )
        .child(section("HARDWARE", t))
        .child(toggle_row("toggle-keyboard", &icons::KEYBOARD, "Hardware keyboard", d.hardware_keyboard, t, DebugAction::ToggleHardwareKeyboard))
        .child(toggle_row("toggle-island", &icons::ISLAND, "Hide Dynamic Island", d.island_hidden, t, DebugAction::ToggleIsland))
        .child(wrap_row(vec![
            icon_button("memory-warning", &icons::ALERT, "Memory warning", true, t, act(api("memory warning", "memory_warning", json!({})))),
            icon_button("reset-keychain", &icons::KEY, "Reset keychain", true, t, act(api("reset keychain", "reset_keychain", json!({})))),
        ]))
        .child(section("AUDIO", t))
        .child(toggle_row("toggle-mute", &icons::SPEAKER_OFF, "Mute this device on the host", d.muted, t, DebugAction::ToggleMute))
        .child(toggle_row("toggle-record", &icons::RECORD, "Record device audio (WAV on Desktop)", d.recording.is_some(), t, DebugAction::ToggleRecording))
        .child(section("BIOMETRICS", t))
        .child(wrap_row(vec![
            icon_button("bio-enroll", &icons::FACE, "Enroll", true, t, act(api("enroll", "biometric", json!({"action": "enroll"})))),
            icon_button("bio-unenroll", &icons::CLOSE, "Unenroll", true, t, act(api("unenroll", "biometric", json!({"action": "unenroll"})))),
            icon_button("bio-face-match", &icons::FACE, "Face match", true, t, act(api("face match", "biometric", json!({"action": "match", "face": true})))),
            icon_button("bio-face-nomatch", &icons::FACE, "Face no match", true, t, act(api("face no match", "biometric", json!({"action": "nomatch", "face": true})))),
            icon_button("bio-touch-match", &icons::FINGERPRINT, "Touch match", true, t, act(api("touch match", "biometric", json!({"action": "match", "face": false})))),
        ]))
        .child(section("STATUS BAR", t))
        .child(wrap_row(vec![
            icon_button(
                "status-demo",
                &icons::SIGNAL,
                "Demo (9:41)",
                true,
                t,
                act(api(
                    "status bar",
                    "status_bar",
                    json!({"time": "9:41", "data_network": "5g", "cellular_mode": "active", "cellular_bars": 4, "wifi_mode": "active", "wifi_bars": 3, "battery_state": "charged", "battery_level": 100}),
                )),
            ),
            icon_button("status-clear", &icons::CLOSE, "Clear", true, t, act(api("clear status bar", "clear_status_bar", json!({})))),
        ]))
        .child(section("LOCATION", t))
        .child(wrap_row(
            [("Cupertino", 37.3349, -122.009), ("London", 51.5074, -0.1278), ("Tokyo", 35.6762, 139.6503)]
                .into_iter()
                .map(|(name, lat, lon)| {
                    icon_button(
                        format!("location-{}", name.to_lowercase()),
                        &icons::PIN,
                        name,
                        true,
                        t,
                        act(api("location", "location", json!({"latitude": lat, "longitude": lon}))),
                    )
                })
                .collect(),
        ));
    c
}

fn mono_row(key: String, cells: Vec<(String, f32)>, t: Theme, trailing: Option<El>) -> El {
    let mut r = row().key(key).w_full().h(24.0).gap(6.0).center_y();
    for (i, (value, w)) in cells.into_iter().enumerate() {
        let cell = text(value).font(type_::MONO_CAPTION).color(if i == 0 { t.text } else { t.text_secondary }).truncate();
        r = r.child(if w == 0.0 { cell.grow(1.0) } else { cell.w(w) });
    }
    if let Some(tr) = trailing {
        r = r.child(tr);
    }
    r
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / f64::from(1u32 << 30)),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / f64::from(1u32 << 20)),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// One-line summary of `network_state`.
fn net_summary(v: &Value) -> String {
    let p = &v["profile"];
    if p.is_null() {
        return "not shaped".into();
    }
    let mut parts = Vec::new();
    if p["offline"] == true {
        parts.push("offline".to_owned());
    }
    for (key, unit) in [("latency_ms", "ms"), ("down_kbps", "kbps down"), ("up_kbps", "kbps up"), ("loss_pct", "% loss")] {
        if let Some(n) = p[key].as_u64().filter(|&n| n > 0) {
            parts.push(format!("{n} {unit}"));
        }
    }
    let rules = p["rules"].as_array().map_or(0, Vec::len);
    if rules > 0 {
        parts.push(format!("{rules} host rules"));
    }
    let profile = if parts.is_empty() { "unshaped".to_owned() } else { parts.join(", ") };
    let mut how = Vec::new();
    if v["boot_injected"] == true {
        how.push("all processes".to_owned());
    }
    if let Some(port) = v["proxy_port"].as_u64() {
        how.push(format!("proxy :{port}"));
    }
    if v["injected"] == true {
        how.push("new processes".to_owned());
    }
    if how.is_empty() { profile } else { format!("{profile} · {}", how.join(" + ")) }
}

fn network_tab(s: &State, t: Theme) -> El {
    let d = &s.debug;
    let boot = d.net_state["boot_injected"] == true;
    let shape = |preset: &'static str| {
        act(DebugAction::Api {
            label: "network",
            method: "network_shape",
            params: json!({"preset": preset, "proxy": !boot}),
            then: Some(Tag::NetState),
        })
    };
    let mut c = col()
        .w_full()
        .gap(2.0)
        .child(section("SHAPING", t))
        .child(text(net_summary(&d.net_state)).key("net-summary").font(type_::MONO_CAPTION).color(t.text).truncate())
        .child(wrap_row(vec![
            icon_button("net-none", &icons::GLOBE, "None", true, t, shape("none")),
            icon_button("net-offline", &icons::WIFI_OFF, "Offline", true, t, shape("offline")),
            icon_button("net-lossy", &icons::ALERT, "Lossy", true, t, shape("lossy")),
            icon_button("net-3g", &icons::SIGNAL, "3G", true, t, shape("3g")),
            icon_button("net-edge", &icons::GAUGE, "Edge", true, t, shape("edge")),
            icon_button("net-lte", &icons::SIGNAL, "LTE", true, t, shape("lte")),
        ]));
    if !boot {
        c = c.child(icon_button(
            "net-boot",
            &icons::POWER,
            "Shape every process (reboots)",
            true,
            t,
            act(DebugAction::Api {
                label: "reboot shaped",
                method: "network_shape",
                params: json!({"inject": "boot", "proxy": false}),
                then: Some(Tag::NetState),
            }),
        ));
    }
    if !d.flows.is_empty() {
        c = c.child(section(&format!("PROXIED HOSTS ({})", d.flows.len()), t));
        for (i, f) in d.flows.iter().rev().take(40).enumerate() {
            c = c.child(mono_row(
                format!("flow-{i}"),
                vec![(format!("{}:{}", f.host, f.port), 0.0), (f.outcome.clone(), 64.0), (format!("↓{}", bytes(f.bytes_down)), 70.0)],
                t,
                None,
            ));
        }
    }
    c = c.child(section(&format!("TRAFFIC ({})", d.traffic.len()), t));
    for (i, tr) in d.traffic.iter().take(30).enumerate() {
        c = c.child(mono_row(
            format!("traffic-{i}"),
            vec![(tr.process.clone(), 0.0), (format!("↓{}", bytes(tr.bytes_in)), 70.0), (format!("↑{}", bytes(tr.bytes_out)), 70.0)],
            t,
            None,
        ));
    }
    c = c.child(section(&format!("SOCKETS ({})", d.connections.len()), t));
    for (i, cn) in d.connections.iter().take(80).enumerate() {
        let peer = if cn.remote.is_empty() { cn.local.clone() } else { cn.remote.clone() };
        c = c.child(mono_row(
            format!("socket-{i}"),
            vec![(cn.process.clone(), 110.0), (format!("{} {peer}", cn.protocol), 0.0), (cn.state.clone(), 80.0)],
            t,
            None,
        ));
    }
    c
}

fn storage_tab(s: &State, t: Theme) -> El {
    let d = &s.debug;
    let mut apps: Vec<&App> = d.containers.iter().filter(|a| a.data.is_some()).collect();
    apps.sort_by_key(|a| std::cmp::Reverse(a.data_bytes + a.bundle_bytes));
    let mut c = col().w_full().gap(2.0).child(section(&format!("APP CONTAINERS ({})", apps.len()), t));
    for (i, a) in apps.into_iter().take(60).enumerate() {
        let data = a.data.clone().unwrap_or_default();
        let name = a.bundle_id.strip_prefix("com.apple.").map_or_else(|| a.bundle_id.clone(), |n| format!("·{n}"));
        let buttons = row()
            .gap(4.0)
            .child(icon_button(format!("push-{i}"), &icons::BELL, "Send test push", false, t, {
                let id = a.bundle_id.clone();
                act(api("push", "push", json!({"bundle_id": id, "payload": {"aps": {"alert": {"title": "simee", "body": "Test notification"}, "sound": "default"}}})))
            }))
            .child(icon_button(format!("reveal-{i}"), &icons::REVEAL, "Reveal in Finder", false, t, act(DebugAction::Reveal(data.clone()))))
            .child(icon_button(format!("clear-{i}"), &icons::TRASH, "Clear app data", false, t, act(DebugAction::Api { label: "clear data", method: "clear_container", params: json!({"path": data}), then: Some(Tag::Containers) })));
        c = c.child(
            mono_row(format!("container-{i}"), vec![(name, 0.0), (bytes(a.data_bytes + a.bundle_bytes), 64.0)], t, Some(buttons)).h(30.0),
        );
    }
    c
}

fn processes_tab(s: &State, t: Theme) -> El {
    let d = &s.debug;
    let mut c = col().w_full().gap(2.0).child(section(&format!("PROCESSES ({})", d.processes.len()), t));
    for (i, p) in d.processes.iter().take(80).enumerate() {
        let kill = icon_button(
            format!("kill-{i}"),
            &icons::CLOSE,
            "Terminate process",
            false,
            t,
            act(DebugAction::Api { label: "kill", method: "kill", params: json!({"pid": p.pid}), then: Some(Tag::Processes) }),
        );
        c = c.child(
            mono_row(
                format!("process-{i}"),
                vec![(p.name.clone(), 0.0), (p.pid.to_string(), 48.0), (format!("{:.1}%", p.cpu), 44.0), (bytes(p.rss_kb * 1024), 60.0)],
                t,
                Some(kill),
            )
            .h(30.0),
        );
    }
    c
}

fn logs_tab(s: &State, t: Theme) -> El {
    let mut c = col().w_full().gap(2.0).child(wrap_row(vec![
        icon_button("logs-30s", &icons::TERMINAL, "Last 30s", true, t, act(DebugAction::Logs("30s"))),
        icon_button("logs-2m", &icons::TERMINAL, "Last 2m", true, t, act(DebugAction::Logs("2m"))),
    ]));
    for (i, line) in s.debug.logs.iter().enumerate() {
        c = c.child(text(line.clone()).key(format!("log-{i}")).font(type_::MONO_CAPTION).color(t.text_secondary).truncate());
    }
    c
}

/// The panel: tab bar, reload, and the current tab's content.
pub fn panel(s: &State, t: Theme) -> El {
    let mut tabs = row().key("debug-tabs").w_full().gap(4.0).center_y();
    for tab in Tab::ALL {
        let selected = s.debug.tab == tab;
        tabs = tabs.child(
            row()
                .key(tab.key())
                .role("tab")
                .label(tab.label().to_string())
                .h(28.0)
                .px(7.0)
                .center_y()
                .radius(radius::SM)
                .fill(if selected { tok::accent(0, true) } else { t.control_off })
                .on_press(act(DebugAction::Tab(tab)))
                .child(icon(tab.icon(), 15.0).color(t.text)),
        );
    }
    tabs = tabs.child(el().grow(1.0));
    if s.debug.busy > 0 {
        tabs = tabs.child(text("…").font(type_::BODY).color(t.text_secondary));
    }
    tabs = tabs.child(icon_button("debug-reload", &icons::REFRESH, "Reload", false, t, act(DebugAction::Reload)));
    let body = match s.debug.tab {
        Tab::Device => device_tab(s, t),
        Tab::Network => network_tab(s, t),
        Tab::Storage => storage_tab(s, t),
        Tab::Processes => processes_tab(s, t),
        Tab::Logs => logs_tab(s, t),
    };
    col()
        .key("debug-panel")
        .w(340.0)
        .min_w(340.0)
        .h_full()
        .padding(12.0)
        .gap(10.0)
        .fill(t.card)
        .border(1.0, t.card_border)
        .child(el().h(10.0))
        .child(text(s.debug.tab.label().to_uppercase()).font(type_::SECTION).color(t.text_secondary))
        .child(tabs)
        .child(col().w_full().child(body).scroll(ScrollSpec { stick_bottom: false }).shrink().grow(1.0).min_h(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_formatting() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(2048), "2 KB");
        assert_eq!(bytes(5 << 20), "5.0 MB");
        assert_eq!(bytes(3 << 30), "3.0 GB");
    }

    static GATE: Mutex<Option<mpsc::Receiver<()>>> = Mutex::new(None);

    /// A stand-in for the API: listings wait on `GATE`, commands answer at once.
    fn fake_call(_: &mut Session, method: &str, _: &Value) -> Result<Value, String> {
        if method == "containers"
            && let Some(gate) = GATE.lock().map_err(|e| e.to_string())?.take()
        {
            gate.recv().ok();
        }
        Ok(json!([]))
    }

    /// A slow listing doesn't hold up a toggle queued after it; a command's follow-up reload
    /// stays behind the command.
    #[test]
    fn a_slow_listing_does_not_delay_a_toggle() {
        let (release, gate) = mpsc::channel::<()>();
        *GATE.lock().expect("gate") = Some(gate);
        let mut d = Debug::default();
        d.start_with(Arc::new(|| {}), fake_call);
        d.submit("A1", Tag::Containers, "containers", json!({}));
        d.perform("A1", DebugAction::ToggleContrast);
        let start = std::time::Instant::now();
        let mut status = None;
        while status.is_none() {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "the toggle finished while the listing waits");
            status = d.drain().1;
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(status.as_deref(), Some("contrast: done"));
        assert_eq!(d.busy, 1, "the listing is still running");
        release.send(()).expect("release");
        while d.busy > 0 {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "the listing finished");
            d.drain();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(Tag::Containers.lane(), Lane::Slow);
        assert_eq!(Tag::Done("x").lane(), Lane::Quick);
    }

    static STALE_GATE: Mutex<Option<mpsc::Receiver<()>>> = Mutex::new(None);

    /// The listing asked for first (`{"before": true}`) waits on `STALE_GATE` and still shows
    /// the app; every later one sees it cleared.
    fn clearing_call(_: &mut Session, method: &str, params: &Value) -> Result<Value, String> {
        if method != "containers" {
            return Ok(json!({}));
        }
        if params["before"] == json!(true) {
            if let Some(gate) = STALE_GATE.lock().map_err(|e| e.to_string())?.take() {
                gate.recv().ok();
            }
            return Ok(json!([{"bundle_id": "com.example", "data": "Data/x", "bundle": null, "data_bytes": 9, "bundle_bytes": 0}]));
        }
        Ok(json!([]))
    }

    /// "Clear app data" while a slow Storage listing runs: the command's quick reload lands
    /// first, and the older listing finishing after it is dropped, not shown over it.
    #[test]
    fn an_older_listing_never_overwrites_a_newer_reload() {
        let (release, gate) = mpsc::channel::<()>();
        *STALE_GATE.lock().expect("gate") = Some(gate);
        let mut d = Debug::default();
        d.start_with(Arc::new(|| {}), clearing_call);
        d.submit("A1", Tag::Containers, "containers", json!({"before": true}));
        d.perform("A1", DebugAction::Api { label: "cleared", method: "clear_container", params: json!({}), then: Some(Tag::Containers) });
        let start = std::time::Instant::now();
        while d.busy > 1 {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "the command and its reload finished");
            d.drain();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(d.containers.is_empty(), "the reload after the clear");
        release.send(()).expect("release");
        while d.busy > 0 {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "the listing finished");
            d.drain();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(d.containers.is_empty(), "the older listing was dropped: {:?}", d.containers);
        // Per tag, a result older than the newest applied is dropped.
        assert!(d.newest(Tag::Processes, 7) && !d.newest(Tag::Processes, 6) && d.newest(Tag::Processes, 8));
        assert!(d.newest(Tag::Done("x"), 1), "command results always report");
    }
}
