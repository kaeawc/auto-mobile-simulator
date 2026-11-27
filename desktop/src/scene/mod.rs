//! The 3D device view (#151, #152): the selected device's body (`sim_core::body`) drawn in
//! perspective with depth and 4x MSAA, lit by one directional light and ambient, with the live
//! screen on its screen face. ⌘3 toggles it; `SIMEE_VIEW=3d` starts with it on.
//!
//! The screen texture is the flat view's own picture: the view keeps the flat screen element
//! where it always is and lays an effect slot over the stage that asks Rune for a backdrop copy
//! (`El::effect_backdrop`). The compositor ([`gpu::Scene`]) renders the body into its own MSAA and
//! depth targets, sampling the screen element's rect of that copy, and draws the result into the
//! slot, which hides the flat screen. So whatever the flat view shows (an iOS IOSurface sampled
//! in place, Android's frames, a turned panel, an iPhone Duo's screen in use) is on the body,
//! with no CPU copy of any pixel: the copy is one GPU texture copy per drawn frame. Rune can't
//! hand a compositor a `NativeSurface`'s texture yet (anomalyco/rune#2546, simee#192).
//!
//! Depth attachments and MSAA resolves need Rune's wgpu backend (`SIMEE_GPU_BACKEND=wgpu`):
//! the native Metal backend panics on them (anomalyco/rune#2547, simee#193), so on it the toggle
//! says why it does nothing. With the view off the window draws exactly as without it.
//!
//! The camera is fixed (`SIMEE_SCENE_CAMERA=front|side` picks another preset for screenshots);
//! orbit, pan and zoom are #155. A foldable lies flat: the desktop doesn't track the hinge's
//! angle (#154 animates it).

// Live Rust editing (`hot`) runs the spec without a compositor.
#![cfg_attr(feature = "hot", allow(dead_code))]

mod camera;
mod gpu;
mod layout;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rune::{Color, Drag, El, Tx};
use sim_core::body::{self, Body, Display, Face};
use sim_core::platform::Platform;
use sim_core::rotation::Rotation;

pub use camera::Preset;
pub use gpu::Scene;

use crate::State;

/// The slot's `El::effect` mark.
const MARK: u32 = 152;

/// What ⌘3 says on a backend that can't draw the scene.
pub const UNAVAILABLE: &str = "The 3D view needs SIMEE_GPU_BACKEND=wgpu: Rune's native Metal backend can't draw a depth buffer \
                               or MSAA yet (anomalyco/rune#2547)";
/// What ⌘3 says once the view is on.
pub const ON: &str = "3D view on: clicks on the device are ignored there until input goes through the 3D projection (#156)";
/// What ⌘3 says for a device with no body.
pub const NO_BODY: &str = "The 3D view has bodies for phones, tablets, foldables and watches";

/// Whether this process draws with a backend the scene can use, set once in `main` before the
/// window opens (headless scenes and tests leave it false unless they set it).
static SUPPORTED: AtomicBool = AtomicBool::new(false);

pub fn set_supported(on: bool) {
    SUPPORTED.store(on, Ordering::Relaxed);
}

/// Whether the backend `RUNE_GPU_BACKEND` (which wins) and `SIMEE_GPU_BACKEND` choose can draw
/// the scene: wgpu's. macOS draws with Rune's native Metal backend when neither says; other
/// hosts always draw with wgpu. Rune matches its variable exactly (`wgpu` or `metal`; anything
/// else is ignored), while simee's is trimmed and lowercased (`crate::gpu_backend`): each is read
/// the way its owner reads it, so this agrees with the backend Rune draws with.
pub fn wgpu_backend(rune: Option<&str>, simee: Option<&str>, macos: bool) -> bool {
    let rune = rune.filter(|s| matches!(*s, "wgpu" | "metal"));
    let simee = simee.map(|s| s.trim().to_ascii_lowercase()).filter(|s| s == "wgpu" || s == "metal");
    match rune.or(simee.as_deref()) {
        Some(b) => b == "wgpu" || !macos,
        None => !macos,
    }
}

/// The 3D view's switch.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Toggle {
    /// Asked for (⌘3, `SIMEE_VIEW=3d`) on a backend that can draw it.
    pub on: bool,
    /// The backend can draw it.
    pub supported: bool,
    pub preset: Preset,
}

