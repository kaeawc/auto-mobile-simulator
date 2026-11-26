//! A foldable's hinge, as stock's `FoldableModel` drives it: the console's `fold`, `unfold` and
//! `posture <id>` (and `sensor set hinge-angle0`) move the hinge angle the guest's sensors HAL
//! reports, the posture follows from the angle, and at the AVD's fold posture the device shows
//! only its folded screen (`hw.displayRegion.0.1.*`: a fold-in's outer screen, one side of a
//! dual-screen).
//!
//! Stock gives a Pixel Fold its outer screen as a second display; simee's board has one, so a
//! folded device's display is forced to the folded screen's size (`wm size`), and readers of the
//! scanout (the display socket, screenshots) see only where the guest then draws it
//! ([`FrameStore::set_view`]; touches on it land there too).
//!
//! What the guest is told besides, as stock tells it: an image that follows the hinge with its
//! own device states (a Pixel Fold's `data/misc/pixel_fold`, sent at boot as
//! `androidboot.qemu.device_state`) gets nothing more; any other foldable gets the
//! `device_posture` and `display_features` global settings Jetpack WindowManager's emulator
//! support reads, over adb, as stock sends them.
//!
//! Builds and tests on any host; only an Apple silicon boot serves it.
use std::sync::{Arc, Mutex};

use emu_vmm::display::FrameStore;
use sim_core::fold::{Hinge, Rect, StockPosture};

/// Runs shell commands in the guest, in order (stock's `AdbInterface`); fire and forget.
pub type GuestShell = Box<dyn Fn(Vec<String>) + Send + Sync>;

pub struct Foldable {
    hinge: Hinge,
    /// The unfolded screen.
    screen: (u32, u32),
    angle: Mutex<f32>,
    sensors: Arc<emu_vmm::pipe::sensors::Sensors>,
    /// Whether the guest follows the hinge through its own device states (a Pixel Fold image):
    /// stock then sends it no posture ("no need to send posture to pixel_fold device").
    guest_device_states: bool,
    shell: Option<GuestShell>,
    display: Option<Arc<FrameStore>>,
    /// Sent once the guest has booted, before its posture: the device's skin overlays.
    boot_commands: Vec<String>,
}

impl Foldable {
    /// The hinge `hinge` on a `screen` (width, height); the hinge sensor starts at its default.
    pub fn new(hinge: Hinge, screen: (u32, u32), sensors: Arc<emu_vmm::pipe::sensors::Sensors>, guest_device_states: bool) -> Foldable {
        let angle = hinge.default;
        sensors.set("hinge-angle0", vec![angle]);
        Foldable {
            hinge,
            screen,
            angle: Mutex::new(angle),
            sensors,
            guest_device_states,
            shell: None,
            display: None,
            boot_commands: Vec::new(),
        }
    }

    /// Commands the guest gets once it has booted, before its posture.
    pub fn with_boot_commands(mut self, commands: Vec<String>) -> Foldable {
        self.boot_commands = commands;
        self
    }

    /// Commands the guest settings go through.
    pub fn with_shell(mut self, shell: GuestShell) -> Foldable {
        self.shell = Some(shell);
        self
    }

    /// The scanout whose readers see only the folded screen while folded.
    pub fn with_display(mut self, display: Option<Arc<FrameStore>>) -> Foldable {
        self.display = display;
        self.apply_view();
        self
    }

    pub fn hinge(&self) -> &Hinge {
        &self.hinge
    }

