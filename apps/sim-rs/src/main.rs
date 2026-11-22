//! sim-rs: drive simulated iOS devices without the stock simulator app.

use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use sim_core::device::{DeviceState, privacy_service};
use sim_core::geometry::{Point, Size};
use sim_core::gesture::{self, Sample};
use sim_core::hid::{Button, KEY_LEFT_SHIFT, keystroke};
use sim_coresim::{Appearance, Context, Device, Direction, Frame, Hid};

#[derive(Parser)]
#[command(name = "sim-rs", version, about = "Create, boot and drive simulated iOS devices")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
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
    /// Create a device; prints its UDID.
    Create { name: String, device_type: String, runtime: String },
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
    },
    /// Shut a device down.
    Shutdown { device: String },
    /// Erase all content and settings (device must be shut down).
    Erase { device: String },
    /// Save the screen as a PNG.
    Screenshot { device: String, path: String },
    /// Tap at a point, in pixels from the top-left of the screen.
    Tap {
        device: String,
        x: f64,
        y: f64,
        /// Hold time in milliseconds.
        #[arg(long, default_value_t = 60)]
        hold: u64,
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
    },
    /// Press a hardware button: home, lock, side, siri, apple-pay.
    Button {
        device: String,
        button: String,
        /// Hold time in milliseconds.
        #[arg(long, default_value_t = 80)]
        hold: u64,
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
    /// Serve the JSON-lines API on a unix socket (default `$SIM_RS_SOCKET` or `$TMPDIR/sim-rs.sock`).
    Serve {
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Call an API method on a running server, e.g. `sim-rs call processes '{"device":"booted"}'`.
    Call {
        method: String,
        /// JSON parameters.
        #[arg(default_value = "{}")]
        params: String,
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
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
            eprintln!("sim-rs API on {}", path.display());
            sim_api::serve(&path)?;
            return Ok(());
        }
        Cmd::Call { method, params, socket } => {
            let path = socket.clone().unwrap_or_else(sim_api::default_socket);
            let params: serde_json::Value = serde_json::from_str(params).context("params must be JSON")?;
            let result = sim_api::call(&path, method, params).map_err(anyhow::Error::msg)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(());
        }
        _ => {}
    }
    let ctx = Context::new()?;
    let device = |q: &str| ctx.device(q).map_err(anyhow::Error::from);
    match cli.command {
        Cmd::List { json, booted } => {
            let infos: Vec<_> = ctx.devices()?.iter().map(Device::info).filter(|d| !booted || d.state == DeviceState::Booted).collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&infos)?);
            } else {
                let mut runtime = String::new();
                for d in infos {
                    if d.runtime_id != runtime {
                        runtime.clone_from(&d.runtime_id);
                        println!("-- {runtime} --");
                    }
                    let note = if d.available { "" } else { " (unavailable)" };
                    println!("    {} ({}) ({}){note}", d.name, d.udid, d.state.as_str());
                }
            }
        }
        Cmd::Runtimes { json } => {
            let runtimes = ctx.runtimes()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&runtimes)?);
            } else {
                for r in runtimes {
                    println!(
                        "{} ({} - {}) - {}{}",
                        r.name,
                        r.version,
                        r.build,
                        r.identifier,
                        if r.available { "" } else { " (unavailable)" }
                    );
                }
            }
        }
        Cmd::DeviceTypes { json } => {
            let types = ctx.device_types()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&types)?);
            } else {
                for t in types {
                    println!("{} ({}) {}x{} @{}x", t.name, t.identifier, t.screen_width, t.screen_height, t.screen_scale);
                }
            }
        }
        Cmd::Create { name, device_type, runtime } => println!("{}", ctx.create(&name, &device_type, &runtime)?.info().udid),
        Cmd::Delete { device: q } => ctx.delete(&device(&q)?)?,
        Cmd::Boot { device: q, timeout, env, shaped } => {
            let mut env: Vec<(String, String)> =
                env.iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
            let d = device(&q)?;
            if shaped {
                let dylib = sim_netshape::dylib().map_err(anyhow::Error::msg)?;
                env.extend(sim_netshape::launch_env(&d.info().udid, &dylib));
            }
            if d.state() != DeviceState::Booted && !env.is_empty() {
                d.boot_with_env(&env)?;
            }
            boot(&d, Duration::from_secs(timeout))?
        }
        Cmd::Shutdown { device: q } => device(&q)?.shutdown()?,
        Cmd::Erase { device: q } => device(&q)?.erase()?,
        Cmd::Screenshot { device: q, path } => write_png(&device(&q)?.display()?.capture()?, &path)?,
        Cmd::Tap { device: q, x, y, hold } => {
            let d = device(&q)?;
            play(&d.hid()?, screen(&d)?, &gesture::tap(Point::new(x, y), Duration::from_millis(hold)))?;
        }
        Cmd::Swipe { device: q, x1, y1, x2, y2, duration, settle } => {
            let d = device(&q)?;
            let samples =
                gesture::swipe(Point::new(x1, y1), Point::new(x2, y2), Duration::from_millis(duration), Duration::from_millis(17), settle);
            play(&d.hid()?, screen(&d)?, &samples)?;
        }
        Cmd::Button { device: q, button, hold } => {
            let button = Button::parse(&button).with_context(|| format!("unknown button `{button}`"))?;
            let hid = device(&q)?.hid()?;
            hid.button(button, Direction::Down)?;
            sleep(Duration::from_millis(hold));
            hid.button(button, Direction::Up)?;
            flush();
        }
        Cmd::Type { device: q, text } => type_text(&device(&q)?.hid()?, &text)?,
        Cmd::Key { device: q, usage } => {
            let hid = device(&q)?.hid()?;
            press(&hid, usage)?;
            flush();
        }
        Cmd::OpenUrl { device: q, url } => device(&q)?.open_url(&url)?,
        Cmd::Install { device: q, path } => device(&q)?.install(&path)?,
        Cmd::Uninstall { device: q, bundle_id } => device(&q)?.uninstall(&bundle_id)?,
        Cmd::Launch { device: q, bundle_id } => println!("{}", device(&q)?.launch(&bundle_id)?),
        Cmd::Terminate { device: q, bundle_id } => device(&q)?.terminate(&bundle_id)?,
        Cmd::Apps { device: q } => device(&q)?.installed_apps()?.iter().for_each(|id| println!("{id}")),
        Cmd::Appearance { device: q, mode } => {
            let appearance = match mode.as_str() {
                "light" => Appearance::Light,
                "dark" => Appearance::Dark,
                other => bail!("unknown appearance `{other}` (light or dark)"),
            };
            device(&q)?.set_appearance(appearance)?;
        }
        Cmd::Location { device: q, latitude, longitude } => device(&q)?.set_location(latitude, longitude)?,
        Cmd::Privacy { device: q, action, service, bundle_id } => {
            let granted = match action.as_str() {
                "grant" => true,
                "revoke" => false,
                other => bail!("unknown action `{other}` (grant or revoke)"),
            };
            let service = privacy_service(&service).with_context(|| format!("unknown privacy service `{service}`"))?;
            device(&q)?.set_privacy(&service, &bundle_id, granted)?;
        }
        Cmd::MemoryWarning { device: q } => device(&q)?.memory_warning()?,
        Cmd::Serve { .. } | Cmd::Call { .. } => unreachable!("handled above"),
        Cmd::Exec { device: q, env, program, args } => {
            let env: Vec<(String, String)> =
                env.iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
            let out = device(&q)?.exec(&program, &args, &env)?;
            print!("{}", out.stdout);
            eprint!("{}", out.stderr);
        }
    }
    Ok(())
}

