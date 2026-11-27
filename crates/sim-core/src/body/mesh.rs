//! The procedural mesh for a [`Body`]: each panel a rounded box (the outline extruded through the
//! body's thickness, its front and back edges rounded over by the bevel), the screens set flush
//! into its faces with their own UVs, and the bump, buttons and crown as small rounded boxes.
//!
//! One vertex and index buffer per body, split into [`Part`]s by role and panel so the scene picks
//! a material per part and draws a foldable's panel 1 with [`super::HingeAxis::transform`]. The
//! mesh is authored flat (unfolded), in the body's frame (millimetres, right-handed, origin at
//! the centre), triangles wound counter-clockwise seen from outside. A panel's shell and the
//! screens set into it close up into one watertight, outward-facing surface.

use std::collections::HashMap;
use std::f32::consts::FRAC_PI_2;
use std::sync::{Arc, Mutex, OnceLock};

use super::{Body, Display, Face, Side};
use crate::form_factor::Shape;

/// Points per rounded corner, ends included.
const CORNER_STEPS: usize = 8;
/// Steps across each rounded-over edge.
const BEVEL_STEPS: usize = 4;
/// Points around an outline.
const RING: usize = 4 * (CORNER_STEPS + 1);
/// The smallest corner radius drawn, mm: a square corner is rounded this much.
pub const MIN_RADIUS: f32 = 0.3;
/// How far a bump, button or crown sinks into the body, mm, so no seam shows where it sits.
const EMBED: f32 = 0.3;
/// How far a cutout floats off the screen it sits on, mm (draw it after the screen).
pub const CUTOUT_LIFT: f32 = 0.05;

/// Triangles in f32: positions (mm) and unit normals in the body's frame, UVs in 0..1.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    pub parts: Vec<Part>,
}

impl Mesh {
    /// The indices a part draws.
    pub fn part_indices(&self, part: &Part) -> &[u32] {
        &self.indices[part.first as usize..(part.first + part.count) as usize]
    }
}

/// A run of the index buffer: what it draws and which panel it moves with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub role: Role,
    /// 0, or 1 for the half of a foldable that turns about the hinge.
    pub panel: usize,
    pub first: u32,
    pub count: u32,
}

/// What a part is, for its material.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// The body: UVs project its outline onto 0..1.
    Shell,
    /// A screen's glass: UV (0, 0) is the screen's top-left as it shows, v grows down its rows.
    Screen(Display),
    Cutout,
    Bump,
    Button,
    Crown,
}

