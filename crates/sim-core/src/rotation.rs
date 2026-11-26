//! How far a device's display is rotated from its natural orientation, in quarter turns
//! clockwise, and what that does to its frames and touches.
//!
//! The panel never turns: an Android guest that rotates its display draws its turned layout
//! into the same scanout, sideways on it. Here a [`Rotation`] says how far to turn a panel frame
//! clockwise to show that layout upright (the *screen*, what `adb screencap` and uiautomator's
//! bounds are in), and maps points both ways: a touch at screen pixels lands on the panel pixel
//! shown there, which is what the touch screen (fixed to the panel) reports.
//!
//! The turn is the guest's own display rotation, not the accelerometer's: the guest decides
//! whether to follow gravity (auto-rotate, an app's orientation lock, a phone's refusal to turn
//! upside down, a device lying flat), and its window manager logs each rotation it makes
//! ([`guest_rotation`]), which emu reads from the guest's logcat stream. Only the form factors
//! whose guests rotate ([`turns`]) are followed.

use crate::form_factor::FormFactor;

/// Quarter turns clockwise, from the device's natural orientation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Rotation {
    #[default]
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

impl Rotation {
    pub const ALL: [Rotation; 4] = [Self::Deg0, Self::Deg90, Self::Deg180, Self::Deg270];

    /// `quarters` quarter turns clockwise (any count; four make a whole turn).
    pub fn from_quarters(quarters: u32) -> Rotation {
        Self::ALL[(quarters % 4) as usize]
    }

    pub fn quarters(self) -> u32 {
        self as u32
    }

    pub fn degrees(self) -> u32 {
        self.quarters() * 90
    }

    /// `0`, `90`, `180` or `270` (other multiples of 90 wrap).
    pub fn from_degrees(degrees: u32) -> Option<Rotation> {
        degrees.is_multiple_of(90).then(|| Self::from_quarters(degrees / 90))
    }

    /// The turn for Android's display rotation `surface` (`Surface.ROTATION_0..270`, 0 to 3:
    /// quarter turns the layout is rotated counter-clockwise on the panel). A phone turned with
    /// its top to the right is at `ROTATION_270`, and its panel's frames turn a quarter clockwise
    /// to show it upright (checked on the stock emulator 37.2.13: after one `rotate` its guest
    /// reports `ROTATION_270` and `adb screencap` is the panel turned so). None past 3.
    pub fn from_surface(surface: u32) -> Option<Rotation> {
        (surface < 4).then(|| Self::from_quarters(surface).inverse())
    }

    /// A further quarter turn clockwise (what Android's `rotate` console command does).
    pub fn clockwise(self) -> Rotation {
        Self::from_quarters(self.quarters() + 1)
    }

    /// The turn that undoes this one.
    pub fn inverse(self) -> Rotation {
        Self::from_quarters(4 - self.quarters())
    }

    /// Whether width and height trade places.
    pub fn is_sideways(self) -> bool {
        matches!(self, Self::Deg90 | Self::Deg270)
    }

    /// The screen's size for a `width` x `height` panel.
    pub fn turned(self, (width, height): (u32, u32)) -> (u32, u32) {
        if self.is_sideways() { (height, width) } else { (width, height) }
    }

    /// Where pixel `(x, y)` of a `width` x `height` panel shows on the screen.
    pub fn panel_to_screen(self, (x, y): (u32, u32), (width, height): (u32, u32)) -> (u32, u32) {
        let (w, h) = (width.max(1) - 1, height.max(1) - 1);
        match self {
            Self::Deg0 => (x, y),
            Self::Deg90 => (h - y.min(h), x),
            Self::Deg180 => (w - x.min(w), h - y.min(h)),
            Self::Deg270 => (y, w - x.min(w)),
        }
    }

    /// The panel pixel shown at screen point `(x, y)`, for a `width` x `height` panel: a point
    /// in pixel `(i, j)` of the screen lands in the panel pixel shown there. Points off the
    /// screen are clamped onto its edge.
    pub fn screen_to_panel(self, (x, y): (f64, f64), (width, height): (u32, u32)) -> (u32, u32) {
        let (sw, sh) = self.turned((width, height));
        let pixel = |v: f64, n: u32| if v.is_finite() { v.floor().clamp(0.0, f64::from(n.max(1) - 1)) as u32 } else { 0 };
        let (sx, sy) = (pixel(x, sw), pixel(y, sh));
        // Undoing the turn on the screen's own size gives the panel pixel.
        self.inverse().panel_to_screen((sx, sy), (sw, sh))
    }
}

