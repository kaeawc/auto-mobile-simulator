//! simee: create, boot and drive iOS simulator devices and Android emulator devices.
//!
//! Every device command is a call into the API's method table (`sim-api`), the same code the
//! socket server and the desktop window run; this file only parses arguments and prints results.
//! When a server (`simee serve`) of this build and environment answers on the default socket,
//! commands go to it, so they reuse its service connection and per-device input connections;
//! otherwise (and always for `boot`, see `sim_api::IN_CALLER`) they run in this process. The
//! output is the same either way.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use sim_core::device::DeviceInfo;

#[derive(Parser)]
#[command(name = "simee", version, about = "Create, boot and drive iOS simulator and Android emulator devices")]
struct Cli {
    /// Run the command in this process even when a server is listening on the default socket
    /// (also `SIMEE_IN_PROCESS=1`).
    #[arg(long)]
    in_process: bool,
    /// Say on stderr where each command runs and, when not on the server, why (also
    /// `SIMEE_VERBOSE=1`). `SIMEE_REQUIRE_SERVER=1` makes a command that could have gone to a
    /// server fail instead of running here.
    #[arg(long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Cmd,
}

/// Which CPU an iOS settle wait judges (`settle_foreground`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum SettleCpu {
    All,
    Foreground,
}

#[derive(Subcommand)]
enum Cmd {
    /// List devices.
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
        /// Only booted devices.
        #[arg(long)]
        booted: bool,
        /// Only this platform's devices: android or ios.
        #[arg(long)]
        platform: Option<String>,
        /// Only this form factor: phone, tablet, foldable, watch, tv, auto, vision.
        #[arg(long)]
        form_factor: Option<String>,
    },
    /// Every form factor on every platform: sub-platform and inputs, or why it's unsupported.
    FormFactors {
        #[arg(long)]
        json: bool,
        /// Only this platform: android or ios.
        #[arg(long)]
        platform: Option<String>,
    },
    /// List installed runtimes.
    Runtimes {
        #[arg(long)]
        json: bool,
    },
    /// List device types.
    DeviceTypes {
        #[arg(long)]
        json: bool,
    },
    /// Create a device; prints its UDID (an Android device's UDID is its AVD name).
    ///
    /// iOS: `create NAME DEVICE_TYPE RUNTIME`. Android: `create NAME --platform android
    /// --system-image system-images;android-36;google_apis;arm64-v8a`.
    Create {
        name: String,
        /// iOS device type (name or identifier).
        device_type: Option<String>,
        /// iOS runtime (name or identifier).
        runtime: Option<String>,
        /// ios (default) or android.
        #[arg(long)]
        platform: Option<String>,
        /// Android system image, as an SDK package or a path.
        #[arg(long)]
        system_image: Option<String>,
        /// Fail unless the device type or system image is this form factor; shapes a new AVD.
        #[arg(long)]
        form_factor: Option<String>,
        /// Android: the hardware a new AVD follows, as avdmanager's `-d` (foldables: pixel_fold,
        /// surface_duo).
        #[arg(long)]
        device: Option<String>,
    },
    /// Delete a device and its data.
    Delete { device: String },
    /// Boot a device and wait until it finishes booting.
    Boot {
        device: String,
        /// Environment for the device's launchd, `KEY=VALUE` (repeatable).
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Inject the network shaper into every process; shape later with `network_shape`.
        #[arg(long)]
        shaped: bool,
        /// Seconds to wait for the first frame.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        /// iOS: also wait until the device is quiet (its processes' foreground CPU under 10 % of a
        /// core for 10 s) and print the timings. A device booted but not settled in time prints
        /// "booted, not settled" and still exits 0.
        #[arg(long)]
        settle: bool,
        /// iOS: seconds the settle wait may take after the boot (default 600).
        #[arg(long)]
        settle_timeout: Option<u64>,
        /// iOS: which CPU the settle wait judges: `all`, or `foreground` (background-QoS work on
        /// the efficiency cores doesn't count). Default: foreground.
        #[arg(long, value_enum)]
        settle_cpu: Option<SettleCpu>,
        /// iOS: turn off a named set of daemons once booted (`slim`).
        #[arg(long)]
        profile: Option<String>,
        /// Android: play the device's audio on the host's default output (silent otherwise).
        #[arg(long)]
        audio: bool,
    },
    /// Boot several devices of one platform, a few at a time, and print each one's timings.
    /// Exits non-zero only when a device failed to boot (one that booted but didn't settle in
    /// time is counted in `unsettled`).
    BootMany {
        #[arg(required = true)]
        devices: Vec<String>,
        /// Boots in flight at once (default: a quarter of the host's CPUs, at least 2).
        #[arg(short = 'j', long)]
        concurrency: Option<u64>,
        /// Seconds each device's boot may take.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// iOS: wait until each device is quiet (the default; kept for older scripts).
        #[arg(long, hide = true, conflicts_with = "no_settle")]
        settle: bool,
        /// iOS: don't wait until each device is quiet (a slot then bounds only the boot call).
        #[arg(long)]
        no_settle: bool,
        /// iOS: seconds each settle wait may take after the boot (default 600, more above K=4).
        #[arg(long)]
        settle_timeout: Option<u64>,
        /// iOS: which CPU each settle wait judges: `all`, or `foreground`. Default: foreground.
        #[arg(long, value_enum)]
        settle_cpu: Option<SettleCpu>,
        /// iOS: turn off a named set of daemons on each device (`slim`).
        #[arg(long)]
        profile: Option<String>,
        /// Android: the first console port to hand out (default 5554).
        #[arg(long)]
        first_port: Option<u16>,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// iOS: wait until a booted simulator is quiet; prints how long that took.
    Settle {
        device: String,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Which CPU to judge: `all`, or `foreground` (background-QoS work doesn't count). Default: foreground.
        #[arg(long, value_enum)]
        settle_cpu: Option<SettleCpu>,
    },
    /// iOS: turn a named set of daemons off on a booted simulator (`slim`), or back on with --undo.
    Profile {
        device: String,
        name: String,
        /// Re-enable the profile's daemons (they run again from the next boot).
        #[arg(long)]
        undo: bool,
    },
    /// Create a device as a copy of a shut-down one (iOS: a simulator; Android: an AVD from a
    /// golden booted once under simee); prints its UDID (an AVD's name).
    Clone { template: String, name: String },
    /// Shut a device down.
    Shutdown { device: String },
    /// Erase all content and settings (device must be shut down).
    Erase { device: String },
    /// Save the screen as a PNG.
    Screenshot {
        device: String,
        /// Where to write the PNG; `-` writes it to stdout.
        path: String,
        /// iOS: `outer` or `inner` for an iPhone Duo's screen (`main` is the one in use), `carplay` for an iPhone's CarPlay screen.
        #[arg(long, default_value = "main")]
        display: String,
    },
    /// Tap at a point, in pixels from the top-left of the screen.
    Tap {
        device: String,
        x: f64,
        y: f64,
        /// Hold time in milliseconds (default: the platform's, Android 60, iOS 10).
        #[arg(long)]
        hold: Option<u64>,
        /// iOS: `outer` or `inner` for an iPhone Duo's screen (`main` is the one in use), `carplay` for an iPhone's CarPlay screen.
        #[arg(long, default_value = "main")]
        display: String,
    },
    /// Swipe between two points, in pixels.
    Swipe {
        device: String,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        /// Duration in milliseconds.
        #[arg(long, default_value_t = 300)]
        duration: u64,
        /// Pause at the end before lifting (a drag rather than a flick).
        #[arg(long)]
        settle: bool,
        /// iOS: `outer` or `inner` for an iPhone Duo's screen (`main` is the one in use), `carplay` for an iPhone's CarPlay screen.
        #[arg(long, default_value = "main")]
        display: String,
        /// iOS: the touch comes in from this screen edge (top, bottom, left, right), as a drag from
        /// Simulator.app's bezel: `--edge bottom` from the bottom goes home.
        #[arg(long)]
        edge: Option<String>,
    },
    /// Press a hardware button: home, lock (power), side, siri, apple-pay, action (iOS); back,
    /// volume-up, volume-down, recents, menu (Android); up, down, left, right, select, play-pause
    /// (TV remotes: the Siri Remote on Apple TV, where menu and back are its Menu button).
    Button {
        device: String,
        button: String,
        /// Hold time in milliseconds (default: 10 for home and lock on an iPhone or iPad, else 80).
        #[arg(long)]
        hold: Option<u64>,
    },
    /// Type text on the hardware keyboard (US layout).
    Type { device: String, text: String },
    /// Press a key by HID usage (page 7), e.g. 40 for Return.
    Key { device: String, usage: u32 },
    /// Open a URL.
    OpenUrl { device: String, url: String },
    /// Install an .app bundle.
    Install { device: String, path: String },
    /// Uninstall an app.
    Uninstall { device: String, bundle_id: String },
    /// Launch an app; prints its pid.
    Launch { device: String, bundle_id: String },
    /// Terminate an app.
    Terminate { device: String, bundle_id: String },
    /// List installed bundle identifiers.
    Apps { device: String },
    /// Set light or dark appearance.
    Appearance { device: String, mode: String },
    /// Set the simulated location.
    #[command(allow_negative_numbers = true)]
    Location { device: String, latitude: f64, longitude: f64 },
    /// Grant or revoke a privacy permission for an app (photos, camera, contacts, all, ...).
    Privacy { device: String, action: String, service: String, bundle_id: String },
    /// Simulate a memory warning.
    MemoryWarning { device: String },
    /// Rotate a device 90° clockwise, or (iOS) to an orientation.
    Rotate {
        device: String,
        /// portrait, landscape-right, portrait-upside-down or landscape-left (iOS).
        #[arg(long)]
        orientation: Option<String>,
    },
    /// Set a foldable's hinge: fold, unfold, a posture (closed, half-opened, opened, flipped, tent) or an angle (0-180;
    /// 0-360 on a dual-screen). Android foldables, and the iPhone Duo (iOS: closed, half-opened, opened, 0-180).
    Fold { device: String, posture: String },
    /// Android XR: recenter, rotate X Y Z (radians), move DX DY DZ (metres), options day|night
    /// PASSTHROUGH (0-1), input mouse|hand|eye, viewport pan|zoom|rotate, pointer X Y [PHASE].
    Xr {
        device: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Touch the touchpad beside the screen (Android XR display glasses), in touchpad units: X Y
    /// [down|move|up|tap], or swipe with --to X2 Y2.
    Touchpad {
        device: String,
        x: u32,
        y: u32,
        /// down, move, up or tap (the default).
        phase: Option<String>,
        /// Swipe to this point instead.
        #[arg(long, num_args = 2, value_names = ["X2", "Y2"])]
        to: Option<Vec<u32>>,
        /// The swipe's duration.
        #[arg(long, default_value_t = 300)]
        duration_ms: u64,
    },
    /// A stylus on the touch screen, in pixels: X Y [tap|hover|down|move|up|leave], or a stroke
    /// with --to X2 Y2; --button holds the barrel button, --eraser uses the eraser end. On an iPad
    /// it is the Apple Pencil: tap, down, move, up and strokes only (no hover, pressure, button or
    /// eraser in the simulator).
    #[command(alias = "pencil")]
    Stylus {
        device: String,
        x: u32,
        y: u32,
        /// tap (the default), hover, down, move, up or leave (Android).
        phase: Option<String>,
        /// Draw a stroke to this point instead.
        #[arg(long, num_args = 2, value_names = ["X2", "Y2"])]
        to: Option<Vec<u32>>,
        /// Pressure while touching, 1-255 (Android).
        #[arg(long)]
        pressure: Option<u8>,
        /// Hold the barrel button.
        #[arg(long)]
        button: bool,
        /// The eraser end.
        #[arg(long)]
        eraser: bool,
        /// The stroke's duration.
        #[arg(long, default_value_t = 300)]
        duration_ms: u64,
    },
    /// Turn a watch's crown (Wear OS rotary input, Apple Watch Digital Crown) by some ticks
    /// (negative turns back).
    #[command(allow_negative_numbers = true)]
    Rotary {
        device: String,
        ticks: i64,
        /// Apple Watch: the turn in Digital Crown units instead of ticks.
        #[arg(long)]
        delta: Option<f64>,
    },
    /// An Apple Watch hand gesture: double-tap or wrist-flick.
    WatchGesture { device: String, gesture: String },
    /// Swipe on the Siri Remote's touch surface (Apple TV): up, down, left or right.
    RemoteSwipe { device: String, direction: String },
    /// The iPad's pointer: move it to a point in pixels and hover, or click, press (down) or
    /// release (up) a button there, or drag with --to X2 Y2.
    Pointer {
        device: String,
        x: f64,
        y: f64,
        /// move (the default), click, down or up.
        action: Option<String>,
        /// primary (the default) or secondary.
        #[arg(long, default_value = "primary")]
        button: String,
        /// How long a click holds the button, in milliseconds.
        #[arg(long, default_value_t = 80)]
        hold: u64,
        /// Drag to this point instead (press at X Y, move, release).
        #[arg(long, num_args = 2, value_names = ["X2", "Y2"])]
        to: Option<Vec<f64>>,
    },
    /// Attach (`on`) or detach (`off`) an iPhone's CarPlay display (iOS 26 runtimes).
    Carplay { device: String, state: String },
    /// Android Automotive's vehicle HAL: list every property, read one, or set one, e.g.
    /// `vehicle car gear drive`, `vehicle car speed 12.5`, `vehicle car IGNITION_STATE`.
    #[command(allow_negative_numbers = true)]
    Vehicle {
        device: String,
        /// A VHAL property name (GEAR_SELECTION, or gear, speed, ignition, night, parking-brake)
        /// or id (0x11600207); none lists every property.
        property: Option<String>,
        /// Set the property to this (typed by the property: drive, 12.5, true, 2,0).
        value: Option<String>,
        /// The area (seat, door, ...) id; default 0, the global area.
        #[arg(long)]
        area: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Send commands to an Android device's emulator console (`adb emu`'s protocol).
    Console {
        device: String,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Serve the JSON-lines API on a unix socket (default `$SIMEE_SOCKET` or `$TMPDIR/simee.sock`).
    Serve {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Call an API method on a running server, e.g. `simee call processes '{"device":"booted"}'`.
    Call {
        method: String,
        /// JSON parameters.
        #[arg(default_value = "{}")]
        params: String,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Serve MCP on stdin and stdout, so an agent can do everything simee can: every API method as
    /// a tool, devices as resources, long operations as tasks, a device view as an app. Configure
    /// it in the agent's MCP settings as the command `simee mcp` (see docs/mcp.md).
    Mcp,
    /// Run a program inside the device (e.g. `defaults`, `log`, `launchctl`, `notifyutil`, `heap`).
    #[command(disable_help_flag = true)]
    Exec {
        device: String,
        /// Environment for the program, `KEY=VALUE` (repeatable).
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        program: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Cmd::Serve { socket } => {
            let path = socket.clone().unwrap_or_else(sim_api::default_socket);
            eprintln!("simee API on {}", path.display());
            sim_api::serve(&path)?;
            return Ok(());
        }
        Cmd::Mcp => {
            // Methods run in this process, on the server's own threads: nothing but protocol
            // messages may reach stdout.
            sim_mcp::serve_stdio(std::sync::Arc::new(sim_mcp::ApiBackend::new()))?;
            return Ok(());
        }
        Cmd::Call { method, params, socket } => {
            let path = socket.clone().unwrap_or_else(sim_api::default_socket);
            let params: Value = serde_json::from_str(params).context("params must be JSON")?;
            let result = sim_api::call(&path, method, params).map_err(anyhow::Error::msg)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(());
        }
        _ => {}
    }
    let in_process = cli.in_process || flag("SIMEE_IN_PROCESS");
    let mut transport = if in_process { Transport::local() } else { Transport::open(&sim_api::default_socket()) };
    transport.verbose = cli.verbose || flag("SIMEE_VERBOSE");
    transport.require = flag("SIMEE_REQUIRE_SERVER");
    let (stdout, stderr) = (std::io::stdout(), std::io::stderr());
    run(cli.command, &mut transport, &mut stdout.lock(), &mut stderr.lock())
}

/// Whether an on/off environment variable is on (set, not empty, not `0`).
fn flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

/// How long an in-process command waits for its last HID message to leave.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// Where a command's method runs: a matching server already listening, or this process.
///
/// The server is tried on the first method that may be forwarded (`sim_api::forwards`; `boot`
/// and the other methods whose child processes or state belong to the caller never are), with a
/// handshake: it must answer `identity` within `sim_api::HANDSHAKE_TIMEOUT` with this build's,
/// this executable's and this environment's identity. Otherwise (no server, a stale socket, an
/// older or newer build, a suspended or wedged server) the method runs here: silently, or with the
/// reason on stderr (`--verbose`), or not at all (`SIMEE_REQUIRE_SERVER=1`: a farm that counts on
/// the server's warm connections would rather fail than run several times slower).
struct Transport {
    /// The socket still to try.
    socket: Option<std::path::PathBuf>,
    server: Option<sim_api::Client>,
    /// Why the server isn't used, once the handshake has failed (or none was tried: in-process).
    unused: Option<String>,
    /// This process's session, made when a method first runs here.
    local: Option<Box<sim_api::Session>>,
    verbose: bool,
    require: bool,
}

impl Transport {
    fn local() -> Transport {
        Transport {
            socket: None,
            server: None,
            unused: Some("in-process was asked for".into()),
            local: None,
            verbose: false,
            require: false,
        }
    }

    /// The server on `socket` if it answers the handshake; else this process.
    fn open(socket: &std::path::Path) -> Transport {
        Transport { socket: Some(socket.to_owned()), unused: None, ..Transport::local() }
    }

    /// Whether methods that may be forwarded go to a server (handshaking first if not yet tried).
    fn connected(&mut self) -> bool {
        if let Some(socket) = self.socket.take() {
            match sim_api::Client::handshake(&socket, sim_api::HANDSHAKE_TIMEOUT) {
                Ok(client) => self.server = Some(client),
                Err(e) => self.unused = Some(format!("the server on {} can't be used: {e}", socket.display())),
            }
        }
        self.server.is_some()
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.route(method, params, false).map(|(result, _)| result)
    }

    /// [`Transport::call`] taking a result's bytes raw (a PNG), on the server (a binary reply)
    /// or here: the result and those bytes.
    fn call_binary(&mut self, method: &str, params: Value) -> Result<(Value, Option<Vec<u8>>)> {
        self.route(method, params, true)
    }

    fn route(&mut self, method: &str, params: Value, binary: bool) -> Result<(Value, Option<Vec<u8>>)> {
        let forwards = sim_api::forwards(method);
        let forward = forwards && self.connected();
        if forwards && !forward {
            let why = self.unused.as_deref().unwrap_or("no server");
            if self.require {
                anyhow::bail!("SIMEE_REQUIRE_SERVER is set and `{method}` would run in this process: {why}");
            }
            if self.verbose {
                eprintln!("simee: `{method}` runs in this process: {why}");
            }
        } else if self.verbose {
            eprintln!("simee: `{method}` runs {}", if forward { "on the server" } else { "in this process (it always does)" });
        }
        let result = match (&mut self.server, forward) {
            (Some(client), true) if binary => client.call_binary(method, &params),
            (Some(client), true) => client.call(method, &params).map(|result| (result, None)),
            _ => {
                sim_api::track_input();
                let session = self.local.get_or_insert_with(Box::default);
                match binary {
                    true => sim_api::call_method_binary(session, method, &params),
                    false => sim_api::call_method(session, method, &params).map(|result| (result, None)),
                }
            }
        };
        result.map_err(anyhow::Error::msg)
    }

    /// HID messages are delivered asynchronously: a command that ran in this process waits (up to
    /// [`FLUSH_TIMEOUT`]) for the last one to leave before the process (and its HID client)
    /// exits. A server keeps its client, so there is nothing to wait for.
    fn flush(&self) {
        if self.local.is_some() && !sim_api::flush_input(FLUSH_TIMEOUT) && self.verbose {
            eprintln!("simee: input was still being sent after {} ms", FLUSH_TIMEOUT.as_millis());
        }
    }
}

/// A path parameter made absolute here, so a server with another working directory reads and
/// writes the same file this process would.
fn absolute(path: &str) -> String {
    std::path::absolute(path).map_or_else(|_| path.to_owned(), |p| p.display().to_string())
}

/// Runs one device command and prints its result to `out` (and an in-device program's stderr to
/// `err`).
fn run(command: Cmd, transport: &mut Transport, out: &mut dyn Write, err: &mut dyn Write) -> Result<()> {
    let mut call = |method: &str, params: Value| transport.call(method, params);
    let mut input = false;
    match command {
        Cmd::List { json, booted, platform, form_factor } => {
            let devices = call("devices", json!({"booted": booted, "platform": platform, "form_factor": form_factor}))?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&devices)?)?;
            } else {
                write!(out, "{}", list_text(&serde_json::from_value::<Vec<DeviceInfo>>(devices)?))?;
            }
        }
        Cmd::FormFactors { json, platform } => {
            let rows = call("form_factors", json!({"platform": platform}))?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
            } else {
                write!(out, "{}", form_factors_text(&rows))?;
            }
        }
        Cmd::Runtimes { json } => {
            let runtimes = call("runtimes", json!({}))?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&runtimes)?)?;
            } else {
                for r in items(&runtimes) {
                    let note = if r["available"] == true { "" } else { " (unavailable)" };
                    let (name, version, build, id) = (text(&r["name"]), text(&r["version"]), text(&r["build"]), text(&r["identifier"]));
                    writeln!(out, "{name} ({version} - {build}) - {id}{note}")?;
                }
            }
        }
        Cmd::DeviceTypes { json } => {
            let types = call("device_types", json!({}))?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&types)?)?;
            } else {
                for t in items(&types) {
                    writeln!(
                        out,
                        "{} ({}) {}x{} @{}x",
                        text(&t["name"]),
                        text(&t["identifier"]),
                        number(&t["screen_width"]),
                        number(&t["screen_height"]),
                        number(&t["screen_scale"])
                    )?;
                }
            }
        }
        Cmd::Create { name, device_type, runtime, platform, system_image, form_factor, device } => {
            let mut params = json!({"name": name, "device_type": device_type, "runtime": runtime, "platform": platform,
                "system_image": system_image, "form_factor": form_factor});
            if let Some(device) = device {
                params["device"] = json!(device);
            }
            let d = call("create", params)?;
            writeln!(out, "{}", text(&d["udid"]))?;
        }
        Cmd::Delete { device } => drop(call("delete", json!({"device": device}))?),
        Cmd::Boot { device, timeout, env, shaped, settle, settle_timeout, settle_cpu, profile, audio } => {
            let env: serde_json::Map<String, Value> =
                env.iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), Value::from(v))).collect();
            let mut params = json!({"device": device, "timeout_s": timeout, "env": env, "shaped": shaped});
            // Only when asked, so `boot` sends what it always has to an Android device (which
            // rejects these with the reason).
            if settle {
                params["settle"] = json!(true);
            }
            if let Some(s) = settle_timeout {
                params["settle_timeout_s"] = json!(s);
            }
            if let Some(c) = settle_cpu {
                params["settle_foreground"] = json!(c == SettleCpu::Foreground);
            }
            if let Some(profile) = profile {
                params["profile"] = json!(profile);
            }
            if audio {
                params["audio"] = json!(true);
            }
            let timed = call("boot", params)?;
            if !timed.is_null() {
                writeln!(out, "{}", boot_line(&timed))?;
                if timed["settled"] == json!(false) {
                    writeln!(err, "{}", not_settled_line(&timed))?;
                }
            }
        }
        Cmd::BootMany { devices, concurrency, timeout, settle: _, no_settle, settle_timeout, settle_cpu, profile, first_port, json } => {
            let mut params = json!({"devices": devices, "concurrency": concurrency, "timeout_s": timeout});
            // Each only when given: the backend has the defaults (iOS settles; Android has no
            // settle and rejects iOS-only parameters).
            for (key, v) in [
                ("settle", no_settle.then_some(json!(false))),
                ("settle_timeout_s", settle_timeout.map(Value::from)),
                ("settle_foreground", settle_cpu.map(|c| json!(c == SettleCpu::Foreground))),
                ("profile", profile.map(Value::from)),
                ("first_port", first_port.map(Value::from)),
            ] {
                if let Some(v) = v {
                    params[key] = v;
                }
            }
            let r = call("boot_many", params)?;
            print_boot_many(&r, devices.len(), json, out, err)?;
        }
        Cmd::Settle { device, timeout, settle_cpu } => {
            let mut params = json!({"device": device, "timeout_s": timeout});
            if let Some(c) = settle_cpu {
                params["foreground"] = json!(c == SettleCpu::Foreground);
            }
            let r = call("settle", params)?;
            let mut line = format!("settle_s={} procs={} cpu_pct={}", r["settle_s"], r["procs"], r["cpu_pct"]);
            for key in ["foreground_cpu_pct", "background_cpu_pct"] {
                if let Some(v) = r.get(key) {
                    line.push_str(&format!(" {key}={v}"));
                }
            }
            writeln!(out, "{line}")?;
        }
        Cmd::Profile { device, name, undo } => {
            let r = call("profile", json!({"device": device, "name": name, "undo": undo}))?;
            let changed = if undo { ("enabled", &r["enabled"]) } else { ("disabled", &r["disabled"]) };
            writeln!(out, "profile={} {}={} failed={}", text(&r["name"]), changed.0, changed.1, items(&r["failed"]).len())?;
            for l in items(&r["failed"]) {
                writeln!(err, "simee: {} not {}", text(l), changed.0)?;
            }
        }
        Cmd::Clone { template, name } => {
            let d = call("clone", json!({"device": template, "name": name}))?;
            writeln!(out, "{}", text(&d["udid"]))?;
        }
        Cmd::Shutdown { device } => drop(call("shutdown", json!({"device": device}))?),
        Cmd::Erase { device } => drop(call("erase", json!({"device": device}))?),
        // To stdout: the PNG comes back raw (a binary reply from a server), never as base64.
        Cmd::Screenshot { device, path, display } if path == "-" => {
            let (_, png) = transport.call_binary("screenshot", json!({"device": device, "display": display}))?;
            out.write_all(&png.context("the screenshot came back without its PNG")?)?;
            out.flush()?;
        }
        // To a file: the process that runs the method writes it, so its bytes never cross the
        // socket.
        Cmd::Screenshot { device, path, display } => {
            drop(call("screenshot", json!({"device": device, "path": absolute(&path), "display": display}))?)
        }
        Cmd::Tap { device, x, y, hold, display } => {
            call("tap", json!({"device": device, "x": x, "y": y, "hold_ms": hold, "display": display}))?;
            input = true;
        }
        Cmd::Swipe { device, x1, y1, x2, y2, duration, settle, display, edge } => {
            let params = json!({"device": device, "x1": x1, "y1": y1, "x2": x2, "y2": y2, "duration_ms": duration, "settle": settle,
                "display": display, "edge": edge});
            call("swipe", params)?;
            input = true;
        }
        Cmd::Button { device, button, hold } => {
            call("button", json!({"device": device, "button": button, "hold_ms": hold}))?;
            input = true;
        }
        Cmd::Type { device, text } => {
            call("type", json!({"device": device, "text": text}))?;
            input = true;
        }
        Cmd::Key { device, usage } => {
            call("key", json!({"device": device, "usage": usage}))?;
            input = true;
        }
        Cmd::OpenUrl { device, url } => drop(call("open_url", json!({"device": device, "url": url}))?),
        Cmd::Install { device, path } => drop(call("install", json!({"device": device, "path": absolute(&path)}))?),
        Cmd::Uninstall { device, bundle_id } => drop(call("uninstall", json!({"device": device, "bundle_id": bundle_id}))?),
        Cmd::Launch { device, bundle_id } => {
            writeln!(out, "{}", call("launch", json!({"device": device, "bundle_id": bundle_id}))?["pid"])?
        }
        Cmd::Terminate { device, bundle_id } => drop(call("terminate", json!({"device": device, "bundle_id": bundle_id}))?),
        Cmd::Apps { device } => lines(out, &call("apps", json!({"device": device}))?)?,
        Cmd::Appearance { device, mode } => drop(call("appearance", json!({"device": device, "mode": mode}))?),
        Cmd::Location { device, latitude, longitude } => {
            drop(call("location", json!({"device": device, "latitude": latitude, "longitude": longitude}))?)
        }
        Cmd::Privacy { device, action, service, bundle_id } => {
            drop(call("privacy", json!({"device": device, "action": action, "service": service, "bundle_id": bundle_id}))?)
        }
        Cmd::MemoryWarning { device } => drop(call("memory_warning", json!({"device": device}))?),
        Cmd::Rotate { device, orientation } => {
            let r = call("rotate", json!({"device": device, "orientation": orientation}))?;
            if let Some(o) = r["orientation"].as_str() {
                writeln!(out, "{o}")?;
            }
        }
        Cmd::Fold { device, posture } => {
            let params = match (posture.as_str(), posture.parse::<f64>()) {
                // The console's `fold` and `unfold`: to the folded screen, or flat.
                ("fold" | "unfold", _) => json!({"device": device, "folded": posture == "fold"}),
                (_, Ok(angle)) => json!({"device": device, "angle": angle}),
                (_, Err(_)) => json!({"device": device, "posture": posture}),
            };
            writeln!(out, "{}", fold_line(&call("fold", params)?))?;
        }
        Cmd::Xr { device, command } => lines(out, &call("xr", xr_params(&device, &command)?)?)?,
        Cmd::Touchpad { device, x, y, phase, to, duration_ms } => {
            let params = match to.as_deref() {
                Some([tx, ty]) => json!({"device": device, "x": x, "y": y, "to_x": tx, "to_y": ty, "duration_ms": duration_ms}),
                _ => json!({"device": device, "x": x, "y": y, "phase": phase.as_deref().unwrap_or("tap")}),
            };
            call("touchpad", params)?;
        }
        Cmd::Stylus { device, x, y, phase, to, pressure, button, eraser, duration_ms } => {
            let mut params = json!({"device": device, "x": x, "y": y, "pressure": pressure, "button": button, "eraser": eraser});
            match to.as_deref() {
                Some([tx, ty]) => {
                    params["to_x"] = json!(tx);
                    params["to_y"] = json!(ty);
                    params["duration_ms"] = json!(duration_ms);
                }
                _ => params["phase"] = json!(phase.as_deref().unwrap_or("tap")),
            }
            call("stylus", params)?;
            input = true;
        }
        Cmd::Rotary { device, ticks, delta } => {
            call("rotary", json!({"device": device, "ticks": ticks, "delta": delta}))?;
            input = true;
        }
        Cmd::WatchGesture { device, gesture } => {
            call("watch_gesture", json!({"device": device, "gesture": gesture}))?;
            input = true;
        }
        Cmd::RemoteSwipe { device, direction } => {
            call("remote_swipe", json!({"device": device, "direction": direction}))?;
            input = true;
        }
        Cmd::Pointer { device, x, y, action, button, hold, to } => {
            let mut params = json!({"device": device, "x": x, "y": y, "action": action, "button": button, "hold_ms": hold});
            if let Some([tx, ty]) = to.as_deref() {
                params["to_x"] = json!(tx);
                params["to_y"] = json!(ty);
            }
            call("pointer", params)?;
            input = true;
        }
        Cmd::Carplay { device, state } => {
            let on = match state.as_str() {
                "on" => true,
                "off" => false,
                other => anyhow::bail!("`{other}`: say on or off"),
            };
            let r = call("carplay", json!({"device": device, "on": on}))?;
            if on {
                writeln!(out, "CarPlay display {}x{}", number(&r["width"]), number(&r["height"]))?;
            }
        }
        Cmd::Vehicle { device, property, value, area, json } => {
            let r = call("vehicle", json!({"device": device, "property": property, "value": value, "area_id": area}))?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&r)?)?;
            } else if !r.is_null() {
                let readings = if r.is_array() { items(&r).to_vec() } else { vec![r] };
                for v in readings {
                    writeln!(out, "{} area {}: {}", text(&v["property"]), v["area_id"], text(&v["value"]))?;
                }
            }
        }
        Cmd::Console { device, command } => lines(out, &call("console", json!({"device": device, "command": command.join(" ")}))?)?,
        Cmd::Serve { .. } | Cmd::Call { .. } | Cmd::Mcp => anyhow::bail!("`serve`, `call` and `mcp` don't run through a transport"),
        Cmd::Exec { device, env, program, args } => {
            let env: serde_json::Map<String, Value> =
                env.iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), Value::from(v))).collect();
            let result = call("exec", json!({"device": device, "program": program, "args": args, "env": env}))?;
            write!(out, "{}", text(&result["stdout"]))?;
            write!(err, "{}", text(&result["stderr"]))?;
        }
    }
    if input {
        transport.flush();
    }
    Ok(())
}