/// The mesh for `body`, generated (use [`cached_mesh`] in a frame loop).
pub fn mesh(body: &Body) -> Mesh {
    let mut b = Builder::default();
    let t = body.size[2];
    let panels = body.panels();
    let margin = body.bevel + MIN_RADIUS;
    for (pi, panel) in panels.iter().enumerate() {
        let [cx, cy] = panel.center;
        let frame = Frame::at([cx, cy, 0.0]);
        let shell = RoundRect {
            center: [0.0, 0.0],
            half: [panel.size[0] / 2.0, panel.size[1] / 2.0],
            radii: panel.radii,
            exponent: body.corner.exponent,
        };
        // The part of each face's screen on this panel, clear of the edges.
        let inner = [
            cx - panel.size[0] / 2.0 + margin,
            cx + panel.size[0] / 2.0 - margin,
            cy - panel.size[1] / 2.0 + margin,
            cy + panel.size[1] / 2.0 - margin,
        ];
        let piece = |face: Face| {
            body.screens.iter().filter(|s| s.face == face).find_map(|s| {
                let rect = screen_piece(body, s, inner)?;
                Some((s, RoundRect { center: [rect.center[0] - cx, rect.center[1] - cy], ..rect }))
            })
        };
        let (front, back) = (piece(Face::Front), piece(Face::Back));
        b.part(Role::Shell, pi, |b| slab(b, &frame, &shell, t, body.bevel, front.map(|(_, r)| r).as_ref(), back.map(|(_, r)| r).as_ref()));
        for (face, piece) in [(Face::Front, front), (Face::Back, back)] {
            let Some((s, local)) = piece else { continue };
            let [l, r, bottom, top] = s.rect();
            let uv = move |p: [f32; 3]| {
                let u = (p[0] - l) / (r - l);
                [if face == Face::Front { u } else { 1.0 - u }, (top - p[1]) / (top - bottom)]
            };
            let z = if face == Face::Front { t / 2.0 } else { -t / 2.0 };
            b.part(Role::Screen(s.display), pi, |b| fill(b, &frame, &local, z, face == Face::Front, uv));
        }
    }
    for s in &body.screens {
        let Some(c) = s.cutout else { continue };
        let center = [s.center[0] + c.center[0], s.center[1] + c.center[1]];
        let pi = body.panel_at(center);
        let [cx, cy] = panels[pi].center;
        let world =
            RoundRect { center, half: [c.size[0] / 2.0, c.size[1] / 2.0], radii: [c.size[0].min(c.size[1]) / 2.0; 4], exponent: 2.0 };
        let local = RoundRect { center: [center[0] - cx, center[1] - cy], ..world };
        let front = s.face == Face::Front;
        let z = if front { t / 2.0 + CUTOUT_LIFT } else { -t / 2.0 - CUTOUT_LIFT };
        b.part(Role::Cutout, pi, |b| fill(b, &Frame::at([cx, cy, 0.0]), &local, z, front, planar(&world)));
    }
    if let Some(bump) = &body.bump {
        let pi = body.panel_at(bump.center);
        let frame = Frame::back([bump.center[0], bump.center[1], -t / 2.0 + (EMBED - bump.height) / 2.0]);
        let shape = RoundRect {
            center: [0.0, 0.0],
            half: [bump.size[0] / 2.0, bump.size[1] / 2.0],
            radii: [bump.corner.radius; 4],
            exponent: bump.corner.exponent,
        };
        b.part(Role::Bump, pi, |b| slab(b, &frame, &shape, bump.height + EMBED, 0.6, None, None));
    }
    let mut keys: Vec<SideKey> = body
        .buttons
        .iter()
        .map(|k| SideKey {
            role: Role::Button,
            side: k.side,
            at: k.at,
            size: [k.width, k.length],
            radius: k.width.min(k.length) / 2.0 * 0.9,
            depth: k.depth,
            bevel: 0.25,
        })
        .collect();
    if let Some(c) = &body.crown {
        let d = c.diameter;
        keys.push(SideKey { role: Role::Crown, side: c.side, at: c.at, size: [d, d], radius: d / 2.0, depth: c.length, bevel: 0.4 });
    }
    for SideKey { role, side, at, size: [across, along], radius, depth, bevel } in keys {
        let on_side = match side {
            Side::Left => [-body.size[0] / 2.0, at],
            Side::Right => [body.size[0] / 2.0, at],
            Side::Top => [at, body.size[1] / 2.0],
            Side::Bottom => [at, -body.size[1] / 2.0],
        };
        let pi = body.panel_at(on_side);
        let p = panels[pi];
        let depth = depth + EMBED;
        let out = depth / 2.0 - EMBED;
        let origin = match side {
            Side::Left => [p.center[0] - p.size[0] / 2.0 - out, at, 0.0],
            Side::Right => [p.center[0] + p.size[0] / 2.0 + out, at, 0.0],
            Side::Top => [at, p.center[1] + p.size[1] / 2.0 + out, 0.0],
            Side::Bottom => [at, p.center[1] - p.size[1] / 2.0 - out, 0.0],
        };
        let shape = RoundRect { center: [0.0, 0.0], half: [across / 2.0, along / 2.0], radii: [radius; 4], exponent: 2.0 };
        b.part(role, pi, |b| slab(b, &Frame::side(side, origin), &shape, depth, bevel, None, None));
    }
    b.mesh
}

/// A button or crown: a small rounded box standing out of a side.
struct SideKey {
    role: Role,
    side: Side,
    at: f32,
    /// Across the side (along z) and along it.
    size: [f32; 2],
    radius: f32,
    depth: f32,
    bevel: f32,
}

/// [`mesh`], generated once per device type (keyed by the body's id) and shared after: a frame
/// costs a lookup.
pub fn cached_mesh(body: &Body) -> Arc<Mesh> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Mesh>>>> = OnceLock::new();
    let mut map = CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(map.entry(body.id.clone()).or_insert_with(|| Arc::new(mesh(body))))
}