/// Whether a device of `form_factor` turns its display: phones, tablets and foldables. A TV, a
/// car's display, a watch and an XR headset or glasses keep their panel's orientation (their
/// guests don't rotate, and simee never turns their frames).
pub fn turns(form_factor: FormFactor) -> bool {
    matches!(form_factor, FormFactor::Phone | FormFactor::Tablet | FormFactor::Foldable)
}

/// The logcat tags whose lines say the guest rotated its display ([`guest_rotation`]), and the
/// priority they're logged at.
pub const ROTATION_TAGS: [(&str, char); 2] = [("WindowManager", 'I'), ("ActivityTaskManager", 'I')];

/// The default display's new rotation, if logcat `line` reports one. The guest's window manager
/// logs it two ways, both at info: `Display id=0 rotation changed to 3 from 0, ...`
/// (`WindowManager`, API 36 and later) and the global configuration it then applies, `Config
/// changes=... winConfig={ ... mDisplayRotation=ROTATION_270 ...}` (`ActivityTaskManager`, which
/// API 34 and 35 log too). Other displays' lines and anything else are None.
pub fn guest_rotation(line: &str) -> Option<Rotation> {
    const CHANGED: &str = "Display id=0 rotation changed to ";
    if let Some(at) = line.find(CHANGED) {
        let n = line[at + CHANGED.len()..].chars().next()?.to_digit(10)?;
        return Rotation::from_surface(n);
    }
    // Capitalised: WindowManager's per-display `Override config changes=` is the same change.
    const CONFIG: &str = "Config changes=";
    const DISPLAY: &str = "mDisplayRotation=ROTATION_";
    let config = &line[line.find(CONFIG)?..];
    let at = config.find(DISPLAY)? + DISPLAY.len();
    let degrees: String = config[at..].chars().take_while(char::is_ascii_digit).collect();
    Rotation::from_surface(degrees.parse::<u32>().ok().filter(|d| d % 90 == 0)? / 90)
}

