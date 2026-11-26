//! Foldable hinges as the stock emulator models them (`FoldableModel`, the `hw.sensor.hinge*`,
//! `hw.sensor.posture_list` and `hw.displayRegion.0.1.*` keys of `hardware-properties.ini`): the
//! hinge's angle range and on-screen area, the postures by angle, the posture at which the device
//! counts as folded and the display region it then shows. Also the foldable device definitions
//! new AVDs follow, with the `config.ini` keys avdmanager writes for them.

use serde::{Deserialize, Serialize};

use crate::rotation::Rotation;

/// Stock's posture ids (`hw.sensor.posture_list`: 0 unknown, 1 closed, 2 half-open, 3 open,
/// 4 flipped, 5 tent), as the console's `posture <id>` and the guest's `device_posture` setting
/// number them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StockPosture {
    Unknown,
    Closed,
    HalfOpen,
    Open,
    /// Folded all the way back (a dual-screen or fold-out device showing one side).
    Flipped,
    /// Folded back partway and stood on its edges.
    Tent,
}

impl StockPosture {
    pub const ALL: [StockPosture; 6] = [Self::Unknown, Self::Closed, Self::HalfOpen, Self::Open, Self::Flipped, Self::Tent];

    pub fn id(self) -> u8 {
        self as u8
    }

    pub fn from_id(id: u8) -> Option<StockPosture> {
        Self::ALL.get(usize::from(id)).copied()
    }

    /// Stock's names (`CLOSED`, `HALF_OPENED`, `OPENED`, `TENT`; the UI's "flipped").
    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Closed => "closed",
            Self::HalfOpen => "half-opened",
            Self::Open => "opened",
            Self::Flipped => "flipped",
            Self::Tent => "tent",
        }
    }

    pub fn parse(s: &str) -> Option<StockPosture> {
        if let Ok(id) = s.parse::<u8>() {
            return Self::from_id(id).filter(|p| *p != Self::Unknown);
        }
        match s.to_ascii_lowercase().as_str() {
            "closed" => Some(Self::Closed),
            "half-opened" | "half-open" | "half" | "tabletop" | "book" => Some(Self::HalfOpen),
            "opened" | "open" | "flat" => Some(Self::Open),
            "flipped" => Some(Self::Flipped),
            "tent" => Some(Self::Tent),
            _ => None,
        }
    }
}

/// A rectangle on the screen, in pixels of the unfolded display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One hinge's configuration: what avdmanager writes from a device definition's `<d:hinge>` and
/// `<d:foldable-region>`, and what the stock emulator reads back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hinge {
    /// `hw.sensor.hinge.type`: 1 vertical (the fold runs top to bottom, a book), 0 horizontal (a
    /// flip phone).
    pub vertical: bool,
    /// `hw.sensor.hinge.sub_type`: 1 a physical hinge, 0 a fold in one continuous screen.
    pub physical: bool,
    /// `hw.sensor.hinge.ranges`: the angles the hinge turns through.
    pub range: (f32, f32),
    /// `hw.sensor.hinge.defaults`: the angle a fresh boot starts at.
    pub default: f32,
    /// `hw.sensor.hinge.areas` (`x-y-width-height`): the hinge or fold on the unfolded screen.
    pub area: Option<Rect>,
    /// `hw.sensor.posture_list` with `hw.sensor.hinge_angles_posture_definitions`: each posture
    /// and the angles it spans, in order.
    pub postures: Vec<(StockPosture, f32, f32)>,
    /// `hw.sensor.hinge.fold_to_displayRegion.0.1_at_posture`: the posture at which the device
    /// shows only its folded region.
    pub fold_at: StockPosture,
    /// `hw.displayRegion.0.1.*`: what the screen shows while folded (the outer screen of a
    /// fold-in, one side of a dual-screen or fold-out). None when the AVD has none.
    pub folded: Option<Rect>,
}