/// The part of screen `s` inside `bounds` (left, right, bottom, top), in body coordinates: its
/// own corners where they fall inside, small corners where a hinge cuts it.
fn screen_piece(body: &Body, s: &super::Screen, [bl, br, bb, bt]: [f32; 4]) -> Option<RoundRect> {
    let [l, r, b, t] = s.rect();
    let (cl, cr, cb, ct) = (l.max(bl), r.min(br), b.max(bb), t.min(bt));
    if cr - cl < 1.0 || ct - cb < 1.0 {
        return None;
    }
    let [w, h] = s.size();
    let (radius, exponent) = match s.shape {
        Shape::Round => (w.min(h) / 2.0, 2.0),
        Shape::Rounded => (s.corner_radius, body.corner.exponent),
        Shape::Rectangle => (MIN_RADIUS, body.corner.exponent),
    };
    let (keep_l, keep_r, keep_b, keep_t) = (cl == l, cr == r, cb == b, ct == t);
    let corner = |a: bool, b: bool| if a && b { radius } else { MIN_RADIUS };
    let radii = [corner(keep_r, keep_t), corner(keep_l, keep_t), corner(keep_l, keep_b), corner(keep_r, keep_b)];
    Some(RoundRect { center: [(cl + cr) / 2.0, (cb + ct) / 2.0], half: [(cr - cl) / 2.0, (ct - cb) / 2.0], radii, exponent })
}

// --- geometry ---

type V3 = [f32; 3];

fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn normalize(v: V3) -> V3 {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len > 0.0 { v.map(|c| c / len) } else { [0.0, 0.0, 1.0] }
}

/// A local frame: an origin and a right-handed basis, local z the way the part extrudes.
#[derive(Clone, Copy)]
struct Frame {
    origin: V3,
    ex: V3,
    ey: V3,
    ez: V3,
}

impl Frame {
    fn at(origin: V3) -> Self {
        Frame { origin, ex: [1.0, 0.0, 0.0], ey: [0.0, 1.0, 0.0], ez: [0.0, 0.0, 1.0] }
    }

    /// Local z out of the back face.
    fn back(origin: V3) -> Self {
        Self::basis(origin, [0.0, 1.0, 0.0], [0.0, 0.0, -1.0])
    }