/// One device's boot timings as `key=value` words (or its error).
fn boot_line(d: &Value) -> String {
    if let Some(e) = d.get("error") {
        return format!("device={} error={}", text(&d["device"]), text(e));
    }
    let mut line = format!("device={} boot_s={}", text(&d["device"]), d["boot_s"]);
    for key in [
        "settled",
        "settle_s",
        "waited_s",
        "procs",
        "cpu_pct",
        "foreground_cpu_pct",
        "background_cpu_pct",
        "cpu_s",
        "background_cpu_s",
        "started_s",
        "done_s",
        "console_port",
        "serial",
    ] {
        if let Some(v) = d.get(key) {
            line.push_str(&format!(" {key}={}", text(v)));
        }
    }
    if let Some(p) = d.get("profile") {
        line.push_str(&format!(" profile={}", text(&p["name"])));
        match p.get("error") {
            Some(e) => line.push_str(&format!(" profile_error={}", text(e))),
            None => {
                line.push_str(&format!(" disabled={} failed={}", p["disabled"], items(&p["failed"]).len()));
                if let Some(a) = p.get("already") {
                    line.push_str(&format!(" already={a}"));
                }
                if let Some(v) = p.get("verified") {
                    line.push_str(&format!(" verified={}", text(v)));
                }
            }
        }
        line.push_str(&format!(" profile_s={}", p["took_s"]));
    }
    line
}