impl Toggle {
    /// From `SIMEE_VIEW` and `SIMEE_SCENE_CAMERA`; a status line when 3D was asked for and can't be drawn.
    pub fn from_env(view: Option<&str>, camera: Option<&str>, supported: bool) -> (Toggle, Option<String>) {
        let asked = view.is_some_and(|v| v.trim().eq_ignore_ascii_case("3d"));
        let t = Toggle { on: asked && supported, supported, preset: Preset::parse(camera) };
        (t, (asked && !supported).then(|| UNAVAILABLE.to_string()))
    }

    /// The process's own settings.
    pub fn from_process() -> (Toggle, Option<String>) {
        let var = |k| std::env::var(k).ok();
        Toggle::from_env(var("SIMEE_VIEW").as_deref(), var("SIMEE_SCENE_CAMERA").as_deref(), SUPPORTED.load(Ordering::Relaxed))
    }

    /// ⌘3: turns it on or off, or says why it can't.
    pub fn flip(&mut self) -> Option<String> {
        if !self.supported {
            return Some(UNAVAILABLE.into());
        }
        self.on = !self.on;
        None
    }
}

/// The selected device's body: the table's row for its device type (an Apple identifier, an
/// AVD's `hw.device.name`) when it has one, else a generic body for its screen.
pub fn body_for(s: &State) -> Option<Arc<Body>> {
    let d = s.selected_info()?;
    match d.platform {
        Platform::Ios => body::get(&d.device_type_id).or_else(|| body::for_profile(&d.kind()?.profile())),
        Platform::Android => {
            let device = s.android.as_ref().filter(|a| a.info.udid == d.udid);
            let profile = device.and_then(|a| a.profile().ok()).or_else(|| d.kind().map(|k| k.profile()))?;
            // An unlisted foldable folds the way its AVD's hinge says (a flip, its angles).
            let config = device.and_then(|a| a.avd.as_ref());
            let hinge = config.and_then(|c| sim_core::fold::Hinge::from_config(|k| c.get(k).map(str::to_owned)));
            body::for_android_hinge(&d.device_type_id, &profile, hinge.as_ref())
        }
    }
}

/// The body to draw when the 3D view is on and the screen is showing.
pub fn active(s: &State) -> Option<Arc<Body>> {
    (s.view3d.on && s.showing_screen()).then(|| body_for(s)).flatten()
}

/// The slot laid over the stage while the 3D view is on: it covers the flat screen (still laid
/// out where it always is, so Rune draws it into the backdrop copy) and takes the pointer, so a
/// click on the 3D view doesn't land on the flat screen beneath (mapping touches through the
/// projection is #156). It tells the compositor where that screen is in the same frame: the
/// stage's padding (`inset`, points) and the screen's aspect ratio, which the view centres in
/// what's left ([`screen_rect`]).
pub fn slot(inset: f32, aspect: f32) -> El {
    rune::effect(MARK)
        .key("scene-3d")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .effect_backdrop(true)
        .effect_data([inset, aspect, 0.0, 0.0])
        .on_drag(|_: &mut State, _: &mut Drag, _: &mut Tx| {})
}

/// The flat screen inside a slot at `rect` (`[x, y, w, h]`, device pixels) with `scale` device
/// pixels per point, from the slot's payload: the largest rect of the screen's aspect inside
/// the stage's padding, centred, as the view lays it out.
pub fn screen_rect(rect: [f32; 4], scale: f32, [inset, aspect, ..]: [f32; 4]) -> [f32; 4] {
    let pad = inset * scale;
    let (w, h) = ((rect[2] - 2.0 * pad).max(1.0), (rect[3] - 2.0 * pad).max(1.0));
    let aspect = if aspect.is_finite() && aspect > 0.0 { aspect } else { w / h };
    let (sw, sh) = if w / h > aspect { (h * aspect, h) } else { (w, w / aspect) };
    [rect[0] + pad + (w - sw) / 2.0, rect[1] + pad + (h - sh) / 2.0, sw, sh]
}

/// What the scene is cleared to: the stage's colour (the window is always dark).
pub fn background() -> Color {
    rune::tok::theme(true).bg
}

/// Which screen of the body shows the frame, and how the body lies so it reads upright.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub display: Display,
    pub face: Face,
    /// Quarter turns clockwise, as the screen is seen, that the body is rolled by: the frame's
    /// rows run along the screen's rows after this turn.
    pub quarters: u32,
    /// The hinge's angle, degrees (180 flat).
    pub hinge: f32,
}

