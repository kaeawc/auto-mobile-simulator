//! Points and sizes, and the mappings between window, pixel and normalized device coordinates.

/// A 2D point.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// A 2D size.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// Linear interpolation towards `to`; `t` in `0..=1`.
    pub fn lerp(self, to: Point, t: f64) -> Point {
        Point::new(self.x + (to.x - self.x) * t, self.y + (to.y - self.y) * t)
    }
}

impl Size {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }

    /// Maps a point inside this size to `0..=1` on both axes, clamped. A degenerate size maps to 0.
    pub fn normalize(self, p: Point) -> Point {
        let axis = |v: f64, extent: f64| if extent > 0.0 { (v / extent).clamp(0.0, 1.0) } else { 0.0 };
        Point::new(axis(p.x, self.width), axis(p.y, self.height))
    }

    /// The largest size with this aspect ratio that fits inside `bounds`.
    pub fn fit_within(self, bounds: Size) -> Size {
        if self.width <= 0.0 || self.height <= 0.0 {
            return Size::default();
        }
        let scale = (bounds.width / self.width).min(bounds.height / self.height);
        Size::new(self.width * scale, self.height * scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_clamps() {
        let s = Size::new(200.0, 400.0);
        assert_eq!(s.normalize(Point::new(100.0, 100.0)), Point::new(0.5, 0.25));
        assert_eq!(s.normalize(Point::new(-5.0, 900.0)), Point::new(0.0, 1.0));
        assert_eq!(Size::default().normalize(Point::new(1.0, 1.0)), Point::new(0.0, 0.0));
    }

    #[test]
    fn fit_keeps_aspect() {
        let fitted = Size::new(1000.0, 2000.0).fit_within(Size::new(400.0, 400.0));
        assert_eq!(fitted, Size::new(200.0, 400.0));
    }

    #[test]
    fn lerp_midpoint() {
        assert_eq!(Point::new(0.0, 10.0).lerp(Point::new(10.0, 20.0), 0.5), Point::new(5.0, 15.0));
    }
}
