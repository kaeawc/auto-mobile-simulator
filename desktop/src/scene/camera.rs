//! The scene's fixed camera: a perspective view of the body from a preset direction, at the
//! distance that fits it in the slot. Column-major 4x4 matrices (`m[column][row]`), as
//! `sim_core::body::HingeAxis::transform` returns them; right-handed, depth 0..1 (wgpu).

/// A column-major 4x4 matrix.
pub type Mat4 = [[f32; 4]; 4];

#[cfg(test)]
pub const IDENTITY: Mat4 = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];

/// `a * b`: `b` applies first.
pub fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    std::array::from_fn(|c| std::array::from_fn(|r| (0..4).map(|k| a[k][r] * b[c][k]).sum()))
}

/// Turns about x by `t` radians (counter-clockwise looking down the axis).
pub fn rot_x(t: f32) -> Mat4 {
    let (s, c) = t.sin_cos();
    [[1.0, 0.0, 0.0, 0.0], [0.0, c, s, 0.0], [0.0, -s, c, 0.0], [0.0, 0.0, 0.0, 1.0]]
}

pub fn rot_y(t: f32) -> Mat4 {
    let (s, c) = t.sin_cos();
    [[c, 0.0, -s, 0.0], [0.0, 1.0, 0.0, 0.0], [s, 0.0, c, 0.0], [0.0, 0.0, 0.0, 1.0]]
}

pub fn rot_z(t: f32) -> Mat4 {
    let (s, c) = t.sin_cos();
    [[c, s, 0.0, 0.0], [-s, c, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]]
}

pub fn translate([x, y, z]: [f32; 3]) -> Mat4 {
    [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [x, y, z, 1.0]]
}

/// A right-handed perspective projection onto wgpu's clip space (depth 0 at `near`, 1 at `far`).
pub fn perspective(fovy: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let f = 1.0 / (fovy / 2.0).tan();
    [[f / aspect, 0.0, 0.0, 0.0], [0.0, f, 0.0, 0.0], [0.0, 0.0, far / (near - far), -1.0], [0.0, 0.0, near * far / (near - far), 0.0]]
}

/// `m` applied to point `p`, divided through by w.
pub fn apply(m: &Mat4, [x, y, z]: [f32; 3]) -> [f32; 3] {
    let v: [f32; 4] = std::array::from_fn(|r| m[0][r] * x + m[1][r] * y + m[2][r] * z + m[3][r]);
    [v[0] / v[3], v[1] / v[3], v[2] / v[3]]
}

/// Where the camera looks from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Preset {
    /// The default: a little to the right of and above the screen, so its right side and top
    /// edge show.
    #[default]
    ThreeQuarter,
    /// Straight at the screen.
    Front,
    /// Edge on from the right and a little behind: the thickness, the side keys and the camera
    /// bump standing off the back.
    Side,
}

impl Preset {
    /// `SIMEE_SCENE_CAMERA`: `front`, `side`, anything else the 3/4 view.
    pub fn parse(s: Option<&str>) -> Preset {
        match s.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("front") => Preset::Front,
            Some("side") => Preset::Side,
            _ => Preset::ThreeQuarter,
        }
    }

    /// Yaw (about y, positive moves the camera to the right) and pitch (positive above),
    /// radians.
    fn angles(self) -> (f32, f32) {
        match self {
            Preset::ThreeQuarter => (28f32.to_radians(), 14f32.to_radians()),
            Preset::Front => (0.0, 0.0),
            Preset::Side => (105f32.to_radians(), 8f32.to_radians()),
        }
    }
}

/// The camera for one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub yaw: f32,
    pub pitch: f32,
    /// Vertical field of view, radians.
    pub fovy: f32,
}

/// What the slot's height (or width, if narrower) the body fills.
const FILL: f32 = 0.88;

impl Camera {
    /// `preset`, looking at the back instead when `back` (the screen shown is on the back face).
    pub fn new(preset: Preset, back: bool) -> Camera {
        let (yaw, pitch) = preset.angles();
        Camera { yaw: if back { yaw + std::f32::consts::PI } else { yaw }, pitch, fovy: 24f32.to_radians() }
    }

    /// The camera's turn, world to view (without the pull-back along z).
    fn turn(&self) -> Mat4 {
        mul(&rot_x(self.pitch), &rot_y(-self.yaw))
    }

    /// How far back from the box's centre the camera stands so the box (`half` extents, mm,
    /// world axes) fills [`FILL`] of a slot of `aspect` (width over height): every corner inside
    /// the frustum.
    pub fn distance(&self, half: [f32; 3], aspect: f32) -> f32 {
        let ty = (self.fovy / 2.0).tan() * FILL;
        let tx = ty * aspect.max(0.01);
        let turn = self.turn();
        let mut d: f32 = 1.0;
        for i in 0..8 {
            let corner = [0, 1, 2].map(|k| if i >> k & 1 == 1 { half[k] } else { -half[k] });
            let [x, y, z] = apply(&turn, corner);
            d = d.max(z + x.abs() / tx).max(z + y.abs() / ty);
        }
        d
    }

    /// A direction given in the camera's frame (x right, y up, z toward the viewer), in the world.
    pub fn world(&self, v: [f32; 3]) -> [f32; 3] {
        apply(&mul(&rot_y(self.yaw), &rot_x(-self.pitch)), v)
    }

    /// Projection times view looking at the box `bounds` (centre and half extents, world mm) in
    /// a slot of `aspect`, and the eye's position (world, mm).
    pub fn view_proj(&self, (center, half): ([f32; 3], [f32; 3]), aspect: f32) -> (Mat4, [f32; 3]) {
        let d = self.distance(half, aspect);
        let reach = (half[0].hypot(half[1]).hypot(half[2]) * 1.5).max(1.0);
        let (near, far) = ((d - reach).max(d * 0.05).max(0.5), d + reach);
        let view = mul(&mul(&translate([0.0, 0.0, -d]), &self.turn()), &translate(center.map(|c| -c)));
        let back = self.world([0.0, 0.0, d]);
        let eye = [center[0] + back[0], center[1] + back[1], center[2] + back[2]];
        (mul(&perspective(self.fovy, aspect, near, far), &view), eye)
    }
}

/// The box around `points` (centre, half extents), or a unit box at the origin for none.
pub fn bounds(points: impl Iterator<Item = [f32; 3]>) -> ([f32; 3], [f32; 3]) {
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for p in points {
        for k in 0..3 {
            lo[k] = lo[k].min(p[k]);
            hi[k] = hi[k].max(p[k]);
        }
    }
    if lo[0] > hi[0] {
        return ([0.0; 3], [1.0; 3]);
    }
    (std::array::from_fn(|k| (lo[k] + hi[k]) / 2.0), std::array::from_fn(|k| (hi[k] - lo[k]) / 2.0))
}