/// `boot-many`'s output for result `r` of `n` devices: a line per device and a summary (or the
/// JSON), a note per device that booted but didn't settle; an error only when one failed to boot.
fn print_boot_many(r: &Value, n: usize, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> Result<()> {
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(r)?)?;
    } else {
        for d in items(&r["devices"]) {
            writeln!(out, "{}", boot_line(d))?;
        }
        let unsettled = r["unsettled"].as_u64().unwrap_or(0);
        writeln!(out, "all_s={} concurrency={} failed={} unsettled={unsettled}", r["all_s"], r["concurrency"], r["failed"])?;
    }
    for d in items(&r["devices"]).iter().filter(|d| d["settled"] == json!(false)) {
        writeln!(err, "{}", not_settled_line(d))?;
    }
    if r["failed"].as_u64().unwrap_or(0) > 0 {
        anyhow::bail!("{} of {n} devices failed to boot", r["failed"]);
    }
    Ok(())
}

/// What `boot --settle` and `boot-many` say about a device that booted but didn't settle in time.
fn not_settled_line(d: &Value) -> String {
    let after = d.get("waited_s").map_or_else(String::new, |s| format!(" after {s} s"));
    format!("simee: {}: booted, not settled{after} ({})", text(&d["device"]), text(&d["settle_error"]))
}