/// Boots (if needed) and waits for the display to produce a frame.
fn boot(device: &Device, timeout: Duration) -> Result<()> {
    if device.state() != DeviceState::Booted {
        device.boot()?;
    }
    let deadline = Instant::now() + timeout;
    loop {
        let finished = device.state() == DeviceState::Booted && device.boot_finished().unwrap_or(true);
        if finished && device.display().is_ok_and(|d| d.surface().is_some()) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("device did not finish booting within {}s", timeout.as_secs());
        }
        sleep(Duration::from_millis(100));
    }
}

/// The device's screen size in pixels.
fn screen(device: &Device) -> Result<Size> {
    let (w, h) = device.display()?.size()?;
    Ok(Size::new(f64::from(w), f64::from(h)))
}

/// Plays touch samples given in pixels.
fn play(hid: &Hid, screen: Size, samples: &[Sample]) -> Result<()> {
    for s in samples {
        sleep(s.delay);
        hid.touch(screen.normalize(s.point), s.phase)?;
    }
    flush();
    Ok(())
}

fn press(hid: &Hid, usage: u32) -> Result<()> {
    hid.key(usage, Direction::Down)?;
    sleep(Duration::from_millis(15));
    hid.key(usage, Direction::Up)?;
    sleep(Duration::from_millis(15));
    Ok(())
}

fn type_text(hid: &Hid, text: &str) -> Result<()> {
    for c in text.chars() {
        let stroke = keystroke(c).with_context(|| format!("no key for {c:?} on the US layout"))?;
        if stroke.shift {
            hid.key(KEY_LEFT_SHIFT, Direction::Down)?;
            sleep(Duration::from_millis(10));
        }
        press(hid, stroke.usage)?;
        if stroke.shift {
            hid.key(KEY_LEFT_SHIFT, Direction::Up)?;
            sleep(Duration::from_millis(10));
        }
    }
    flush();
    Ok(())
}

/// HID messages are delivered asynchronously; give the last one time to leave before exiting.
fn flush() {
    sleep(Duration::from_millis(100));
}

fn write_png(frame: &Frame, path: &str) -> Result<()> {
    let png = sim_api::encode_png(frame).map_err(anyhow::Error::msg)?;
    std::fs::write(path, png).with_context(|| format!("cannot write {path}"))
}
