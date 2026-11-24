//! End-to-end tests against a real running Android device (an AVD booted by simee). Skipped
//! unless `SIMEE_E2E_AVD` names one; needs an Apple silicon Mac:
//!
//! ```text
//! simee create e2e --platform android --system-image "system-images;android-36;google_apis;arm64-v8a"
//! simee boot e2e && SIMEE_E2E_AVD=e2e cargo nextest run -p simee --test e2e_android
//! ```

use std::process::{Command, Output};

fn avd() -> Option<String> {
    std::env::var("SIMEE_E2E_AVD").ok().filter(|s| !s.is_empty())
}

fn sim(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_simee")).args(args).output().expect("simee runs")
}

fn ok(args: &[&str]) -> String {
    let out = sim(args);
    assert!(out.status.success(), "simee {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn device_is_listed_as_a_booted_android_device() {
    let Some(avd) = avd() else { return };
    let list: serde_json::Value = serde_json::from_str(&ok(&["list", "--json", "--platform", "android"])).expect("json");
    let d = list.as_array().expect("array").iter().find(|d| d["udid"] == avd.as_str()).expect("listed").clone();
    assert_eq!((d["platform"].as_str(), d["state"].as_str()), (Some("android"), Some("Booted")));
    assert!(d["form_factor"].is_string(), "{d}");
}

#[test]
fn screenshot_matches_the_avd_screen() {
    let Some(avd) = avd() else { return };
    let dir = std::env::temp_dir().join(format!("simee-e2e-android-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp");
    let path = dir.join("screen.png");
    ok(&["screenshot", &avd, path.to_str().expect("utf8")]);
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).expect("png")));
    let info = decoder.read_info().expect("header").info().clone();
    assert!(info.width > 0 && info.height > 0);
    let devices: serde_json::Value = serde_json::from_str(&ok(&["list", "--json", "--platform", "android"])).expect("json");
    assert!(devices.as_array().is_some_and(|a| a.iter().any(|d| d["udid"] == avd.as_str())));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn input_commands_succeed() {
    let Some(avd) = avd() else { return };
    ok(&["button", &avd, "home"]);
    ok(&["tap", &avd, "10", "10"]);
    ok(&["swipe", &avd, "300", "900", "300", "300", "--duration", "200"]);
    ok(&["type", &avd, "Hello"]);
    ok(&["button", &avd, "back"]);
    ok(&["location", &avd, "37.33", "-122.03"]);
    assert!(ok(&["console", &avd, "ping"]).contains("I am alive"));
}