    pub fn angle(&self) -> f32 {
        *self.angle.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn posture(&self) -> StockPosture {
        self.hinge.posture_at(self.angle())
    }

    pub fn folded(&self) -> bool {
        self.hinge.folded_at(self.angle())
    }

    /// Where on the scanout the device shows now: the folded screen while folded, else all of it.
    pub fn visible(&self) -> Rect {
        self.hinge.visible(self.angle(), self.screen)
    }

    /// The display size the guest lays out for now.
    pub fn logical_size(&self) -> (u32, u32) {
        self.hinge.logical_size(self.angle(), self.screen)
    }

    /// Moves the hinge to `angle` (within its range): the sensor, then, if the posture changed,
    /// the screen and the guest's settings.
    pub fn set_angle(&self, angle: f32) -> Result<StockPosture, String> {
        let (lo, hi) = self.hinge.range;
        if !(lo..=hi).contains(&angle) {
            return Err(format!("hinge angle {angle} is outside {lo}-{hi}"));
        }
        // Held while the guest is told, so two changes reach it in the order they were made.
        let mut a = self.angle.lock().unwrap_or_else(|e| e.into_inner());
        let (posture, folded) = (self.hinge.posture_at(*a), self.hinge.folded_at(*a));
        *a = angle;
        self.sensors.set("hinge-angle0", vec![angle]);
        let now = self.hinge.posture_at(angle);
        let folds = self.hinge.folded_at(angle) != folded;
        if folds {
            self.apply_view_at(angle);
        }
        if now != posture || folds {
            self.send(self.guest_commands(angle, folds));
        }
        Ok(now)
    }

    /// Jumps the hinge to `posture`'s angle.
    pub fn set_posture(&self, posture: StockPosture) -> Result<f32, String> {
        let angle = self.hinge.angle_for(posture).ok_or_else(|| format!("Posture {} not supported", posture.id()))?;
        self.set_angle(angle)?;
        Ok(angle)
    }

    /// `fold`: to the posture that shows the folded screen.
    pub fn fold(&self) -> Result<f32, String> {
        self.set_posture(self.hinge.fold_at)
    }

    /// `unfold`: open flat.
    pub fn unfold(&self) -> Result<f32, String> {
        self.set_posture(StockPosture::Open)
    }

    /// What the guest is told at `angle`: the display size when the fold changed it (`resize`),
    /// and, for an image that doesn't follow the hinge itself, the settings stock's
    /// `sendPostureToSystem` writes (the posture, and where the fold or hinge is).
    pub fn guest_commands(&self, angle: f32, resize: bool) -> Vec<String> {
        let mut out = Vec::new();
        if resize {
            out.push(match self.hinge.folded_at(angle) {
                true => {
                    let (w, h) = self.hinge.logical_size(angle, self.screen);
                    format!("wm size {w}x{h}")
                }
                false => "wm size reset".to_owned(),
            });
        }
        if !self.guest_device_states {
            out.push(format!("settings put global device_posture {}", self.hinge.posture_at(angle).id()));
            if let Some(feature) = self.hinge.display_feature() {
                out.push(format!("settings put global display_features {feature}"));
            }
        }
        out
    }

    /// Tells the guest the current state once it has booted (a boot keeps a forced size of ours
    /// from before: the size is set again), after the boot commands.
    pub fn announce(&self) {
        let a = self.angle.lock().unwrap_or_else(|e| e.into_inner());
        let mut commands = self.boot_commands.clone();
        commands.extend(self.guest_commands(*a, true));
        self.send(commands);
    }

    fn apply_view(&self) {
        self.apply_view_at(self.angle());
    }

    fn apply_view_at(&self, angle: f32) {
        if let Some(display) = &self.display {
            let folded = self.hinge.folded_at(angle);
            let r = self.hinge.visible(angle, self.screen);
            display.set_view(folded.then_some((r.x, r.y, r.width, r.height)));
        }
    }

    fn send(&self, commands: Vec<String>) {
        if let (Some(shell), false) = (&self.shell, commands.is_empty()) {
            shell(commands);
        }
    }
}

/// `fold`, `unfold`, `posture <id>` (stock's console syntax and replies).
pub fn console(fold: Option<&Foldable>, words: &[&str]) -> String {
    let Some(&cmd) = words.first() else { return String::new() };
    let Some(fold) = fold else { return "KO: Device is not foldable".into() };
    let done = |r: Result<f32, String>| match r {
        Ok(_) => "OK".to_string(),
        Err(e) => format!("KO: {e}"),
    };
    match (cmd, &words[1..]) {
        ("fold", []) => done(fold.fold()),
        ("fold", _) => "KO: Usage: \"fold\"".into(),
        ("unfold", []) => done(fold.unfold()),
        ("unfold", _) => "KO: Usage: \"unfold\"".into(),
        ("posture", [id]) => match id.parse::<u8>().ok().and_then(StockPosture::from_id) {
            Some(p) if p != StockPosture::Unknown => match fold.set_posture(p) {
                Ok(_) => "OK".into(),
                Err(_) => format!("KO: Posture {} not supported", p.id()),
            },
            _ => format!("KO: Posture {id} not supported"),
        },
        ("posture", _) => "KO: Usage: \"posture <posture_id>\" 1: closed, 2: half-open, 3: open, 4: flipped, 5: tent".into(),
        _ => String::new(),
    }
}

/// `fold status` (simee): the hinge angle, the posture, whether folded and the region shown.
pub fn status(fold: Option<&Foldable>) -> String {
    let Some(fold) = fold else { return "KO: Device is not foldable".into() };
    let r = fold.visible();
    format!(
        "angle: {}\r\nposture: {} ({})\r\nfolded: {}\r\nvisible: {}x{}+{}+{}\r\nsize: {}x{}\r\npostures: {}\r\nOK",
        fold.angle(),
        fold.posture().name(),
        fold.posture().id(),
        fold.folded(),
        r.width,
        r.height,
        r.x,
        r.y,
        fold.logical_size().0,
        fold.logical_size().1,
        fold.hinge().posture_list().iter().map(|p| p.id().to_string()).collect::<Vec<_>>().join(",")
    )
}

/// The AVD's hinge, if it has one, driving `sensors` and the readers' view of `display`; its guest
/// commands go, in order, through `adb -s emulator-<console_port> shell` (stock's `AdbInterface`
/// runs the SDK's adb the same way).
pub fn for_avd(
    avd: &emu_avd::Avd,
    sensors: Arc<emu_vmm::pipe::sensors::Sensors>,
    display: Option<Arc<FrameStore>>,
    console_port: u16,
) -> Option<Foldable> {
    let hinge = Hinge::from_config(|k| avd.get(k).map(str::to_owned))?;
    let (w, h, _) = avd.lcd();
    let adb = avd
        .sdk_root
        .as_ref()
        .map(|r| r.join("platform-tools/adb"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| std::path::PathBuf::from("adb"));
    let serial = format!("emulator-{console_port}");
    let (tx, rx) = std::sync::mpsc::channel::<Vec<String>>();
    // One thread, so a fold and the unfold after it reach the guest in that order.
    let _ = std::thread::Builder::new().name("fold-guest".into()).spawn(move || {
        for commands in rx {
            for c in commands {
                let _ = std::process::Command::new(&adb)
                    .args(["-s", &serial, "shell", &c])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
    });
    let tx = Mutex::new(tx);
    let shell: GuestShell = Box::new(move |commands| {
        let _ = tx.lock().unwrap_or_else(|e| e.into_inner()).send(commands);
    });
    Some(
        Foldable::new(hinge, (w, h), sensors, guest_device_states(avd).is_some())
            .with_shell(shell)
            .with_display(display)
            .with_boot_commands(skin_overlays(avd)),
    )
}

/// The device's skin overlays, which stock enables once the guest has booted ("Enable overlay
/// packages with name ending in ..."): on images that ship them (`DeviceSkinOverlay`), e.g. the
/// Pixel Fold's `com.android.internal.emulation.pixel_fold`, which gives the framework the fold's
/// `config_display_features` and the device states' postures that Jetpack WindowManager reads.
pub fn skin_overlays(avd: &emu_avd::Avd) -> Vec<String> {
    let device = avd.get("hw.device.name").map(str::trim).unwrap_or_default();
    let valid = !device.is_empty() && device.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if avd.image_feature("DeviceSkinOverlay") != Some(true) || !valid {
        return Vec::new();
    }
    ["com.android.internal.emulation", "com.android.systemui.emulation"].map(|p| format!("cmd overlay enable {p}.{device}")).to_vec()
}

/// The device states a Pixel Fold-style image follows its hinge with, when it has them: the image
/// asks for them (`DeviceStateOnBoot`) and ships a configuration for the AVD's device
/// (`hw.device.name`, e.g. `pixel_fold`, `pixel_9_pro_fold`).
pub fn guest_device_states(avd: &emu_avd::Avd) -> Option<String> {
    if avd.image_feature("DeviceStateOnBoot") != Some(true) {
        return None;
    }
    let device = avd.get("hw.device.name")?.trim();
    if device.is_empty() || device.contains(['/', '.']) {
        return None;
    }
    device_state_config(&avd.sysdir, device)
}

/// The device-state configuration a Pixel Fold image reads at boot (`androidboot.qemu.device_state`,
/// which its `init` writes to `/data/vendor/device_state_configuration.xml`): the file the image
/// ships for its device in `data/misc/<device>/devicestate/`, as stock sends it (one line).
pub fn device_state_config(sysdir: &std::path::Path, device: &str) -> Option<String> {
    let text = std::fs::read_to_string(sysdir.join("data/misc").join(device).join("devicestate/device_state_configuration.xml")).ok()?;
    let line: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    Some(line.join(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_core::fold::{dual_screen, pixel_fold};

    fn sensors() -> Arc<emu_vmm::pipe::sensors::Sensors> {
        Arc::new(emu_vmm::pipe::sensors::Sensors::new(1 << 11))
    }

    fn recording(fold: Foldable) -> (Foldable, Arc<Mutex<Vec<String>>>) {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let s = seen.clone();
        (fold.with_shell(Box::new(move |c| s.lock().expect("lock").extend(c))), seen)
    }

    #[test]
    fn fold_and_unfold_move_the_hinge_and_the_screen() {
        let s = sensors();
        let display = Arc::new(FrameStore::default());
        let pixels = vec![0u8; 2208 * 1840 * 4];
        emu_vmm::display::DisplaySink::present(&*display, 2208, 1840, 2208 * 4, 1, &pixels);
        let fold = Foldable::new(pixel_fold().hinge, (2208, 1840), s.clone(), true).with_display(Some(display.clone()));
        assert_eq!((fold.posture(), fold.folded(), display.geometry().0), (StockPosture::Open, false, 2208));
        assert_eq!(console(Some(&fold), &["fold"]), "OK");
        assert_eq!(s.get("hinge-angle0"), Some(vec![0.0]));
        assert!(fold.folded());
        // The outer screen's 1080x2092, drawn at the inner screen's height, centred.
        assert_eq!((fold.logical_size(), fold.visible()), ((1080, 2092), Rect { x: 629, y: 0, width: 949, height: 1840 }));
        assert_eq!((display.geometry().0, display.geometry().1, display.view_origin()), (949, 1840, (629, 0)));
        assert_eq!(console(Some(&fold), &["posture", "2"]), "OK");
        assert_eq!((fold.angle(), fold.folded(), display.geometry().0), (90.0, false, 2208));
        assert_eq!(console(Some(&fold), &["unfold"]), "OK");
        assert_eq!(fold.visible(), Rect { x: 0, y: 0, width: 2208, height: 1840 });
        assert!(status(Some(&fold)).starts_with("angle: 180\r\nposture: opened (3)\r\nfolded: false\r\nvisible: 2208x1840+0+0"));
    }

    #[test]
    fn the_console_answers_as_stock_does() {
        let fold = Foldable::new(pixel_fold().hinge, (2208, 1840), sensors(), true);
        assert_eq!(console(None, &["fold"]), "KO: Device is not foldable");
        assert_eq!(console(Some(&fold), &["fold", "now"]), "KO: Usage: \"fold\"");
        assert_eq!(console(Some(&fold), &["unfold", "x"]), "KO: Usage: \"unfold\"");
        assert_eq!(console(Some(&fold), &["posture", "4"]), "KO: Posture 4 not supported", "a fold-in doesn't flip");
        assert_eq!(console(Some(&fold), &["posture", "9"]), "KO: Posture 9 not supported");
        assert!(console(Some(&fold), &["posture"]).starts_with("KO: Usage: \"posture <posture_id>\""));
        assert_eq!(fold.set_angle(200.0), Err("hinge angle 200 is outside 0-180".into()));
    }

    #[test]
    fn a_dual_screen_folds_back_to_one_screen() {
        let fold = Foldable::new(dual_screen().hinge, (2784, 1800), sensors(), false);
        assert_eq!(console(Some(&fold), &["fold"]), "OK");
        assert_eq!((fold.angle(), fold.posture()), (360.0, StockPosture::Flipped));
        assert_eq!((fold.logical_size(), fold.visible()), ((1350, 1800), Rect { x: 717, y: 0, width: 1350, height: 1800 }));
        assert_eq!(console(Some(&fold), &["posture", "1"]), "OK");
        assert!(!fold.folded(), "closed, both screens face each other: nothing to shrink to");
        assert_eq!(console(Some(&fold), &["posture", "5"]), "OK");
        assert_eq!(fold.angle(), 270.0);
    }

    #[test]
    fn the_guest_hears_of_posture_and_size_changes() {
        let (fold, seen) = recording(Foldable::new(dual_screen().hinge, (2784, 1800), sensors(), false));
        let take = |seen: &Arc<Mutex<Vec<String>>>| std::mem::take(&mut *seen.lock().expect("lock"));
        fold.set_angle(170.0).expect("same posture");
        assert!(take(&seen).is_empty(), "no posture change, nothing sent");
        fold.set_posture(StockPosture::HalfOpen).expect("half");
        let hinge = "settings put global display_features hinge-[1350,0,1434,1800]";
        assert_eq!(take(&seen), ["settings put global device_posture 2", hinge]);
        fold.fold().expect("flip");
        assert_eq!(take(&seen), ["wm size 1350x1800", "settings put global device_posture 4", hinge]);
        fold.unfold().expect("open");
        assert_eq!(take(&seen), ["wm size reset", "settings put global device_posture 3", hinge]);
        // A Pixel Fold image follows the hinge sensor with its own device states: only the size.
        let (pixel, seen) = recording(Foldable::new(pixel_fold().hinge, (2208, 1840), sensors(), true));
        pixel.fold().expect("fold");
        assert_eq!(take(&seen), ["wm size 1080x2092"]);
        pixel.set_posture(StockPosture::HalfOpen).expect("half");
        assert_eq!(take(&seen), ["wm size reset"]);
        pixel.unfold().expect("open");
        assert!(take(&seen).is_empty());
        pixel.announce();
        assert_eq!(take(&seen), ["wm size reset"], "a boot gets the size again");
    }

    #[test]
    fn the_device_state_config_comes_from_the_image() {
        let dir = std::env::temp_dir().join(format!("emu-fold-{}", std::process::id()));
        let states = dir.join("data/misc/pixel_fold/devicestate");
        std::fs::create_dir_all(&states).expect("mkdir");
        std::fs::write(
            states.join("device_state_configuration.xml"),
            "<device-state-config>\n    <device-state>\n    </device-state>\n</device-state-config>\n",
        )
        .expect("write");
        assert_eq!(
            device_state_config(&dir, "pixel_fold").as_deref(),
            Some("<device-state-config><device-state></device-state></device-state-config>")
        );
        assert_eq!(device_state_config(&dir, "surface_duo"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