/// How far apart two aspect ratios can be and still be one screen (Android streams frames
/// scaled to a width, and rounds).
const ASPECT_SLACK: f32 = 0.04;

fn aspect((w, h): (u32, u32)) -> f32 {
    w as f32 / h.max(1) as f32
}

fn same_shape(a: (u32, u32), b: (u32, u32)) -> bool {
    (aspect(a) / aspect(b) - 1.0).abs() < ASPECT_SLACK
}

/// Places a frame shown upright at `shown` pixels, whose panel lies turned `turn` (clockwise,
/// as the flat view turns it to show it upright), on `body`: on the screen of that shape (the
/// outer screen of a folded foldable, its inner one unfolded), the body rolled with the panel.
/// A screen stored the other way round from its frame (the iPhone Duo's inner screen) takes the
/// frame a quarter turn clockwise, as `sim_core::body::Screen` documents: the body rolls a quarter
/// counter-clockwise so the frame reads upright. A frame no screen matches goes on the first front screen.
/// The hinge is closed when the frame is a foldable's outer screen (emu streams only that one
/// folded) and flat otherwise: the desktop doesn't know a half-open angle (#154).
pub fn place(body: &Body, shown: (u32, u32), turn: Rotation) -> Option<Placement> {
    let panel = if turn.is_sideways() { (shown.1, shown.0) } else { shown };
    let hinge = |d: Display| match (d, &body.fold) {
        (Display::Outer, Some(f)) => f.hinge.range.0,
        _ => body::HingeAxis::FLAT,
    };
    let at = |s: &body::Screen, extra: u32| Placement {
        display: s.display,
        face: s.face,
        quarters: (turn.quarters() + extra) % 4,
        hinge: hinge(s.display),
    };
    let front_first = || body.screens.iter().filter(|s| s.face == Face::Front).chain(body.screens.iter().filter(|s| s.face == Face::Back));
    // The screen of exactly that size first (two screens can be close in shape: the iPhone Duo's),
    // then one of that shape (a stream scaled to a width).
    let fits = |px: (u32, u32), exact: bool| if exact { px == panel } else { same_shape(px, panel) };
    for (exact, extra) in [(true, 0), (true, 3), (false, 0), (false, 3)] {
        let turned = |s: &&body::Screen| if extra == 0 { s.px } else { (s.px.1, s.px.0) };
        if let Some(s) = front_first().find(|s| fits(turned(s), exact)) {
            return Some(at(s, extra));
        }
    }
    body.screens.iter().find(|s| s.face == Face::Front).map(|s| at(s, 0))
}

/// Where the body UV `uv` of the screen face falls in the upright frame, with the body rolled
/// `quarters` clockwise: what the shader's `turned` does, kept here to test.
#[cfg(test)]
pub fn frame_uv([u, v]: [f32; 2], quarters: u32) -> [f32; 2] {
    match quarters % 4 {
        0 => [u, v],
        1 => [1.0 - v, u],
        2 => [1.0 - u, 1.0 - v],
        _ => [v, 1.0 - u],
    }
}

#[cfg(test)]
mod tests {
    use super::camera::{self, Camera, IDENTITY};
    use super::*;

    #[test]
    fn only_wgpu_draws_the_scene_on_macos() {
        assert!(!wgpu_backend(None, None, true));
        assert!(wgpu_backend(None, Some("wgpu"), true));
        assert!(wgpu_backend(None, Some(" WGPU "), true));
        assert!(!wgpu_backend(None, Some("metal"), true));
        assert!(!wgpu_backend(None, Some("vulkan"), true));
        // Rune's own variable wins over simee's.
        assert!(!wgpu_backend(Some("metal"), Some("wgpu"), true));
        assert!(wgpu_backend(Some("wgpu"), Some("metal"), true));
        // Rune matches its variable exactly: `WGPU` or ` wgpu` is no choice, so Rune draws with
        // simee's setting (else the default).
        assert!(!wgpu_backend(Some("WGPU"), None, true));
        assert!(!wgpu_backend(Some(" wgpu"), Some("metal"), true));
        assert!(wgpu_backend(Some("WGPU"), Some("wgpu"), true));
        // Elsewhere there's only wgpu.
        assert!(wgpu_backend(None, None, false));
        assert!(wgpu_backend(Some("metal"), None, false));
    }