/// Turns tightly packed RGBA (`width` x `height`, top row first) clockwise by `rotation` into
/// `dst`, which must hold exactly as many bytes; returns the turned size. Square tiles keep both
/// sides' reads and writes in cache. Exact: every pixel is copied, none are resampled.
pub fn rotate_rgba_into(src: &[u8], width: u32, height: u32, rotation: Rotation, dst: &mut [u8]) -> (u32, u32) {
    let (w, h) = (width as usize, height as usize);
    let turned = rotation.turned((width, height));
    let n = w * h;
    let (Some(src), Some(dst)) = (src.get(..n * 4), dst.get_mut(..n * 4)) else { return turned };
    let (src, _) = src.as_chunks::<4>();
    let (dst, _) = dst.as_chunks_mut::<4>();
    if rotation == Rotation::Deg0 {
        dst.copy_from_slice(src);
        return turned;
    }
    if rotation == Rotation::Deg180 {
        for (d, s) in dst.iter_mut().zip(src.iter().rev()) {
            *d = *s;
        }
        return turned;
    }
    const TILE: usize = 32;
    let dw = turned.0 as usize;
    for ty in (0..h).step_by(TILE) {
        for tx in (0..w).step_by(TILE) {
            for y in ty..(ty + TILE).min(h) {
                let end = (tx + TILE).min(w);
                for (x, px) in src[y * w + tx..y * w + end].iter().enumerate().map(|(i, p)| (tx + i, p)) {
                    // 90: (x, y) -> (h-1-y, x); 270: (x, y) -> (y, w-1-x).
                    let (dx, dy) = if rotation == Rotation::Deg90 { (h - 1 - y, x) } else { (y, w - 1 - x) };
                    dst[dy * dw + dx] = *px;
                }
            }
        }
    }
    turned
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame whose every pixel is its own index, so a turned frame says where each came from.
    fn indexed(w: u32, h: u32) -> Vec<u8> {
        (0..w * h).flat_map(|i| i.to_le_bytes()).collect()
    }

    fn at(rgba: &[u8], w: u32, (x, y): (u32, u32)) -> u32 {
        let i = (y * w + x) as usize * 4;
        u32::from_le_bytes(rgba[i..i + 4].try_into().expect("pixel"))
    }

    #[test]
    fn turns_count_round_and_name_their_degrees() {
        assert_eq!(Rotation::default(), Rotation::Deg0);
        assert_eq!(Rotation::Deg270.clockwise(), Rotation::Deg0);
        assert_eq!(Rotation::from_quarters(5), Rotation::Deg90);
        assert_eq!(Rotation::from_degrees(270), Some(Rotation::Deg270));
        assert_eq!(Rotation::from_degrees(45), None);
        for r in Rotation::ALL {
            assert_eq!(Rotation::from_degrees(r.degrees()), Some(r));
            assert_eq!(Rotation::from_quarters(r.quarters() + r.inverse().quarters()), Rotation::Deg0);
        }
        // Android's ROTATION_270 (a phone turned with its top to the right, as after stock's
        // first `rotate`) is the panel turned a quarter clockwise.
        for (surface, r) in [(0, Rotation::Deg0), (1, Rotation::Deg270), (2, Rotation::Deg180), (3, Rotation::Deg90)] {
            assert_eq!(Rotation::from_surface(surface), Some(r), "ROTATION_{}", surface * 90);
        }
        assert_eq!(Rotation::from_surface(4), None);
        assert_eq!(Rotation::Deg90.turned((1080, 2400)), (2400, 1080));
        assert_eq!(Rotation::Deg180.turned((1080, 2400)), (1080, 2400));
    }

    /// The lines the guest logs when it rotates, as API 36 and API 34 images log them (captured
    /// under simee, 2026-11-26), and lines that must not count.
    #[test]
    fn the_guests_rotation_is_read_from_its_log() {
        let api36 = "11-26 09:58:57.210   642   680 I WindowManager: Display id=0 rotation changed to 3 from 0, lastOrientation=-1 userRotationMode=0 userRotation=0 lastSensorRotation=3";
        assert_eq!(guest_rotation(api36), Some(Rotation::Deg90));
        let back = "11-26 09:58:41.101   642   680 I WindowManager: Display id=0 rotation changed to 0 from 1, lastOrientation=-1";
        assert_eq!(guest_rotation(back), Some(Rotation::Deg0));
        let api34 = "11-26 10:07:09.487   540  1888 I ActivityTaskManager: Config changes=20000580 {1.0 310mcc260mnc [en_US] ldltr sw360dp w640dp h312dp 320dpi nrml long land finger qwerty/v/v dpad/v winConfig={ mBounds=Rect(0, 0 - 1280, 720) mAppBounds=Rect(0, 0 - 1280, 672) mMaxBounds=Rect(0, 0 - 1280, 720) mDisplayRotation=ROTATION_270 mWindowingMode=fullscreen mActivityType=undefined mAlwaysOnTop=undefined mRotation=ROTATION_270} s.6 fontWeightAdjustment=0}";
        assert_eq!(guest_rotation(api34), Some(Rotation::Deg90));
        assert_eq!(guest_rotation(&api34.replace("ROTATION_270", "ROTATION_90")), Some(Rotation::Deg270));
        for not in [
            // Another display, the per-display override (the same change again), a config change
            // that says nothing of rotation, and noise.
            "I WindowManager: Display id=2 rotation changed to 1 from 0",
            "I WindowManager: Override config changes=20000480 {... mDisplayRotation=ROTATION_90 ...} for displayId=3",
            "I ActivityTaskManager: Config changes=4 {1.0 ... }",
            "I ActivityTaskManager: Config changes=20000480 {... mDisplayRotation=ROTATION_45 ...}",
            "I WindowManager: Display id=0 rotation changed to x from 0",
            "",
        ] {
            assert_eq!(guest_rotation(not), None, "{not}");
        }
    }

    /// Phones, tablets and foldables turn; XR (headsets and glasses), TVs, cars and watches
    /// never do.
    #[test]
    fn only_handheld_displays_turn() {
        for ff in FormFactor::ALL {
            let want = matches!(ff, FormFactor::Phone | FormFactor::Tablet | FormFactor::Foldable);
            assert_eq!(turns(ff), want, "{ff:?}");
        }
        for never in [FormFactor::Vision, FormFactor::Tv, FormFactor::Auto, FormFactor::Watch] {
            assert!(!turns(never), "{never:?}");
        }
    }

    #[test]
    fn turning_a_frame_puts_each_pixel_where_its_point_maps() {
        // A phone, a landscape tablet and a folded Pixel Fold's outer screen, scaled down; odd
        // sizes catch an off-by-one at the far edge, and 70 crosses a tile boundary.
        for (w, h) in [(5, 7), (8, 5), (19, 37), (70, 33), (1, 1), (1, 4)] {
            let src = indexed(w, h);
            for r in Rotation::ALL {
                let (tw, th) = r.turned((w, h));
                let mut dst = vec![0; src.len()];
                assert_eq!(rotate_rgba_into(&src, w, h, r, &mut dst), (tw, th));
                for y in 0..h {
                    for x in 0..w {
                        let s = r.panel_to_screen((x, y), (w, h));
                        assert!(s.0 < tw && s.1 < th, "{r:?} {w}x{h}: ({x}, {y}) -> {s:?}");
                        assert_eq!(at(&dst, tw, s), y * w + x, "{r:?} {w}x{h} at ({x}, {y})");
                        // And a touch in that screen pixel lands back on it.
                        let centre = (f64::from(s.0) + 0.5, f64::from(s.1) + 0.5);
                        assert_eq!(r.screen_to_panel(centre, (w, h)), (x, y), "{r:?} {w}x{h}");
                        assert_eq!(r.screen_to_panel((f64::from(s.0), f64::from(s.1)), (w, h)), (x, y));
                    }
                }
            }
        }
    }

    #[test]
    fn turned_frames_look_the_way_a_turned_device_does() {
        // A 2x3 panel with its top-left pixel marked: turned a quarter clockwise, the mark is
        // at the top right of the 3x2 screen, as on a phone turned with its top to the right.
        let mut src = vec![0u8; 2 * 3 * 4];
        src[..4].copy_from_slice(&[255, 0, 0, 255]);
        let mut dst = vec![0; src.len()];
        for (r, mark) in [(Rotation::Deg90, (2, 0)), (Rotation::Deg180, (1, 2)), (Rotation::Deg270, (0, 1))] {
            let (tw, _) = rotate_rgba_into(&src, 2, 3, r, &mut dst);
            let i = (mark.1 * tw + mark.0) as usize * 4;
            assert_eq!(&dst[i..i + 4], &[255, 0, 0, 255], "{r:?}");
            assert_eq!(dst.iter().filter(|b| **b == 255).count(), 2, "one pixel marked");
        }
    }

    #[test]
    fn touches_off_the_screen_clamp_to_its_edge() {
        let panel = (1080, 2400);
        assert_eq!(Rotation::Deg90.screen_to_panel((-5.0, 0.0), panel), (0, 2399));
        assert_eq!(Rotation::Deg90.screen_to_panel((2400.0, 1080.0), panel), (1079, 0));
        assert_eq!(Rotation::Deg0.screen_to_panel((f64::NAN, 9.0), panel), (0, 9));
        // The screen's corners land on the panel's, each way it can be turned.
        for (r, top_left) in
            [(Rotation::Deg0, (0, 0)), (Rotation::Deg90, (0, 2399)), (Rotation::Deg180, (1079, 2399)), (Rotation::Deg270, (1079, 0))]
        {
            assert_eq!(r.screen_to_panel((0.0, 0.0), panel), top_left, "{r:?}");
        }
    }

    #[test]
    fn a_short_buffer_is_left_alone() {
        let mut dst = vec![7u8; 8];
        assert_eq!(rotate_rgba_into(&[1; 4], 2, 1, Rotation::Deg90, &mut dst), (1, 2));
        assert_eq!(dst, [7; 8]);
    }

    /// Times a turn of a phone-sized frame: `cargo test -p sim-core --release rotate_cost --
    /// --ignored --nocapture`.
    #[test]
    #[ignore]
    fn rotate_cost() {
        for (w, h) in [(1080u32, 2400u32), (2560, 1600)] {
            let src = indexed(w, h);
            let mut dst = vec![0; src.len()];
            for r in [Rotation::Deg90, Rotation::Deg180, Rotation::Deg270] {
                let mut times: Vec<f64> = (0..15)
                    .map(|_| {
                        let t = std::time::Instant::now();
                        rotate_rgba_into(&src, w, h, r, &mut dst);
                        t.elapsed().as_secs_f64() * 1000.0
                    })
                    .collect();
                times.sort_by(f64::total_cmp);
                println!("{w}x{h} {r:?}: median {:.2} ms", times[times.len() / 2]);
            }
        }
    }
}