impl Hinge {
    /// The hinge `config.ini` describes, if it has one (`hw.sensor.hinge=yes`). Malformed values
    /// fall back to stock's defaults, as stock does ("Incorrect hinge ... default to ...").
    pub fn from_config(get: impl Fn(&str) -> Option<String>) -> Option<Hinge> {
        let get = |k: &str| get(k).map(|v| v.trim().to_owned());
        if !get("hw.sensor.hinge").is_some_and(|v| v == "yes" || v == "true") {
            return None;
        }
        let num = |k: &str| get(k).and_then(|v| v.parse::<i64>().ok());
        let range =
            get("hw.sensor.hinge.ranges").and_then(|v| parse_range(v.split(',').next()?)).filter(|(a, b)| a < b).unwrap_or((0.0, 180.0));
        let default = get("hw.sensor.hinge.defaults")
            .and_then(|v| v.split(',').next()?.trim().parse::<f32>().ok())
            .unwrap_or(range.1)
            .clamp(range.0, range.1);
        let area = get("hw.sensor.hinge.areas").and_then(|v| parse_area(v.split(',').next()?));
        let postures = parse_postures(
            &get("hw.sensor.posture_list").unwrap_or_default(),
            &get("hw.sensor.hinge_angles_posture_definitions").unwrap_or_default(),
        )
        .unwrap_or_else(|| default_postures(range));
        let fold_at = num("hw.sensor.hinge.fold_to_displayRegion.0.1_at_posture")
            .and_then(|n| u8::try_from(n).ok())
            .and_then(StockPosture::from_id)
            .filter(|p| *p != StockPosture::Unknown)
            .unwrap_or(StockPosture::Closed);
        let folded = match (
            num("hw.displayRegion.0.1.xOffset"),
            num("hw.displayRegion.0.1.yOffset"),
            num("hw.displayRegion.0.1.width"),
            num("hw.displayRegion.0.1.height"),
        ) {
            (Some(x), Some(y), Some(w), Some(h)) if x >= 0 && y >= 0 && w > 0 && h > 0 => {
                Some(Rect { x: x as u32, y: y as u32, width: w as u32, height: h as u32 })
            }
            _ => None,
        };
        Some(Hinge {
            vertical: num("hw.sensor.hinge.type") == Some(1),
            physical: num("hw.sensor.hinge.sub_type") == Some(1),
            range,
            default,
            area,
            postures,
            fold_at,
            folded,
        })
    }

    /// The posture at `angle`: the first range holding it (each range includes its start; the
    /// last also its end), else unknown.
    pub fn posture_at(&self, angle: f32) -> StockPosture {
        let last = self.postures.len().saturating_sub(1);
        self.postures
            .iter()
            .enumerate()
            .find(|(i, (_, lo, hi))| angle >= *lo && (angle < *hi || (*i == last && angle <= *hi)))
            .map_or(StockPosture::Unknown, |(_, (p, ..))| *p)
    }

    /// The angle `posture` jumps the hinge to: a closed device shuts all the way, a flipped one
    /// folds all the way back, an open one lies flat (180°) when its range holds it; anything else
    /// takes the middle of its range. None when the device doesn't have the posture.
    pub fn angle_for(&self, posture: StockPosture) -> Option<f32> {
        let (_, lo, hi) = *self.postures.iter().find(|(p, ..)| *p == posture)?;
        Some(match posture {
            StockPosture::Closed => lo,
            StockPosture::Flipped => hi,
            StockPosture::Open if (lo..=hi).contains(&180.0) => 180.0,
            _ => (lo + hi) / 2.0,
        })
    }

    /// The postures this device has, in order.
    pub fn posture_list(&self) -> Vec<StockPosture> {
        self.postures.iter().map(|(p, ..)| *p).collect()
    }

    /// Whether the device shows only its folded region at `angle`.
    pub fn folded_at(&self, angle: f32) -> bool {
        self.folded.is_some() && self.posture_at(angle) == self.fold_at
    }