    #[test]
    fn toggle_starts_from_the_environment_and_explains_metal() {
        assert_eq!(Toggle::from_env(None, None, true), (Toggle { on: false, supported: true, preset: Preset::ThreeQuarter }, None));
        assert!(Toggle::from_env(Some("3d"), None, true).0.on);
        assert!(Toggle::from_env(Some("3D"), Some("side"), true).0.on);
        assert_eq!(Toggle::from_env(None, Some("side"), true).0.preset, Preset::Side);
        assert!(!Toggle::from_env(Some("flat"), None, true).0.on);
        let (t, msg) = Toggle::from_env(Some("3d"), None, false);
        assert!(!t.on);
        assert_eq!(msg.as_deref(), Some(UNAVAILABLE));
    }

    #[test]
    fn flip_turns_it_on_and_off_only_where_it_can_draw() {
        let mut t = Toggle { supported: true, ..Toggle::default() };
        assert_eq!(t.flip(), None);
        assert!(t.on);
        assert_eq!(t.flip(), None);
        assert!(!t.on);
        let mut metal = Toggle::default();
        assert_eq!(metal.flip().as_deref(), Some(UNAVAILABLE));
        assert!(!metal.on);
    }

    #[test]
    fn a_phone_frame_goes_on_its_one_screen_upright_or_turned() {
        let b = body::get("iPhone-17").unwrap();
        let p = place(&b, (1206, 2622), Rotation::Deg0).unwrap();
        assert_eq!((p.display, p.face, p.quarters, p.hinge), (Display::Main, Face::Front, 0, 180.0));
        // Turned a quarter: the flat view shows it 2622x1206 and the body rolls with the panel.
        assert_eq!(place(&b, (2622, 1206), Rotation::Deg90).unwrap().quarters, 1);
        assert_eq!(place(&b, (1206, 2622), Rotation::Deg180).unwrap().quarters, 2);
        assert_eq!(place(&b, (2622, 1206), Rotation::Deg270).unwrap().quarters, 3);
        // A scaled stream is the same screen.
        assert_eq!(place(&b, (603, 1311), Rotation::Deg0).unwrap().quarters, 0);
    }

    #[test]
    fn a_foldable_shows_the_screen_its_frame_is() {
        let b = body::get("pixel_fold").unwrap();
        let inner = b.screen(Display::Inner).unwrap().px;
        let outer = b.screen(Display::Outer).unwrap().px;
        let open = place(&b, inner, Rotation::Deg0).unwrap();
        assert_eq!((open.display, open.face), (Display::Inner, Face::Front));
        let folded = place(&b, outer, Rotation::Deg0).unwrap();
        assert_eq!((folded.display, folded.face), (Display::Outer, Face::Back));
        assert_eq!(folded.hinge, b.fold.unwrap().hinge.range.0, "folded shut");
        assert_eq!(open.hinge, 180.0);
    }

    #[test]
    fn a_screen_stored_the_other_way_round_takes_the_frame_a_quarter_clockwise() {
        let b = body::get("iPhone-Duo").unwrap();
        let (w, h) = b.screen(Display::Inner).unwrap().px;
        let p = place(&b, (h, w), Rotation::Deg0).unwrap();
        assert_eq!((p.display, p.quarters), (Display::Inner, 3));
        // sim_core::body::Screen: a face UV (u, v) samples the frame at (v, 1 - u); the frame's
        // top-left corner is the face's top-right.
        assert_eq!(frame_uv([0.25, 0.5], p.quarters), [0.5, 0.75]);
        assert_eq!(frame_uv([1.0, 0.0], p.quarters), [0.0, 0.0]);
    }

    #[test]
    fn the_screen_rect_is_the_flat_screen_centred_in_the_stage() {
        // A 1000x800 slot at (100, 50), 2 px a point, 10 points of padding: a 1:2 screen fills
        // the 760 px high content box and is centred across its 960 px.
        assert_eq!(screen_rect([100.0, 50.0, 1000.0, 800.0], 2.0, [10.0, 0.5, 0.0, 0.0]), [410.0, 70.0, 380.0, 760.0]);
        // A wide screen fills the width instead.
        assert_eq!(screen_rect([0.0, 0.0, 400.0, 400.0], 1.0, [0.0, 2.0, 0.0, 0.0]), [0.0, 100.0, 400.0, 200.0]);
    }