/// Prints each element of a JSON array on its own line.
fn lines(out: &mut dyn Write, v: &Value) -> Result<()> {
    for line in items(v) {
        writeln!(out, "{}", text(line))?;
    }
    Ok(())
}

/// The elements of a JSON array (none for anything else).
fn items(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

/// A JSON string's contents; other values print as JSON.
fn text(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_owned)
}

/// A JSON number as Rust prints an `f64` (`1206`, not `1206.0`).
fn number(v: &Value) -> f64 {
    v.as_f64().unwrap_or_default()
}

/// `list` without `--json`: devices grouped under their runtime, as `simctl list` prints them, each
/// labelled with its sub-platform and form factor.
fn list_text(devices: &[DeviceInfo]) -> String {
    let mut out = String::new();
    let mut runtime = "";
    for d in devices {
        if d.runtime_id != runtime {
            runtime = &d.runtime_id;
            out.push_str(&format!("-- {runtime} --\n"));
        }
        let note = if d.available { "" } else { " (unavailable)" };
        out.push_str(&format!("    {} ({}) ({}){note} [{}]\n", d.name, d.udid, d.state.as_str(), d.label()));
    }
    out
}

/// `form-factors` without `--json`: one line per form factor and platform.
fn form_factors_text(rows: &Value) -> String {
    let mut out = String::new();
    for r in items(rows) {
        let what = if r["supported"] == true {
            let inputs: Vec<String> = items(&r["inputs"]).iter().map(|i| text(&i["label"])).collect();
            format!("{} (inputs: {})", text(&r["label"]), inputs.join(", "))
        } else {
            format!("unsupported: {}", text(&r["reason"]))
        };
        out.push_str(&format!("{:<9} {:<8} {what}\n", text(&r["form_factor"]), text(&r["platform"])));
    }
    out
}