    /// The region of the unfolded screen shown at `angle`: while folded, where the guest draws a
    /// display forced to the folded region's size ([`letterbox`]), else the whole `screen`.
    pub fn visible(&self, angle: f32, screen: (u32, u32)) -> Rect {
        match self.folded {
            Some(r) if self.folded_at(angle) => letterbox((r.width, r.height), screen),
            _ => Rect { x: 0, y: 0, width: screen.0, height: screen.1 },
        }
    }

    /// The display size the guest lays out for at `angle`: the folded region's while folded.
    pub fn logical_size(&self, angle: f32, screen: (u32, u32)) -> (u32, u32) {
        match self.folded {
            Some(r) if self.folded_at(angle) => (r.width, r.height),
            _ => screen,
        }
    }
}

/// Where Android draws a display whose size is forced to `size` on a `screen` (`wm size`):
/// scaled to fit, keeping its aspect, and centred (`LogicalDisplay.configureDisplayLocked`).
pub fn letterbox((w, h): (u32, u32), (sw, sh): (u32, u32)) -> Rect {
    if w == 0 || h == 0 {
        return Rect { x: 0, y: 0, width: sw, height: sh };
    }
    let (w, h, sw64, sh64) = (u64::from(w), u64::from(h), u64::from(sw), u64::from(sh));
    let (rw, rh) = if sw64 * h < sh64 * w { (sw64, h * sw64 / w) } else { (w * sh64 / h, sh64) };
    Rect { x: ((sw64 - rw) / 2) as u32, y: ((sh64 - rh) / 2) as u32, width: rw as u32, height: rh as u32 }
}

impl Hinge {
    /// Stock's `display_features` global setting for Jetpack WindowManager's emulator support
    /// (`SettingsDisplayFeatureProducer`): `fold-[left,top,right,bottom]` for a fold in one
    /// screen, `hinge-[...]` for a physical hinge. None without a hinge area.
    pub fn display_feature(&self) -> Option<String> {
        let a = self.area?;
        let kind = if self.physical && (a.width > 0 && a.height > 0 && a.width.min(a.height) > 1) { "hinge" } else { "fold" };
        Some(format!("{kind}-[{},{},{},{}]", a.x, a.y, a.x + a.width, a.y + a.height))
    }

    /// The `config.ini` keys avdmanager writes for this hinge (`hw.sensor.hinge.*`,
    /// `hw.sensor.posture_list`, the posture definitions and `hw.displayRegion.0.1.*`), in its
    /// spelling.
    pub fn config(&self) -> Vec<(String, String)> {
        let n = |v: f32| if v.fract() == 0.0 { format!("{v:.0}") } else { v.to_string() };
        let mut out = vec![
            ("hw.sensor.hinge", "yes".to_owned()),
            ("hw.sensor.hinge.count", "1".to_owned()),
            ("hw.sensor.hinge.type", if self.vertical { "1" } else { "0" }.to_owned()),
            ("hw.sensor.hinge.sub_type", if self.physical { "1" } else { "0" }.to_owned()),
            ("hw.sensor.hinge.ranges", format!("{}-{}", n(self.range.0), n(self.range.1))),
            ("hw.sensor.hinge.defaults", n(self.default)),
            ("hw.sensor.posture_list", self.postures.iter().map(|(p, ..)| p.id().to_string()).collect::<Vec<_>>().join(", ")),
            (
                "hw.sensor.hinge_angles_posture_definitions",
                self.postures.iter().map(|(_, lo, hi)| format!("{}-{}", n(*lo), n(*hi))).collect::<Vec<_>>().join(", "),
            ),
            ("hw.sensor.hinge.fold_to_displayRegion.0.1_at_posture", self.fold_at.id().to_string()),
        ];
        if let Some(a) = self.area {
            out.push(("hw.sensor.hinge.areas", format!("{}-{}-{}-{}", a.x, a.y, a.width, a.height)));
        }
        let region = self.folded.map_or((-1, -1, 0, 0), |r| (i64::from(r.x), i64::from(r.y), i64::from(r.width), i64::from(r.height)));
        out.extend([
            ("hw.displayRegion.0.1.xOffset", region.0.to_string()),
            ("hw.displayRegion.0.1.yOffset", region.1.to_string()),
            ("hw.displayRegion.0.1.width", region.2.to_string()),
            ("hw.displayRegion.0.1.height", region.3.to_string()),
        ]);
        out.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
    }
}

