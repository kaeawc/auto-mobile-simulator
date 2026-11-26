//! How far a device has been turned from its natural orientation, in quarter turns clockwise,
//! and what that does to its frames and touches.
//!
//! The panel never turns: an Android guest that follows the accelerometer draws its turned
//! layout into the same scanout, sideways on it. A viewer holding the turned device sees the
//! panel turned with it, so the stock emulator's window turns the scanout by the same amount.
//! Here a [`Rotation`] says how far to turn a panel frame clockwise to show it upright (the
//! *screen*), and maps points both ways: a touch at screen pixels lands on the panel pixel shown
//! there, which is what the touch screen (fixed to the panel) reports.

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

    /// How far a device is turned whose accelerometer reads `x`, `y` (m/s², device axes: x to
    /// the panel's right, y to its top; a device at rest upright reads +g on y). The axis in the
    /// panel's plane that gravity pulls along most wins, as the stock emulator's coarse
    /// orientation (gRPC `getScreenshot`'s rotation) follows the accelerometer: turned a quarter
    /// clockwise, the panel's right edge points down and x reads -g. A device lying flat, or
    /// read with a tie, is upright.
    pub fn from_gravity(x: f32, y: f32) -> Rotation {
        if x.abs() > y.abs() {
            if x < 0.0 { Self::Deg90 } else { Self::Deg270 }
        } else if y < 0.0 {
            Self::Deg180
        } else {
            Self::Deg0
        }
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
        // What the stock emulator's accelerometer reads after 0..3 `rotate`s (37.2.13), and the
        // turn its gRPC screenshots then have.
        for (x, y, r) in
            [(0.0, 9.77, Rotation::Deg0), (-9.81, 0.0, Rotation::Deg90), (0.0, -9.81, Rotation::Deg180), (9.81, 0.0, Rotation::Deg270)]
        {
            assert_eq!(Rotation::from_gravity(x, y), r, "({x}, {y})");
        }
        assert_eq!(Rotation::from_gravity(0.0, 0.0), Rotation::Deg0, "lying flat");
        assert_eq!(Rotation::from_gravity(-6.0, 7.0), Rotation::Deg0, "tilted, mostly upright");
        assert_eq!(Rotation::Deg90.turned((1080, 2400)), (2400, 1080));
        assert_eq!(Rotation::Deg180.turned((1080, 2400)), (1080, 2400));
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
