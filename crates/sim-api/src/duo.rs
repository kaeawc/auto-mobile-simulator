//! `fold` on iOS: the iPhone Duo's hinge ([`sim_core::duo`]). An in-device program
//! (`native/displaytool.m`, built on first use and cached) asks SpringBoard to sweep its synthetic
//! hinge to the angle asked for; SpringBoard moves the content to the other screen when the sweep
//! crosses its swap boundary, and the answer says which screen shows it then.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sim_core::duo::{self, Screen};
use sim_coresim::{Device, Display};

use crate::params::{err, opt_bool, str_param};

/// The in-device display tool's source.
const SOURCE: &str = include_str!("../native/displaytool.m");
/// The entitlement SpringBoard's display tool service asks its clients for, linked into the tool.
const ENTITLEMENTS: &str = include_str!("../native/displaytool.entitlements");
/// How long the content may take to land on the other screen after SpringBoard's reply.
const SETTLE: Duration = Duration::from_secs(3);

/// The display tool for the device's simulator SDK, built on first use.
fn tool(device: &Device) -> Result<PathBuf, String> {
    let sdk = sim_netshape::Sdk::for_runtime(&device.info().runtime_id)?;
    let dir = sim_netshape::cache_dir();
    std::fs::create_dir_all(&dir).map_err(err)?;
    let plist = dir.join("displaytool.entitlements");
    std::fs::write(&plist, ENTITLEMENTS).map_err(|e| format!("{}: {e}", plist.display()))?;
    let section = format!("-Wl,-sectcreate,__TEXT,__entitlements,{}", plist.display());
    // The entitlements are part of the build: other ones make another tool.
    let source = format!("{SOURCE}\n/* entitlements:\n{ENTITLEMENTS}*/\n");
    sim_netshape::objc_tool(sdk, "displaytool", &source, &["-framework", "Foundation", &section])
}

/// `fold` for an iPhone Duo: `folded`, an `angle` (0-180) or a `posture` (closed, half-opened,
/// opened). Answers with the angle and posture, whether the device shows its outer screen, and
/// the screen in use (`display`) with its size, the space tap, swipe and touch then take.
pub fn fold(device: &Device, p: &Value) -> Result<Value, String> {
    let folded = opt_bool(p, "folded")?;
    let angle = duo::hinge_angle(folded, p.get("angle").and_then(Value::as_f64), str_param(p, "posture"))?;
    let info = device.info();
    let (outer, inner) = device.builtin_displays().map_err(err)?;
    let Some(inner) = inner else {
        return Err(format!("folding needs an iPhone Duo (two built-in screens); {} is {}", info.name, info.label()));
    };
    let tool = tool(device)?;
    let out = device.exec(&tool.display().to_string(), &duo::swap_args(angle), &[]).map_err(err)?;
    let said = out.stdout.trim();
    if said != "swap ok" {
        return Err(format!("fold: {}", if said.is_empty() { out.stderr.trim() } else { said }));
    }
    let screen = settle(&outer, &inner);
    if folded.is_some_and(|f| f != (screen == Screen::Outer)) {
        return Err(format!(
            "SpringBoard swept the hinge but the {} screen still shows the device; it may not serve simee's display tool on {}",
            screen.as_str(),
            info.runtime_id
        ));
    }
    let shown = if screen == Screen::Inner { &inner } else { &outer };
    let (width, height) = shown.size().map_err(err)?;
    Ok(json!({
        "angle": angle,
        "posture": duo::posture(angle),
        "folded": screen == Screen::Outer,
        "display": screen.as_str(),
        "width": width,
        "height": height,
    }))
}

/// The screen in use once the swap has landed: as soon as just one screen is lit, else (both dark
/// or both lit after [`SETTLE`]) what [`duo::active`] says.
fn settle(outer: &Display, inner: &Display) -> Screen {
    let deadline = Instant::now() + SETTLE;
    loop {
        let (o, i) = (outer.lit(), inner.lit());
        if o != i || Instant::now() >= deadline {
            return duo::active(o, i);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