fn parse_range(s: &str) -> Option<(f32, f32)> {
    let (a, b) = s.trim().split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn parse_area(s: &str) -> Option<Rect> {
    let v: Vec<u32> = s.trim().split('-').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    match v.as_slice() {
        [x, y, w, h] => Some(Rect { x: *x, y: *y, width: *w, height: *h }),
        _ => None,
    }
}

fn parse_postures(list: &str, defs: &str) -> Option<Vec<(StockPosture, f32, f32)>> {
    let ids: Vec<StockPosture> = list
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().parse::<u8>().ok().and_then(StockPosture::from_id))
        .collect::<Option<_>>()?;
    let ranges: Vec<(f32, f32)> = defs.split(',').filter(|s| !s.trim().is_empty()).map(parse_range).collect::<Option<_>>()?;
    (!ids.is_empty() && ids.len() == ranges.len()).then(|| ids.into_iter().zip(ranges).map(|(p, (lo, hi))| (p, lo, hi)).collect())
}

/// A hinge with no posture definitions: stock's fold-in postures over its range.
fn default_postures((lo, hi): (f32, f32)) -> Vec<(StockPosture, f32, f32)> {
    if (lo, hi) == (0.0, 180.0) {
        vec![(StockPosture::Closed, 0.0, 30.0), (StockPosture::HalfOpen, 30.0, 150.0), (StockPosture::Open, 150.0, 180.0)]
    } else {
        vec![(StockPosture::Open, lo, hi)]
    }
}

/// A running foldable's hinge as simee's emulator reports it (`fold status`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FoldState {
    pub angle: f32,
    pub posture: StockPosture,
    /// Showing only the folded screen.
    pub folded: bool,
    /// Where on the scanout the device shows.
    pub visible: Rect,
    /// The display size the guest lays out for.
    pub width: u32,
    pub height: u32,
    /// The postures the device has.
    pub postures: Vec<StockPosture>,
}

impl FoldState {
    /// The console's reply lines (`angle: 0`, `posture: closed (1)`, `folded: true`,
    /// `visible: 949x1840+629+0`, `size: 1080x2092`, `postures: 1,2,3`); None if any is missing.
    pub fn parse(lines: &[String]) -> Option<FoldState> {
        let field = |name: &str| lines.iter().find_map(|l| l.strip_prefix(name)?.strip_prefix(": ").map(str::trim));
        let size = |s: &str| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        };
        let visible = field("visible")?;
        let (dims, origin) = visible.split_once('+')?;
        let (x, y) = origin.split_once('+')?;
        let (width, height) = size(dims)?;
        let (w, h) = size(field("size")?)?;
        let posture = field("posture")?.split_once('(')?.1.trim_end_matches(')').parse::<u8>().ok().and_then(StockPosture::from_id)?;
        Some(FoldState {
            angle: field("angle")?.parse().ok()?,
            posture,
            folded: field("folded")? == "true",
            visible: Rect { x: x.parse().ok()?, y: y.parse().ok()?, width, height },
            width: w,
            height: h,
            postures: field("postures")?.split(',').filter_map(|p| p.trim().parse::<u8>().ok().and_then(StockPosture::from_id)).collect(),
        })
    }

    /// A point in the guest's layout (what the API's `screen` reports and `tap` takes: a folded
    /// Pixel Fold's 1080x2092, as uiautomator's bounds are) on the frames readers get (the visible
    /// region, 949x1840, origin at its corner). The same point when the two agree (unfolded).
    pub fn guest_to_view(&self, (x, y): (f64, f64)) -> (f64, f64) {
        (rescale(x, self.visible.width, self.width), rescale(y, self.visible.height, self.height))
    }

    /// [`FoldState::guest_to_view`] on a device turned by `rotation`: the guest lays out for its
    /// screen turned (a folded Pixel Fold turned a quarter: 2092x1080) and readers' frames are
    /// the visible region turned (1840x949). The region's place on the scanout doesn't move.
    pub fn guest_to_turned_view(&self, (x, y): (f64, f64), rotation: Rotation) -> (f64, f64) {
        let (vw, vh) = rotation.turned((self.visible.width, self.visible.height));
        let (gw, gh) = rotation.turned((self.width, self.height));
        (rescale(x, vw, gw), rescale(y, vh, gh))
    }

    /// [`FoldState::guest_to_view`] the other way: a point on the frames in the guest's layout.
    pub fn view_to_guest(&self, (x, y): (f64, f64)) -> (f64, f64) {
        (rescale(x, self.width, self.visible.width), rescale(y, self.height, self.visible.height))
    }
}