    /// Local z out of a side, local y along it.
    fn side(side: Side, origin: V3) -> Self {
        match side {
            Side::Left => Self::basis(origin, [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]),
            Side::Right => Self::basis(origin, [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
            Side::Top => Self::basis(origin, [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            Side::Bottom => Self::basis(origin, [1.0, 0.0, 0.0], [0.0, -1.0, 0.0]),
        }
    }

    fn basis(origin: V3, ey: V3, ez: V3) -> Self {
        Frame { origin, ex: cross(ey, ez), ey, ez }
    }

    fn dir(&self, [x, y]: [f32; 2], z: f32) -> V3 {
        [0, 1, 2].map(|i| self.ex[i] * x + self.ey[i] * y + self.ez[i] * z)
    }

    fn point(&self, p: [f32; 2], z: f32) -> V3 {
        let d = self.dir(p, z);
        [0, 1, 2].map(|i| self.origin[i] + d[i])
    }
}

/// A rounded rectangle in a frame's x/y plane, each corner a superellipse quadrant.
#[derive(Clone, Copy, Debug)]
struct RoundRect {
    center: [f32; 2],
    half: [f32; 2],
    /// Top-right, top-left, bottom-left, bottom-right.
    radii: [f32; 4],
    exponent: f32,
}

/// The corners' quadrants, in the order [`RoundRect::radii`] lists them (counter-clockwise).
const QUADRANTS: [[f32; 2]; 4] = [[1.0, 1.0], [-1.0, 1.0], [-1.0, -1.0], [1.0, -1.0]];

impl RoundRect {
    /// The outline shrunk by `inset` about the same corner centres, counter-clockwise from the
    /// top-right corner's right end: [`RING`] points, each with its outward normal. Corners that
    /// meet (a circle) repeat a point; the triangles that makes are dropped.
    fn ring(&self, inset: f32) -> Vec<([f32; 2], [f32; 2])> {
        let n = self.exponent.max(1.0);
        let limit = self.half[0].min(self.half[1]);
        let mut out = Vec::with_capacity(RING);
        for (k, q) in QUADRANTS.iter().enumerate() {
            let r0 = self.radii[k].clamp(MIN_RADIUS.min(limit), limit);
            let c = [self.center[0] + q[0] * (self.half[0] - r0), self.center[1] + q[1] * (self.half[1] - r0)];
            let r = (r0 - inset).max(0.0);
            for j in 0..=CORNER_STEPS {
                let theta = (k as f32 + j as f32 / CORNER_STEPS as f32) * FRAC_PI_2;
                let (s, co) = theta.sin_cos();
                // Each corner keeps to its quadrant, and its ends land exactly on the axes, so
                // corners that meet repeat the same point.
                let snap = |v: f32| if v.abs() < 1e-6 { 0.0 } else { v.abs() };
                let (s, co) = (snap(s) * q[1], snap(co) * q[0]);
                let shape = |v: f32, e: f32| v.signum() * v.abs().powf(e);
                let p = [shape(co, 2.0 / n), shape(s, 2.0 / n)];
                let g = [shape(co, 2.0 * (n - 1.0) / n), shape(s, 2.0 * (n - 1.0) / n)];
                let len = g[0].hypot(g[1]).max(1e-12);
                out.push(([c[0] + r * p[0], c[1] + r * p[1]], [g[0] / len, g[1] / len]));
            }
        }
        out
    }
}

/// UVs projecting `rect`'s bounds onto 0..1, v down.
fn planar(rect: &RoundRect) -> impl Fn(V3) -> [f32; 2] + use<> {
    let (l, top, w, h) = (rect.center[0] - rect.half[0], rect.center[1] + rect.half[1], 2.0 * rect.half[0], 2.0 * rect.half[1]);
    move |p| [(p[0] - l) / w, (top - p[1]) / h]
}

#[derive(Default)]
struct Builder {
    mesh: Mesh,
}

impl Builder {
    fn vertex(&mut self, p: V3, n: V3, uv: [f32; 2]) -> u32 {
        let m = &mut self.mesh;
        m.positions.push(p);
        m.normals.push(normalize(n));
        m.uvs.push(uv.map(|c| c.clamp(0.0, 1.0)));
        (m.positions.len() - 1) as u32
    }

    /// A triangle, unless it has no area (where an outline repeats a point).
    fn tri(&mut self, a: u32, b: u32, c: u32) {
        let p = |i: u32| self.mesh.positions[i as usize];
        let (pa, pb, pc) = (p(a), p(b), p(c));
        let g = cross([0, 1, 2].map(|i| pb[i] - pa[i]), [0, 1, 2].map(|i| pc[i] - pa[i]));
        if g[0] * g[0] + g[1] * g[1] + g[2] * g[2] > 1e-14 {
            self.mesh.indices.extend([a, b, c]);
        }
    }

    fn part(&mut self, role: Role, panel: usize, build: impl FnOnce(&mut Self)) {
        let first = self.mesh.indices.len() as u32;
        build(self);
        let count = self.mesh.indices.len() as u32 - first;
        if count > 0 {
            self.mesh.parts.push(Part { role, panel, first, count });
        }
    }
}

/// A closed rounded box in `frame`: `outline` in the local x/y plane, `depth` along local z
/// (centred), the edges of both caps rounded over by `bevel`. `front` and `back` are holes left in
/// the caps for [`fill`] to close (the screens).
fn slab(b: &mut Builder, frame: &Frame, outline: &RoundRect, depth: f32, bevel: f32, front: Option<&RoundRect>, back: Option<&RoundRect>) {
    let limit = outline.half[0].min(outline.half[1]);
    let bevel = bevel.min(depth / 2.0).min(limit - MIN_RADIUS).max(0.0);
    let outline = RoundRect { radii: outline.radii.map(|r| r.max(bevel + MIN_RADIUS)), ..*outline };
    let uv = planar(&outline);
    // Each ring: inset, z, and how much of its normal points out (xy) and along z.
    let mut rings = Vec::with_capacity(2 * (BEVEL_STEPS + 1));
    for i in 0..=BEVEL_STEPS {
        let (s, c) = (i as f32 / BEVEL_STEPS as f32 * FRAC_PI_2).sin_cos();
        rings.push((bevel * (1.0 - s), depth / 2.0 - bevel * (1.0 - c), s, c));
    }
    for i in 0..=BEVEL_STEPS {
        let (s, c) = (i as f32 / BEVEL_STEPS as f32 * FRAC_PI_2).sin_cos();
        rings.push((bevel * (1.0 - c), -depth / 2.0 + bevel * (1.0 - s), c, -s));
    }
    let base = b.mesh.positions.len() as u32;
    for &(inset, z, out, along) in &rings {
        for (p, n) in outline.ring(inset) {
            let pos = frame.point(p, z);
            b.vertex(pos, frame.dir([n[0] * out, n[1] * out], along), uv(pos_local(frame, pos)));
        }
    }
    let at = |ring: usize, i: usize| base + (ring * RING + i % RING) as u32;
    for ring in 0..rings.len() - 1 {
        for i in 0..RING {
            let (a0, a1, b0, b1) = (at(ring, i), at(ring, i + 1), at(ring + 1, i), at(ring + 1, i + 1));
            b.tri(a0, b0, b1);
            b.tri(a0, b1, a1);
        }
    }
    cap(b, frame, &outline, &uv, at(0, 0), depth / 2.0, true, front);
    cap(b, frame, &outline, &uv, at(rings.len() - 1, 0), -depth / 2.0, false, back);
}

/// A position back in `frame`'s x/y plane.
fn pos_local(frame: &Frame, p: V3) -> V3 {
    let d = [0, 1, 2].map(|i| p[i] - frame.origin[i]);
    let dot = |a: V3| a[0] * d[0] + a[1] * d[1] + a[2] * d[2];
    [dot(frame.ex), dot(frame.ey), dot(frame.ez)]
}

/// Closes a slab's cap ring (`first` its first vertex): a fan, or the band between it and a hole.
#[allow(clippy::too_many_arguments)]
fn cap(
    b: &mut Builder,
    frame: &Frame,
    outline: &RoundRect,
    uv: &impl Fn(V3) -> [f32; 2],
    first: u32,
    z: f32,
    front: bool,
    hole: Option<&RoundRect>,
) {
    let normal = frame.dir([0.0, 0.0], if front { 1.0 } else { -1.0 });
    let o = |i: usize| first + (i % RING) as u32;
    match hole {
        Some(hole) => {
            let s0 = b.mesh.positions.len() as u32;
            for (p, _) in hole.ring(0.0) {
                b.vertex(frame.point(p, z), normal, uv([p[0], p[1], 0.0]));
            }
            let s = |i: usize| s0 + (i % RING) as u32;
            for i in 0..RING {
                if front {
                    b.tri(o(i), o(i + 1), s(i + 1));
                    b.tri(o(i), s(i + 1), s(i));
                } else {
                    b.tri(o(i), s(i + 1), o(i + 1));
                    b.tri(o(i), s(i), s(i + 1));
                }
            }
        }
        None => {
            let c = b.vertex(frame.point(outline.center, z), normal, uv([outline.center[0], outline.center[1], 0.0]));
            for i in 0..RING {
                if front { b.tri(c, o(i), o(i + 1)) } else { b.tri(c, o(i + 1), o(i)) }
            }
        }
    }
}

/// A flat face over `outline` at local `z`, facing local +z (`front`) or -z, with UVs from
/// `uv` (given body positions).
fn fill(b: &mut Builder, frame: &Frame, outline: &RoundRect, z: f32, front: bool, uv: impl Fn(V3) -> [f32; 2]) {
    let normal = frame.dir([0.0, 0.0], if front { 1.0 } else { -1.0 });
    let center = frame.point(outline.center, z);
    let c = b.vertex(center, normal, uv(center));
    for (p, _) in outline.ring(0.0) {
        let pos = frame.point(p, z);
        b.vertex(pos, normal, uv(pos));
    }
    for i in 0..RING {
        let (v0, v1) = (c + 1 + i as u32, c + 1 + ((i + 1) % RING) as u32);
        if front { b.tri(c, v0, v1) } else { b.tri(c, v1, v0) }
    }
}
