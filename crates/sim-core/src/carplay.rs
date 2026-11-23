//! CarPlay on an iPhone simulator: the car display's configuration and which runtimes show it.
//!
//! CarPlay isn't a device type. The iPhone simulator's external (TVOut) screen becomes the car
//! display when the device's `extended_display.plist` asks for it ("StarkConfig") and the screen's
//! mode is set with CarPlay properties; touches then go to a CarPlay HID service.

/// The car display Simulator.app attaches by default: 800×480 at scale 2, touch only.
pub const WIDTH: u32 = 800;
pub const HEIGHT: u32 = 480;
pub const SCALE: f64 = 2.0;

/// The StarkConfig file, relative to the device's data directory. CoreSimulator resets it to
/// `{stark: 0}` on every boot; the guest re-reads it whenever the TVOut screen's mode changes.
pub const CONFIG_PATH: &str = "Library/Application Support/Simulator/extended_display.plist";

/// The StarkConfig plist (XML) that turns the car display on, or off.
pub fn config_plist(on: bool) -> String {
    let body = if on {
        format!(
            "\t<key>hasBackButton</key>\n\t<false/>\n\t<key>hasHomeButton</key>\n\t<false/>\n\t<key>hasTouchScreen</key>\n\t<true/>\n\
             \t<key>hasTouchScreenLofi</key>\n\t<false/>\n\t<key>hasWheel</key>\n\t<false/>\n\t<key>hasWheelBump</key>\n\t<false/>\n\
             \t<key>height</key>\n\t<integer>{HEIGHT}</integer>\n\t<key>protocols</key>\n\t<array/>\n\t<key>scale</key>\n\t<real>{SCALE:.1}</real>\n\
             \t<key>stark</key>\n\t<true/>\n\t<key>width</key>\n\t<integer>{WIDTH}</integer>\n"
        )
    } else {
        "\t<key>stark</key>\n\t<integer>0</integer>\n".to_owned()
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n{body}</dict>\n</plist>\n"
    )
}

/// Whether an iPhone on this runtime (`com.apple.CoreSimulator.SimRuntime.iOS-26-5`) can show
/// CarPlay. iOS 27 runtimes render it black (Apple: "iOS Simulator is not supported for CarPlay").
pub fn runtime_supports(runtime_id: &str) -> Result<(), String> {
    let name = runtime_id.rsplit('.').next().unwrap_or(runtime_id);
    let version = name.strip_prefix("iOS-").ok_or_else(|| format!("CarPlay needs an iOS runtime; `{runtime_id}` isn't one"))?;
    let major: u32 =
        version.split('-').next().and_then(|m| m.parse().ok()).ok_or_else(|| format!("can't read the iOS version of `{runtime_id}`"))?;
    if major >= 27 {
        return Err(format!("CarPlay isn't supported on iOS {major} simulator runtimes (Apple renders it black); use an iOS 26 runtime"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_turns_the_car_display_on_and_off() {
        let on = config_plist(true);
        assert!(on.starts_with("<?xml"));
        assert!(on.contains("<key>stark</key>\n\t<true/>"));
        assert!(on.contains("<key>width</key>\n\t<integer>800</integer>"));
        assert!(on.contains("<key>height</key>\n\t<integer>480</integer>"));
        assert!(on.contains("<key>scale</key>\n\t<real>2.0</real>"));
        assert!(on.contains("<key>hasTouchScreen</key>\n\t<true/>"));
        assert!(on.contains("<key>protocols</key>\n\t<array/>"));
        let off = config_plist(false);
        assert!(off.contains("<key>stark</key>\n\t<integer>0</integer>") && !off.contains("width"));
        // Keys of a plist dict are written sorted, as PropertyListSerialization does.
        let keys: Vec<&str> = on.lines().filter_map(|l| l.trim().strip_prefix("<key>")?.strip_suffix("</key>")).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn only_ios_26_and_earlier_runtimes_show_carplay() {
        let rt = "com.apple.CoreSimulator.SimRuntime.";
        assert_eq!(runtime_supports(&format!("{rt}iOS-26-5")), Ok(()));
        assert_eq!(runtime_supports(&format!("{rt}iOS-18-6")), Ok(()));
        assert!(runtime_supports(&format!("{rt}iOS-27-0")).expect_err("27").contains("iOS 27"));
        assert!(runtime_supports(&format!("{rt}watchOS-26-0")).expect_err("watch").contains("needs an iOS runtime"));
        assert!(runtime_supports(&format!("{rt}iOS-x")).is_err());
    }
}