/// `v` on an axis `from` long, on one `to` long.
fn rescale(v: f64, to: u32, from: u32) -> f64 {
    if from == 0 || to == from { v } else { v * f64::from(to) / f64::from(from) }
}

/// A foldable device definition a new AVD can follow.
#[derive(Clone, Debug, PartialEq)]
pub struct FoldableDevice {
    /// `hw.device.name`.
    pub id: &'static str,
    /// What people call it.
    pub name: &'static str,
    /// The unfolded screen: width, height, dpi.
    pub lcd: (u32, u32, u32),
    pub ram_mb: u64,
    pub hinge: Hinge,
}

impl FoldableDevice {
    /// The `config.ini` keys for this device beyond the screen: its name and its hinge.
    pub fn config(&self) -> Vec<(String, String)> {
        let mut out = vec![("hw.device.name".to_owned(), self.id.to_owned())];
        out.extend(self.hinge.config());
        out
    }
}

fn fold_in_postures() -> Vec<(StockPosture, f32, f32)> {
    default_postures((0.0, 180.0))
}

/// The SDK's Pixel Fold (`pixel_fold` in `sdklib/.../devices/nexus.xml`): a 2208x1840 inner
/// screen at 420 dpi that folds in along a vertical hinge to a 1080x2092 outer screen. The keys
/// are what avdmanager writes for it (`avdmanager create avd -d pixel_fold`).
pub fn pixel_fold() -> FoldableDevice {
    FoldableDevice {
        id: "pixel_fold",
        name: "Pixel Fold",
        lcd: (2208, 1840, 420),
        // The definition's 11444 MiB is a real phone's; simee's AVD default keeps cold boots fast.
        ram_mb: 2048,
        hinge: Hinge {
            vertical: true,
            physical: true,
            range: (0.0, 180.0),
            default: 180.0,
            area: Some(Rect { x: 1080, y: 0, width: 0, height: 1840 }),
            postures: fold_in_postures(),
            fold_at: StockPosture::Closed,
            folded: Some(Rect { x: 0, y: 0, width: 1080, height: 2092 }),
        },
    }
}

/// A Surface Duo-style dual-screen device: two 1350x1800 screens at 401 dpi either side of an
/// 84-pixel physical hinge (2784x1800 in all), turning through 360°. Folded all the way back
/// (flipped) it shows the left screen alone. The SDK has no dual-screen definition; the sizes are
/// Microsoft's published Surface Duo screen and hinge sizes, and the postures are stock's five
/// (closed, half-open, open, flipped, tent) over the full turn.
pub fn dual_screen() -> FoldableDevice {
    FoldableDevice {
        id: "surface_duo",
        name: "Dual-screen (Surface Duo-style)",
        lcd: (2784, 1800, 401),
        ram_mb: 2048,
        hinge: Hinge {
            vertical: true,
            physical: true,
            range: (0.0, 360.0),
            default: 180.0,
            area: Some(Rect { x: 1350, y: 0, width: 84, height: 1800 }),
            postures: vec![
                (StockPosture::Closed, 0.0, 30.0),
                (StockPosture::HalfOpen, 30.0, 150.0),
                (StockPosture::Open, 150.0, 210.0),
                (StockPosture::Tent, 210.0, 330.0),
                (StockPosture::Flipped, 330.0, 360.0),
            ],
            fold_at: StockPosture::Flipped,
            folded: Some(Rect { x: 0, y: 0, width: 1350, height: 1800 }),
        },
    }
}