    #[test]
    fn bounds_frame_the_points() {
        assert_eq!(camera::bounds([[0.0, -2.0, 1.0], [4.0, 2.0, 3.0]].into_iter()), ([2.0, 0.0, 2.0], [2.0, 2.0, 1.0]));
        assert_eq!(camera::bounds(std::iter::empty()), ([0.0; 3], [1.0; 3]));
        // An off-centre box: the camera looks at its centre.
        let (vp, _) = Camera::new(Preset::Front, false).view_proj(([50.0, 0.0, 0.0], [10.0, 10.0, 1.0]), 1.0);
        let c = camera::apply(&vp, [50.0, 0.0, 0.0]);
        assert!(c[0].abs() < 1e-4 && c[1].abs() < 1e-4);
    }

    #[test]
    fn frame_uv_follows_the_roll() {
        // Rolled a quarter clockwise, the screen's top-left corner is the frame's top-right.
        assert_eq!(frame_uv([0.0, 0.0], 1), [1.0, 0.0]);
        assert_eq!(frame_uv([1.0, 0.0], 1), [1.0, 1.0]);
        assert_eq!(frame_uv([0.25, 0.5], 0), [0.25, 0.5]);
        assert_eq!(frame_uv([0.0, 0.0], 2), [1.0, 1.0]);
        assert_eq!(frame_uv([0.0, 0.0], 3), [0.0, 1.0]);
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3)
    }

    #[test]
    fn matrices_compose_and_turn_the_right_way() {
        assert!(close(camera::apply(&camera::rot_z(std::f32::consts::FRAC_PI_2), [1.0, 0.0, 0.0]), [0.0, 1.0, 0.0]));
        assert!(close(camera::apply(&camera::rot_y(std::f32::consts::FRAC_PI_2), [0.0, 0.0, 1.0]), [1.0, 0.0, 0.0]));
        let m = camera::mul(&camera::translate([1.0, 2.0, 3.0]), &camera::rot_x(std::f32::consts::FRAC_PI_2));
        assert!(close(camera::apply(&m, [0.0, 1.0, 0.0]), [1.0, 2.0, 4.0]));
        assert_eq!(camera::mul(&IDENTITY, &m), m);
    }

    /// Every corner of the body lands inside clip space, near and far included.
    fn fits(cam: Camera, half: [f32; 3], aspect: f32) {
        let (vp, _) = cam.view_proj(([0.0; 3], half), aspect);
        for i in 0..8 {
            let c = [0, 1, 2].map(|k| if i >> k & 1 == 1 { half[k] } else { -half[k] });
            let [x, y, z] = camera::apply(&vp, c);
            assert!(x.abs() <= 1.0 && y.abs() <= 1.0 && (0.0..=1.0).contains(&z), "{c:?} -> {:?} ({cam:?}, {aspect})", [x, y, z]);
        }
    }

    #[test]
    fn every_preset_fits_the_body_in_any_slot() {
        let phone = [36.0, 75.0, 4.0];
        let tablet = [125.0, 90.0, 3.0];
        for preset in [Preset::ThreeQuarter, Preset::Front, Preset::Side] {
            for back in [false, true] {
                for aspect in [0.5, 1.0, 1.6] {
                    fits(Camera::new(preset, back), phone, aspect);
                    fits(Camera::new(preset, back), tablet, aspect);
                }
            }
        }
    }

    #[test]
    fn the_front_view_looks_straight_at_the_screen_and_fills_the_slot() {
        let cam = Camera::new(Preset::Front, false);
        let half = [36.0, 75.0, 4.0];
        let (vp, eye) = cam.view_proj(([0.0; 3], half), 0.6);
        assert!(eye[0].abs() < 1e-3 && eye[1].abs() < 1e-3 && eye[2] > 0.0);
        // The front face's top edge sits near (not past) the top of the slot.
        let top = camera::apply(&vp, [0.0, 75.0, 4.0]);
        assert!(top[1] > 0.8 && top[1] <= 0.89, "{top:?}");
        // The 3/4 view stands to the right and above; looking at the back, behind.
        let (_, eye) = Camera::new(Preset::ThreeQuarter, false).view_proj(([0.0; 3], half), 0.6);
        assert!(eye[0] > 0.0 && eye[1] > 0.0 && eye[2] > 0.0);
        let (_, eye) = Camera::new(Preset::ThreeQuarter, true).view_proj(([0.0; 3], half), 0.6);
        assert!(eye[2] < 0.0);
    }
}