/// `simee fold`'s line for the `fold` method's answer: the posture and angle, and the screen and
/// its size when simee's emulator (or an iPhone Duo, naming its screen in use) says them; the
/// stock emulator's answer to a fold or unfold is only which was asked.
fn fold_line(r: &Value) -> String {
    let screen = |folded: bool| if folded { "folded" } else { "unfolded" };
    match (r["posture"].as_str(), r["folded"].as_bool(), r["width"].as_u64(), r["height"].as_u64()) {
        (Some(posture), Some(folded), Some(w), Some(h)) => match r["display"].as_str() {
            Some(display) => format!("{posture} ({}°), {}, {display} {w}x{h}", r["angle"], screen(folded)),
            None => format!("{posture} ({}°), {}, {w}x{h}", r["angle"], screen(folded)),
        },
        (Some(posture), ..) => format!("{posture} ({}°)", r["angle"]),
        (None, Some(folded), ..) => screen(folded).to_owned(),
        (None, None, ..) => r.to_string(),
    }
}

/// The `xr` method's parameters for `simee xr DEVICE WORDS...` (the console's `xr` syntax).
fn xr_params(device: &str, words: &[String]) -> Result<Value> {
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let command = sim_core::xr::XrCommand::parse(&words).map_err(anyhow::Error::msg)?;
    let mut params = serde_json::to_value(command)?;
    params["device"] = Value::from(device);
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_groups_devices_by_runtime_with_labels() {
        let devices: Vec<DeviceInfo> = serde_json::from_value(json!([
            {"udid": "A", "name": "iPhone 17", "state": "Booted", "runtime_id": "rt.iOS-26-5", "device_type_id": "t", "available": true,
             "platform": "ios", "form_factor": "phone", "sub_platform": "ios"},
            {"udid": "B", "name": "iPad", "state": "Shutting Down", "runtime_id": "rt.iOS-26-5", "device_type_id": "t", "available": false,
             "platform": "ios", "form_factor": "tablet", "sub_platform": "ipados"},
            {"udid": "C", "name": "Odd", "state": "Shutdown", "runtime_id": "rt.watchOS-26-0", "device_type_id": "t", "available": true,
             "platform": "ios", "form_factor": null, "sub_platform": null},
        ]))
        .expect("devices");
        assert_eq!(
            list_text(&devices),
            "-- rt.iOS-26-5 --\n    iPhone 17 (A) (Booted) [iOS · Phone]\n    iPad (B) (Shutting Down) (unavailable) [iPadOS · Tablet]\n\
             -- rt.watchOS-26-0 --\n    Odd (C) (Shutdown) [iOS]\n"
        );
    }

    #[test]
    fn form_factors_print_support_and_reasons() {
        let mut s = sim_api::Session::default();
        let rows = sim_api::call_method(&mut s, "form_factors", &json!({"platform": "ios"})).expect("rows");
        let out = form_factors_text(&rows);
        assert!(out.contains("watch     ios      watchOS · Watch (inputs: Touch, Digital Crown, Hardware buttons)"), "{out}");
        assert!(out.contains("foldable  ios      unsupported: no Apple device"), "{out}");
        assert_eq!(out.lines().count(), 7);
    }

    #[test]
    fn numbers_print_like_f64() {
        assert_eq!(format!("{}x{} @{}x", number(&json!(1206.0)), number(&json!(2622)), number(&json!(3.0))), "1206x2622 @3x");
    }

    #[test]
    fn every_command_names_a_real_method() {
        for m in ["devices", "runtimes", "device_types", "create", "delete", "boot", "shutdown", "erase", "screenshot", "tap", "swipe"] {
            assert!(sim_api::METHODS.iter().any(|(name, ..)| *name == m), "{m}");
        }
        for m in ["button", "type", "key", "open_url", "install", "uninstall", "launch", "terminate", "apps", "appearance"] {
            assert!(sim_api::METHODS.iter().any(|(name, ..)| *name == m), "{m}");
        }
        for m in [
            "location",
            "privacy",
            "memory_warning",
            "exec",
            "rotate",
            "fold",
            "rotary",
            "console",
            "watch_gesture",
            "remote_swipe",
            "carplay",
            "xr",
            "vehicle",
            "touchpad",
            "stylus",
            "boot_many",
            "settle",
            "profile",
            "clone",
        ] {
            assert!(sim_api::METHODS.iter().any(|(name, ..)| *name == m), "{m}");
        }
    }

    #[test]
    fn fold_answers_print_what_the_emulator_said() {
        let simee = json!({"angle": 0.0, "posture": "closed", "folded": true, "width": 1080, "height": 2092});
        assert_eq!(fold_line(&simee), "closed (0.0°), folded, 1080x2092");
        // The stock emulator doesn't report the hinge: what was asked.
        assert_eq!(fold_line(&json!({"folded": true})), "folded", "not `null (null°)`");
        assert_eq!(fold_line(&json!({"folded": false})), "unfolded");
        assert_eq!(fold_line(&json!({"angle": 180.0, "posture": "opened"})), "opened (180.0°)");
        // An iPhone Duo names the screen in use.
        let duo = json!({"angle": 180.0, "posture": "opened", "folded": false, "display": "inner", "width": 2007, "height": 2853});
        assert_eq!(fold_line(&duo), "opened (180.0°), unfolded, inner 2007x2853");
    }

    #[test]
    fn xr_words_become_method_parameters() {
        let words = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            xr_params("headset", &words("rotate 0 -0.5 0")).expect("rotate"),
            json!({"device": "headset", "action": "rotate", "x": 0.0, "y": -0.5, "z": 0.0})
        );
        assert_eq!(
            xr_params("headset", &words("options night 0.25")).expect("options"),
            json!({"device": "headset", "action": "options", "environment": "night", "passthrough": 0.25})
        );
        assert!(xr_params("headset", &words("fly")).expect_err("unknown").to_string().starts_with("usage: xr recenter"));
    }

    /// An SDK with a phone image and an AVD home under a fresh temp dir; a session for it.
    fn android_root(tag: &str) -> std::path::PathBuf {
        let root = std::path::PathBuf::from("/tmp").join(format!("simee-cli-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let image = root.join("sdk/system-images/android-36/google_apis/arm64-v8a");
        std::fs::create_dir_all(&image).expect("image dir");
        std::fs::write(image.join("system.img"), b"s").expect("system.img");
        std::fs::write(image.join("source.properties"), "AndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\n").expect("props");
        root
    }

    fn android_session(root: &std::path::Path) -> sim_api::Session {
        let env = emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") };
        let mut android = sim_android::Android::new(env);
        android.running_dir = Some(root.join("running"));
        sim_api::Session::with_android(android)
    }

    /// A command's stdout, stderr and error, run through `transport`.
    fn outputs(transport: &mut Transport, args: &[&str]) -> (String, String, Option<String>) {
        let cli = Cli::try_parse_from(std::iter::once("simee").chain(args.iter().copied())).expect("arguments");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let result = run(cli.command, transport, &mut out, &mut err);
        (String::from_utf8(out).expect("utf-8"), String::from_utf8(err).expect("utf-8"), result.err().map(|e| e.to_string()))
    }

    #[test]
    fn a_server_and_this_process_print_the_same_bytes() {
        let root = android_root("forward");
        let socket = root.join("api.sock");
        let server_root = root.clone();
        sim_api::serve_in_background_with(socket.clone(), move || android_session(&server_root)).expect("serve");
        let mut local = Transport { local: Some(Box::new(android_session(&root))), ..Transport::local() };
        let mut server = Transport::open(&socket);
        assert!(server.connected(), "a listening server of this build is used");
        let image = "system-images;android-36;google_apis;arm64-v8a";
        assert_eq!(outputs(&mut local, &["create", "pixel", "--platform", "android", "--system-image", image]).0, "pixel\n");
        assert_eq!(
            outputs(&mut server, &["create", "fold", "--platform", "android", "--system-image", image, "--form-factor", "foldable"]).0,
            "fold\n"
        );
        for args in [
            &["list", "--platform", "android"][..],
            &["list", "--platform", "android", "--json"],
            &["list", "--platform", "android", "--form-factor", "foldable", "--booted"],
            &["form-factors"],
            &["form-factors", "--json", "--platform", "android"],
            &["tap", "fold", "10", "20"],
            &["screenshot", "pixel", "shot.png"],
            &["fold", "pixel", "closed"],
            &["fold", "fold", "ajar"],
            &["console", "fold", "ping"],
            &["vehicle", "pixel"],
        ] {
            let (here, there) = (outputs(&mut local, args), outputs(&mut server, args));
            assert_eq!(here, there, "simee {args:?}");
        }
        let listed = outputs(&mut server, &["list", "--platform", "android"]).0;
        assert!(listed.contains("    fold (fold) (Shutdown) [Android · Foldable]\n"), "{listed}");
        drop(server);
        std::fs::remove_file(&socket).ok();
        assert!(!Transport::open(&socket).connected(), "no server: this process");
        std::fs::remove_dir_all(root).ok();
    }

    /// A command's stdout as bytes (a PNG isn't UTF-8) and its error, run through `transport`.
    fn output_bytes(transport: &mut Transport, args: &[&str]) -> (Vec<u8>, Option<String>) {
        let cli = Cli::try_parse_from(std::iter::once("simee").chain(args.iter().copied())).expect("arguments");
        let mut out = Vec::new();
        let result = run(cli.command, transport, &mut out, &mut Vec::new());
        (out, result.err().map(|e| e.to_string()))
    }

    #[test]
    fn a_screenshot_to_stdout_is_the_png_here_and_on_a_server() {
        let root = android_root("stdout");
        let socket = root.join("api.sock");
        let server_root = root.clone();
        sim_api::serve_in_background_with(socket.clone(), move || android_session(&server_root)).expect("serve");
        let mut local = Transport { local: Some(Box::new(android_session(&root))), ..Transport::local() };
        let image = "system-images;android-36;google_apis;arm64-v8a";
        assert_eq!(outputs(&mut local, &["create", "pixel", "--platform", "android", "--system-image", image]).0, "pixel\n");
        let mut android = sim_android::Android::new(emu_avd::Env { sdk_root: Some(root.join("sdk")), avd_home: root.join("avd") });
        android.running_dir = Some(root.join("running"));
        let _emulator = sim_android::testing::fake_emulator(&android, "pixel");
        let file = root.join("shot.png");
        assert_eq!(outputs(&mut local, &["screenshot", "pixel", file.to_str().expect("path")]), (String::new(), String::new(), None));
        let png = std::fs::read(&file).expect("the file");
        assert!(png.starts_with(b"\x89PNG"), "{png:?}");
        let mut server = Transport::open(&socket);
        assert_eq!(output_bytes(&mut local, &["screenshot", "pixel", "-"]), (png.clone(), None), "in this process");
        assert_eq!(output_bytes(&mut server, &["screenshot", "pixel", "-"]), (png.clone(), None), "on the server");
        assert!(server.server.is_some(), "it was forwarded");
        // A file is still written by whichever process runs the method, the server here.
        std::fs::remove_file(&file).ok();
        assert_eq!(output_bytes(&mut server, &["screenshot", "pixel", file.to_str().expect("path")]), (Vec::new(), None));
        assert_eq!(std::fs::read(&file).ok(), Some(png));
        std::fs::remove_file(&socket).ok();
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_stale_socket_falls_back_to_this_process() {
        let root = android_root("stale");
        let socket = root.join("api.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind"));
        assert!(socket.exists(), "the socket file outlives its listener");
        let start = std::time::Instant::now();
        assert!(!Transport::open(&socket).connected());
        assert!(start.elapsed() < Duration::from_secs(1), "no wait on a stale socket");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_required_server_that_cant_be_used_fails_the_command_with_why() {
        let root = android_root("require");
        let socket = root.join("api.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind"));
        let mut transport = Transport { require: true, ..Transport::open(&socket) };
        let (out, _, err) = outputs(&mut transport, &["form-factors"]);
        assert_eq!(out, "", "nothing ran here");
        let err = err.expect("fails instead of running here");
        assert!(err.starts_with("SIMEE_REQUIRE_SERVER is set and `form_factors` would run in this process: the server on "), "{err}");
        assert!(err.contains("api.sock can't be used: "), "{err}");
        // Without the requirement the same command runs here, and the reason stays known.
        let mut transport = Transport::open(&socket);
        assert!(outputs(&mut transport, &["form-factors"]).2.is_none());
        assert!(transport.unused.as_deref().is_some_and(|u| u.contains("can't be used")), "{:?}", transport.unused);
        // A matching server satisfies it.
        let live = root.join("live.sock");
        sim_api::serve_in_background(live.clone()).expect("serve");
        let mut transport = Transport { require: true, ..Transport::open(&live) };
        assert!(outputs(&mut transport, &["form-factors"]).2.is_none());
        assert!(transport.server.is_some());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn boot_timings_print_as_words() {
        let ok = json!({"device": "U", "boot_s": 1.5, "settled": true, "settle_s": 9.0, "procs": 280, "cpu_pct": 3.2,
            "profile": {"name": "slim", "disabled": 60, "failed": [], "took_s": 4.5}});
        assert_eq!(
            boot_line(&ok),
            "device=U boot_s=1.5 settled=true settle_s=9.0 procs=280 cpu_pct=3.2 profile=slim disabled=60 failed=0 profile_s=4.5"
        );
        // The CPU split and a re-applied profile's `already` show when the server reports them.
        let split = json!({"device": "U", "boot_s": 1.5, "settled": true, "settle_s": 9.0, "procs": 117, "cpu_pct": 5.6,
            "foreground_cpu_pct": 2.6, "background_cpu_pct": 3.0,
            "profile": {"name": "slim", "disabled": 1, "already": 57, "failed": [], "took_s": 1.1}});
        assert_eq!(
            boot_line(&split),
            "device=U boot_s=1.5 settled=true settle_s=9.0 procs=117 cpu_pct=5.6 foreground_cpu_pct=2.6 background_cpu_pct=3.0 \
             profile=slim disabled=1 failed=0 already=57 profile_s=1.1"
        );
        // Read back from the overrides file launchd used: no launchctl ran.
        let held = json!({"device": "U", "boot_s": 1.5,
            "profile": {"name": "slim", "disabled": 62, "already": 62, "failed": [], "not_loaded": [], "verified": "file", "took_s": 0.0}});
        assert_eq!(boot_line(&held), "device=U boot_s=1.5 profile=slim disabled=62 failed=0 already=62 verified=file profile_s=0.0");
        assert_eq!(boot_line(&json!({"device": "V", "error": "no device matches `V`"})), "device=V error=no device matches `V`");
        let avd = json!({"device": "farm-1", "boot_s": 12.3, "console_port": 5556, "serial": "emulator-5556"});
        assert_eq!(boot_line(&avd), "device=farm-1 boot_s=12.3 console_port=5556 serial=emulator-5556");
        let broken = json!({"device": "W", "boot_s": 2.0, "profile": {"name": "slim", "error": "no launchctl", "took_s": 0.0}});
        assert_eq!(boot_line(&broken), "device=W boot_s=2.0 profile=slim profile_error=no launchctl profile_s=0.0");
    }

    #[test]
    fn a_device_that_booted_but_didnt_settle_is_not_a_boot_failure() {
        let late = json!({"device": "U", "boot_s": 3.0, "settled": false, "waited_s": 603.2, "procs": 250, "cpu_pct": 40.0,
            "settle_error": "device did not settle within 600s (40 % of a core over the last 10s)"});
        assert_eq!(
            not_settled_line(&late),
            "simee: U: booted, not settled after 603.2 s (device did not settle within 600s (40 % of a core over the last 10s))"
        );
        let r = json!({"concurrency": 2, "all_s": 610.0, "failed": 0, "unsettled": 1,
            "devices": [late, {"device": "V", "boot_s": 2.0, "settled": true, "settle_s": 80.0}]});
        let (mut out, mut err) = (Vec::new(), Vec::new());
        print_boot_many(&r, 2, false, &mut out, &mut err).expect("unsettled exits 0");
        let out = String::from_utf8(out).expect("utf-8");
        assert!(out.ends_with("all_s=610.0 concurrency=2 failed=0 unsettled=1\n"), "{out}");
        assert!(String::from_utf8(err).expect("utf-8").contains("U: booted, not settled after 603.2 s"));
        let mut failed = r.clone();
        failed["failed"] = json!(1);
        let e = print_boot_many(&failed, 2, true, &mut Vec::new(), &mut Vec::new()).expect_err("a failed boot fails");
        assert_eq!(e.to_string(), "1 of 2 devices failed to boot");
    }

    #[test]
    fn mcp_is_a_subcommand_that_runs_no_device_command() {
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("simee").chain(args.iter().copied())).map(|c| c.command);
        assert!(matches!(parse(&["mcp"]), Ok(Cmd::Mcp)));
        assert!(parse(&["mcp", "--http"]).is_err(), "stdio only");
        let mut transport = Transport::local();
        let e = run(Cmd::Mcp, &mut transport, &mut Vec::new(), &mut Vec::new()).expect_err("served by main, not run");
        assert!(e.to_string().contains("`mcp` don't run through a transport"));
    }

    #[test]
    fn boot_many_sends_only_what_was_asked() {
        // `--settle` is the default now (kept for scripts); `--no-settle` turns it off.
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("simee").chain(args.iter().copied())).map(|c| c.command);
        assert!(matches!(parse(&["boot-many", "a", "--settle"]), Ok(Cmd::BootMany { no_settle: false, .. })));
        assert!(matches!(parse(&["boot-many", "a", "--no-settle"]), Ok(Cmd::BootMany { no_settle: true, .. })));
        assert!(matches!(parse(&["boot-many", "a"]), Ok(Cmd::BootMany { settle_cpu: None, .. })), "the API's default");
        assert!(matches!(parse(&["boot-many", "a", "--settle-cpu", "all"]), Ok(Cmd::BootMany { settle_cpu: Some(SettleCpu::All), .. })));
        assert!(matches!(
            parse(&["boot", "a", "--settle", "--settle-cpu", "foreground"]),
            Ok(Cmd::Boot { settle_cpu: Some(SettleCpu::Foreground), .. })
        ));
        assert!(parse(&["boot", "a", "--settle-cpu", "background"]).is_err());
        assert!(matches!(parse(&["settle", "a"]), Ok(Cmd::Settle { settle_cpu: None, .. })), "the API's default");
        assert!(matches!(parse(&["settle", "a", "--settle-cpu", "all"]), Ok(Cmd::Settle { settle_cpu: Some(SettleCpu::All), .. })));
        assert!(parse(&["settle", "a", "--settle-cpu", "background"]).is_err());
        assert!(parse(&["boot-many", "a", "--settle", "--no-settle"]).is_err());
        assert!(matches!(parse(&["boot-many", "a", "--first-port", "5600"]), Ok(Cmd::BootMany { first_port: Some(5600), .. })));
        // Android devices boot silent unless `--audio`.
        assert!(matches!(parse(&["boot", "a"]), Ok(Cmd::Boot { audio: false, .. })));
        assert!(matches!(parse(&["boot", "a", "--audio"]), Ok(Cmd::Boot { audio: true, .. })));
    }

    #[test]
    fn pencil_is_the_stylus_command() {
        let parse = |args: &[&str]| Cli::try_parse_from(std::iter::once("simee").chain(args.iter().copied())).map(|c| c.command);
        assert!(matches!(parse(&["pencil", "iPad", "1", "2", "--to", "3", "4"]), Ok(Cmd::Stylus { to: Some(_), .. })));
        assert!(matches!(parse(&["stylus", "iPad", "1", "2", "down"]), Ok(Cmd::Stylus { button: false, pressure: None, .. })));
    }

    /// A fake server's reply to a method: its result or its error.
    type Answer = fn(&str) -> std::result::Result<Value, String>;

    /// A server on `socket` that records each method it is sent and answers with `answer`; `None`
    /// never answers (a wedged server).
    fn fake_server(socket: &std::path::Path, answer: Option<Answer>) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        use std::io::BufRead;
        let listener = std::os::unix::net::UnixListener::bind(socket).expect("bind");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let record = record.clone();
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().expect("clone");
                    for line in std::io::BufReader::new(stream).lines().map_while(Result::ok) {
                        let req: Value = serde_json::from_str(&line).expect("request");
                        let method = req["method"].as_str().unwrap_or("").to_owned();
                        record.lock().expect("lock").push(method.clone());
                        match answer {
                            Some(answer) => {
                                let reply = match answer(&method) {
                                    Ok(result) => json!({"id": req["id"], "ok": true, "result": result}),
                                    Err(error) => json!({"id": req["id"], "ok": false, "error": error}),
                                };
                                writeln!(writer, "{reply}").expect("write");
                            }
                            None => std::thread::park(),
                        }
                    }
                });
            }
        });
        seen
    }

    #[test]
    fn a_server_that_never_answers_runs_the_command_here() {
        let root = android_root("wedged");
        let socket = root.join("api.sock");
        let seen = fake_server(&socket, None);
        let mut local = Transport::local();
        let expected = outputs(&mut local, &["form-factors"]);
        let start = std::time::Instant::now();
        let mut transport = Transport::open(&socket);
        assert_eq!(outputs(&mut transport, &["form-factors"]), expected);
        assert!(start.elapsed() < Duration::from_millis(1500), "{:?}", start.elapsed());
        assert!(!transport.connected());
        assert_eq!(*seen.lock().expect("lock"), ["identity"], "only the handshake reached it");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn another_build_or_an_old_server_runs_the_command_here() {
        let root = android_root("skew");
        let mut local = Transport::local();
        let expected = outputs(&mut local, &["form-factors", "--json"]);
        // An older build has no `identity` (it answers an error); another build answers another one.
        let old: Answer = |m| Err(format!("unknown method `{m}`; call `help`"));
        let other: Answer = |_| Ok(json!({"build": "0.0.0+0", "exe": "elsewhere", "env": "0"}));
        for (tag, answer) in [("old", old), ("other", other)] {
            let socket = root.join(format!("{tag}.sock"));
            let seen = fake_server(&socket, Some(answer));
            let mut transport = Transport::open(&socket);
            assert_eq!(outputs(&mut transport, &["form-factors", "--json"]), expected, "{tag}");
            assert_eq!(*seen.lock().expect("lock"), ["identity"], "{tag}: nothing forwarded");
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn an_in_process_command_waits_only_for_input_still_being_sent() {
        let root = android_root("flush");
        // An Android command's input has left by the time it returns, and no HID send is pending.
        let local = Transport { local: Some(Box::new(android_session(&root))), ..Transport::local() };
        let start = std::time::Instant::now();
        local.flush();
        assert!(start.elapsed() < Duration::from_millis(50), "no fixed pause: {:?}", start.elapsed());
        // A command that never ran here has nothing to wait for.
        let start = std::time::Instant::now();
        Transport::open(&root.join("none.sock")).flush();
        assert!(start.elapsed() < Duration::from_millis(50), "{:?}", start.elapsed());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn boot_runs_here_even_with_a_matching_server() {
        let root = android_root("boot");
        let socket = root.join("api.sock");
        let seen = fake_server(&socket, Some(|m| Ok(if m == "identity" { sim_api::identity().clone() } else { json!([]) })));
        let mut transport = Transport { local: Some(Box::new(android_session(&root))), ..Transport::open(&socket) };
        // Resolved and rejected by this process's (empty) AVD home: nothing is started.
        let err = transport.call("boot", json!({"device": "nope", "platform": "android"})).expect_err("no such AVD");
        assert_eq!(err.to_string(), "no device matches `nope`");
        assert!(seen.lock().expect("lock").is_empty(), "boot doesn't even handshake");
        assert_eq!(transport.call("devices", json!({"platform": "android"})).expect("devices"), json!([]));
        assert_eq!(*seen.lock().expect("lock"), ["identity", "devices"]);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn ios_only_boot_flags_are_refused_on_android_with_the_reason() {
        let root = android_root("boot-flags");
        let mut local = Transport { local: Some(Box::new(android_session(&root))), ..Transport::local() };
        let image = "system-images;android-36;google_apis;arm64-v8a";
        assert_eq!(outputs(&mut local, &["create", "pixel", "--platform", "android", "--system-image", image]).0, "pixel\n");
        for (flags, key) in [(&["--settle"][..], "settle"), (&["--profile", "slim"], "profile"), (&["--shaped"], "shaped")] {
            let args: Vec<&str> = ["boot", "pixel"].into_iter().chain(flags.iter().copied()).collect();
            let (out, _, err) = outputs(&mut local, &args);
            assert_eq!(out, "");
            let err = err.expect("refused");
            assert!(err.starts_with(&format!("`boot` takes `{key}` for iOS devices only (")), "{err}");
        }
        let (_, _, err) = outputs(&mut local, &["boot-many", "pixel", "--profile", "slim"]);
        assert!(err.expect("refused").starts_with("`boot_many` takes `profile` for iOS devices only"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn path_parameters_are_made_absolute() {
        let here = std::env::current_dir().expect("cwd");
        assert_eq!(absolute("shot.png"), here.join("shot.png").display().to_string());
        assert_eq!(absolute("/tmp/x.png"), "/tmp/x.png");
    }

    #[test]
    fn android_xr_is_listed_as_a_vision_form_factor() {
        let mut s = sim_api::Session::default();
        let rows = sim_api::call_method(&mut s, "form_factors", &json!({"platform": "android"})).expect("rows");
        let out = form_factors_text(&rows);
        assert!(out.contains("vision    android  Android XR · Vision (inputs: Eye tracking, Hand tracking, Keyboard)"), "{out}");
        assert!(!out.contains("unsupported"), "{out}");
    }
}