/// The foldable devices new AVDs can follow, by `hw.device.name` or a short name (`fold`,
/// `pixel-fold`, `duo`, `dual-screen`).
pub fn device(name: &str) -> Option<FoldableDevice> {
    match name.to_ascii_lowercase().replace('-', "_").as_str() {
        "pixel_fold" | "fold" => Some(pixel_fold()),
        "surface_duo" | "duo" | "dual_screen" | "dual" => Some(dual_screen()),
        _ => None,
    }
}

/// Every foldable device definition.
pub fn devices() -> Vec<FoldableDevice> {
    vec![pixel_fold(), dual_screen()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn hinge(pairs: &[(String, String)]) -> Option<Hinge> {
        let map: HashMap<&str, &str> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        Hinge::from_config(|k| map.get(k).map(|v| (*v).to_owned()))
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    /// What `avdmanager create avd -d pixel_fold` wrote (SDK cmdline-tools, emulator 37.2.13).
    fn stock_pixel_fold() -> Vec<(String, String)> {
        kv(&[
            ("hw.device.name", "pixel_fold"),
            ("hw.displayRegion.0.1.height", "2092"),
            ("hw.displayRegion.0.1.width", "1080"),
            ("hw.displayRegion.0.1.xOffset", "0"),
            ("hw.displayRegion.0.1.yOffset", "0"),
            ("hw.sensor.hinge", "yes"),
            ("hw.sensor.hinge.areas", "1080-0-0-1840"),
            ("hw.sensor.hinge.count", "1"),
            ("hw.sensor.hinge.defaults", "180"),
            ("hw.sensor.hinge.fold_to_displayRegion.0.1_at_posture", "1"),
            ("hw.sensor.hinge.ranges", "0-180"),
            ("hw.sensor.hinge.sub_type", "1"),
            ("hw.sensor.hinge.type", "1"),
            ("hw.sensor.hinge_angles_posture_definitions", "0-30, 30-150, 150-180"),
            ("hw.sensor.posture_list", "1, 2, 3"),
        ])
    }

    #[test]
    fn the_pixel_fold_writes_what_avdmanager_writes() {
        let mut ours = pixel_fold().config();
        let mut stock = stock_pixel_fold();
        ours.sort();
        stock.sort();
        assert_eq!(ours, stock);
        assert_eq!(hinge(&stock_pixel_fold()), Some(pixel_fold().hinge), "and reads it back");
    }

    #[test]
    fn postures_follow_the_definitions() {
        let fold = pixel_fold().hinge;
        let at = |a| fold.posture_at(a);
        assert_eq!(
            (at(0.0), at(29.9), at(30.0), at(149.0), at(150.0), at(180.0)),
            (
                StockPosture::Closed,
                StockPosture::Closed,
                StockPosture::HalfOpen,
                StockPosture::HalfOpen,
                StockPosture::Open,
                StockPosture::Open
            )
        );
        assert_eq!(at(200.0), StockPosture::Unknown);
        assert_eq!(fold.angle_for(StockPosture::Closed), Some(0.0));
        assert_eq!(fold.angle_for(StockPosture::HalfOpen), Some(90.0));
        assert_eq!(fold.angle_for(StockPosture::Open), Some(180.0));
        assert_eq!(fold.angle_for(StockPosture::Flipped), None, "a fold-in can't flip");
        for p in fold.posture_list() {
            assert_eq!(fold.posture_at(fold.angle_for(p).expect("angle")), p);
        }
        let duo = dual_screen().hinge;
        assert_eq!(duo.posture_list().len(), 5);
        for p in duo.posture_list() {
            assert_eq!(duo.posture_at(duo.angle_for(p).expect("angle")), p, "{p:?}");
        }
        assert_eq!(duo.angle_for(StockPosture::Flipped), Some(360.0));
    }

    #[test]
    fn folding_shows_the_folded_region() {
        let fold = pixel_fold().hinge;
        assert!(fold.folded_at(0.0) && !fold.folded_at(90.0) && !fold.folded_at(180.0));
        // The outer screen is taller than the inner one: the guest lays out for all of it, drawn
        // scaled to the inner screen's height and centred.
        assert_eq!(fold.logical_size(0.0, (2208, 1840)), (1080, 2092));
        assert_eq!(fold.visible(0.0, (2208, 1840)), Rect { x: 629, y: 0, width: 949, height: 1840 });
        assert_eq!(fold.visible(180.0, (2208, 1840)), Rect { x: 0, y: 0, width: 2208, height: 1840 });
        assert_eq!(fold.logical_size(180.0, (2208, 1840)), (2208, 1840));
        let duo = dual_screen().hinge;
        assert!(duo.folded_at(360.0) && !duo.folded_at(0.0), "a dual-screen shows one side when flipped");
        assert_eq!(duo.visible(359.0, (2784, 1800)), Rect { x: 717, y: 0, width: 1350, height: 1800 });
        // Wider than the screen: letterboxed.
        assert_eq!(letterbox((200, 50), (100, 100)), Rect { x: 0, y: 37, width: 100, height: 25 });
        assert_eq!(letterbox((0, 0), (100, 100)), Rect { x: 0, y: 0, width: 100, height: 100 });
        let mut none = fold.clone();
        none.folded = None;
        assert!(!none.folded_at(0.0));
    }

    #[test]
    fn display_features_name_a_fold_or_a_hinge() {
        assert_eq!(pixel_fold().hinge.display_feature().as_deref(), Some("fold-[1080,0,1080,1840]"));
        assert_eq!(dual_screen().hinge.display_feature().as_deref(), Some("hinge-[1350,0,1434,1800]"));
    }

    #[test]
    fn missing_or_bad_keys_take_stocks_defaults() {
        assert_eq!(hinge(&kv(&[("hw.sensor.hinge", "no")])), None);
        let bare = hinge(&kv(&[("hw.sensor.hinge", "yes")])).expect("hinge");
        assert_eq!(
            (bare.range, bare.default, bare.fold_at, bare.folded, bare.area),
            ((0.0, 180.0), 180.0, StockPosture::Closed, None, None)
        );
        assert_eq!(bare.posture_list(), [StockPosture::Closed, StockPosture::HalfOpen, StockPosture::Open]);
        // What simee's earlier foldables wrote: no region, so nothing to fold the screen to.
        let old = hinge(&kv(&[
            ("hw.sensor.hinge", "yes"),
            ("hw.sensor.hinge.type", "0"),
            ("hw.sensor.hinge.defaults", "90"),
            ("hw.sensor.posture_list", "1,2,3"),
            ("hw.sensor.hinge_angles_posture_definitions", "0-30, 30-150, 150-180"),
            ("hw.displayRegion.0.1.xOffset", "-1"),
            ("hw.displayRegion.0.1.width", "0"),
        ]))
        .expect("hinge");
        assert_eq!((old.default, old.vertical, old.folded), (90.0, false, None));
        let bad = hinge(&kv(&[
            ("hw.sensor.hinge", "yes"),
            ("hw.sensor.posture_list", "1, 2"),
            ("hw.sensor.hinge_angles_posture_definitions", "0-30"),
        ]));
        assert_eq!(bad.expect("hinge").postures.len(), 3, "mismatched lists fall back");
    }

    #[test]
    fn fold_status_parses() {
        let lines: Vec<String> =
            ["angle: 0", "posture: closed (1)", "folded: true", "visible: 949x1840+629+0", "size: 1080x2092", "postures: 1,2,3"]
                .map(String::from)
                .to_vec();
        let s = FoldState::parse(&lines).expect("state");
        assert_eq!((s.angle, s.posture, s.folded, s.width, s.height), (0.0, StockPosture::Closed, true, 1080, 2092));
        assert_eq!(s.visible, Rect { x: 629, y: 0, width: 949, height: 1840 });
        assert_eq!(s.postures.len(), 3);
        assert_eq!(FoldState::parse(&[]), None, "an emulator that doesn't report it");
        assert_eq!(FoldState::parse(&lines[..4]), None);
    }

    #[test]
    fn folded_points_map_between_the_guests_layout_and_the_frames() {
        let lines: Vec<String> =
            ["angle: 0", "posture: closed (1)", "folded: true", "visible: 949x1840+629+0", "size: 1080x2092", "postures: 1,2,3"]
                .map(String::from)
                .to_vec();
        let folded = FoldState::parse(&lines).expect("state");
        // The outer screen's bottom-right corner in uiautomator's bounds is the frame's.
        assert_eq!(folded.guest_to_view((1080.0, 2092.0)), (949.0, 1840.0));
        let (x, y) = folded.guest_to_view((1000.0, 2000.0));
        assert!((x - 878.7).abs() < 0.1 && (y - 1759.1).abs() < 0.1, "({x}, {y})");
        let (gx, gy) = folded.view_to_guest((x, y));
        assert!((gx - 1000.0).abs() < 1e-9 && (gy - 2000.0).abs() < 1e-9, "round trip: ({gx}, {gy})");
        // Unfolded, the guest lays out for the whole screen it shows: points don't move.
        let open = FoldState {
            folded: false,
            visible: Rect { x: 0, y: 0, width: 2208, height: 1840 },
            width: 2208,
            height: 1840,
            ..folded.clone()
        };
        assert_eq!((open.guest_to_view((1000.0, 2000.0)), open.view_to_guest((7.5, 9.0))), ((1000.0, 2000.0), (7.5, 9.0)));
        // Turned, the guest's turned layout maps onto the turned frames: corner to corner.
        for r in Rotation::ALL {
            let (gw, gh) = r.turned((1080, 2092));
            let (vw, vh) = r.turned((949, 1840));
            assert_eq!(folded.guest_to_turned_view((f64::from(gw), f64::from(gh)), r), (f64::from(vw), f64::from(vh)), "{r:?}");
        }
        assert_eq!(folded.guest_to_turned_view((1000.0, 2000.0), Rotation::Deg0), folded.guest_to_view((1000.0, 2000.0)));
        let (x, y) = folded.guest_to_turned_view((2000.0, 1000.0), Rotation::Deg90);
        assert!((x - 1759.1).abs() < 0.1 && (y - 878.7).abs() < 0.1, "({x}, {y})");
    }

    #[test]
    fn postures_parse_by_name_and_stock_id() {
        assert_eq!(StockPosture::parse("1"), Some(StockPosture::Closed));
        assert_eq!(StockPosture::parse("5"), Some(StockPosture::Tent));
        assert_eq!(StockPosture::parse("0"), None);
        assert_eq!(StockPosture::parse("half-opened"), Some(StockPosture::HalfOpen));
        assert_eq!(StockPosture::parse("Flipped"), Some(StockPosture::Flipped));
        assert_eq!(StockPosture::parse("ajar"), None);
        assert_eq!(serde_json::to_value(StockPosture::HalfOpen).expect("json"), "half-open");
        assert_eq!(device("duo").map(|d| d.id), Some("surface_duo"));
        assert_eq!(device("pixel-fold").map(|d| d.lcd), Some((2208, 1840, 420)));
        assert_eq!(device("pixel_9"), None);
    }
}
