use std::collections::HashMap;

use super::*;
use crate::fold;
use crate::form_factor::{Kind, matrix};

/// How closely a drawn screen matches simee's pixels (aspect and px/ppi size): 0.1%.
const EXACT: f32 = 0.001;
/// How closely a screen matches the maker's rounded figure (6.27" sold as 6.3"): 3%.
const PUBLISHED: f32 = 0.03;

fn close(a: f32, b: f32, tolerance: f32) -> bool {
    (a - b).abs() <= tolerance * b.abs().max(1e-6)
}

fn kind(platform: Platform, form_factor: FormFactor) -> Kind {
    Kind::new(platform, form_factor).expect("supported")
}

/// An Apple device type as CoreSimulator reports it (screen in pixels).
fn apple_type(id: &str, name: &str, family: &str, (w, h): (u32, u32), scale: u32) -> DeviceTypeInfo {
    DeviceTypeInfo {
        identifier: format!("com.apple.CoreSimulator.SimDeviceType.{id}"),
        name: name.to_owned(),
        model: String::new(),
        product_family: family.to_owned(),
        screen_width: f64::from(w),
        screen_height: f64::from(h),
        screen_scale: f64::from(scale),
        platform: Platform::Ios,
        form_factor: crate::form_factor::apple_family_form_factor(family),
    }
}

/// The profile sim-android gives a new AVD following a foldable definition.
fn foldable_profile(d: &fold::FoldableDevice) -> Profile {
    let mut p = kind(Platform::Android, FormFactor::Foldable).profile().with_screen(d.lcd.0, d.lcd.1, Density::Dpi(d.lcd.2));
    if let (Some(f), Some(r)) = (&mut p.fold, d.hinge.folded) {
        (f.folded_width, f.folded_height) = (r.width, r.height);
    }
    p
}

#[test]
fn every_device_type_simee_lists_gets_a_body() {
    // Every kind's reference profile: a body for phones, tablets, foldables and watches.
    for (form_factor, platform, k) in matrix() {
        let Ok(k) = k else { continue };
        let body = for_profile(&k.profile());
        assert_eq!(body.is_some(), has_body(form_factor), "{form_factor:?} on {platform:?}");
        if let Some(b) = body {
            assert_eq!((b.platform, b.form_factor), (platform, form_factor));
        }
    }
    // The AVD form factors simee creates are rows of the table, marked generic.
    for f in [FormFactor::Phone, FormFactor::Tablet, FormFactor::Foldable, FormFactor::Watch] {
        let b = for_profile(&kind(Platform::Android, f).profile()).expect("body");
        assert!(all().iter().any(|row| Arc::ptr_eq(row, &b)), "{f:?} is a table row");
        assert!(matches!(b.source, Source::Generic(_)), "{f:?}");
    }
    // The foldable definitions new AVDs follow get their published bodies.
    for d in fold::devices() {
        let b = for_android(d.id, &foldable_profile(&d)).expect("body");
        assert_eq!(b.id, d.id);
        assert!(matches!(b.source, Source::Published(_)), "{}", d.id);
    }
    // The Apple device types simee names, and a current line-up.
    for id in [
        "iPhone-17",
        "iPhone-17-Pro",
        "iPhone-17-Pro-Max",
        "iPhone-Air",
        "iPhone-16e",
        "iPhone-16",
        "iPhone-16-Plus",
        "iPhone-16-Pro",
        "iPhone-16-Pro-Max",
        "iPhone-SE-3rd-generation",
        "iPad-Pro-11-inch-M4",
        "iPad-Pro-13-inch-M4",
        "iPad-Air-11-inch-M3",
        "iPad-Air-13-inch-M3",
        "iPad-mini-A17-Pro",
        "iPad-A16",
        "Apple-Watch-Series-10-46mm",
        "Apple-Watch-Series-10-42mm",
        "Apple-Watch-Ultra-2-49mm",
        "Apple-Watch-SE-44mm-2nd-generation",
        "iPhone-Duo",
    ] {
        let b = get(&format!("com.apple.CoreSimulator.SimDeviceType.{id}")).unwrap_or_else(|| panic!("{id} has a body"));
        assert_eq!(b.platform, Platform::Ios, "{id}");
    }
    let mut ids: Vec<&str> = all().iter().map(|b| b.id.as_str()).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), before, "ids are unique");
    for b in all() {
        assert!(!b.source.note().is_empty() && !b.screens.is_empty(), "{}", b.id);
    }
}

#[test]
fn unknown_types_get_a_generic_body_from_the_screen_they_report() {
    let ty = apple_type("iPhone-18-Pro", "iPhone 18 Pro", "iPhone", (1206, 2622), 3);
    let b = for_apple_type(&ty).expect("generic body");
    assert!(matches!(b.source, Source::Generic(_)));
    assert!(b.shows((1206, 2622)));
    assert!(Arc::ptr_eq(&b, &for_apple_type(&ty).expect("again")), "cached");
    // A known type whose reported screen differs from the row's isn't drawn with the row.
    let odd = apple_type("iPhone-17-Pro", "iPhone 17 Pro", "iPhone", (1179, 2556), 3);
    let b = for_apple_type(&odd).expect("generic body");
    assert!(matches!(b.source, Source::Generic(_)) && b.shows((1179, 2556)));
    let known = apple_type("iPhone-17-Pro", "iPhone 17 Pro", "iPhone", (1206, 2622), 3);
    assert_eq!(for_apple_type(&known).map(|b| b.id.clone()).as_deref(), Some("iPhone-17-Pro"));
    assert!(for_apple_type(&apple_type("Apple-TV-4K-3rd-generation-4K", "Apple TV 4K", "Apple TV", (3840, 2160), 1)).is_none());
    // An AVD on a custom screen, and a square watch.
    let custom = kind(Platform::Android, FormFactor::Phone).profile().with_screen(1080, 2400, Density::Dpi(420));
    let b = for_android("custom", &custom).expect("generic body");
    assert!(b.shows((1080, 2400)) && matches!(b.source, Source::Generic(_)));
    let mut square = kind(Platform::Android, FormFactor::Watch).profile();
    square.shape = Shape::Rectangle;
    assert_eq!(for_android("wearos_square", &square).expect("watch").screens[0].shape, Shape::Rectangle);
}

#[test]
fn screens_match_the_screen_simee_reports() {
    for b in all() {
        for s in &b.screens {
            let [w, h] = s.size();
            let what = format!("{} {:?}", b.id, s.display);
            assert!(close(w / h, s.px.0 as f32 / s.px.1 as f32, EXACT), "{what}: aspect");
            assert!(close(w, s.px.0 as f32 * MM_PER_INCH / s.ppi, EXACT), "{what}: size");
            match s.published {
                Some(Published::Diagonal(inches)) => {
                    assert!(close(s.diagonal_inches(), inches, PUBLISHED), "{what}: {} vs {inches} in", s.diagonal_inches())
                }
                Some(Published::Area(mm2)) => assert!(close(w * h, mm2, PUBLISHED), "{what}: {} vs {mm2} mm²", w * h),
                None => assert!(matches!(b.source, Source::Generic(_)), "{what}: a published body publishes its screen"),
            }
            // On the face, inside the rounded-over edge.
            let [l, r, bottom, top] = s.rect();
            let [bw, bh, _] = b.size;
            let m = b.bevel;
            assert!(l >= -bw / 2.0 + m && r <= bw / 2.0 - m && bottom >= -bh / 2.0 + m && top <= bh / 2.0 - m, "{what}: inside the body");
        }
    }
    // The pixels are simee's own: reference profiles, foldable definitions, the iPhone Duo.
    for f in [FormFactor::Phone, FormFactor::Tablet, FormFactor::Foldable, FormFactor::Watch] {
        let p = kind(Platform::Android, f).profile();
        let b = for_profile(&p).expect("body");
        let main = b.screen(Display::Main).or(b.screen(Display::Inner)).expect("screen");
        assert_eq!((main.px, main.ppi, main.shape), ((p.width, p.height), 0.0 + dpi(p.density), p.shape), "{f:?}");
    }
    for d in fold::devices() {
        let b = get(d.id).expect("body");
        assert_eq!(b.screen(Display::Inner).map(|s| s.px), Some((d.lcd.0, d.lcd.1)), "{}", d.id);
        if d.hinge.fold_at == fold::StockPosture::Closed {
            let r = d.hinge.folded.expect("outer");
            assert_eq!(b.screen(Display::Outer).map(|s| (s.px, s.face)), Some(((r.width, r.height), Face::Back)), "{}", d.id);
        }
    }
    for (id, f) in [
        ("iPhone-17-Pro", FormFactor::Phone),
        ("iPad-Pro-11-inch-M4", FormFactor::Tablet),
        ("Apple-Watch-Series-10-46mm", FormFactor::Watch),
    ] {
        let p = kind(Platform::Ios, f).profile();
        assert!(get(id).expect("row").shows((p.width, p.height)), "{id} shows the {f:?} reference screen");
    }
    let duo = get("iPhone-Duo").expect("duo");
    assert_eq!(duo.screen(Display::Outer).map(|s| s.px), Some((1398, 2034)));
    assert!(duo.shows((2007, 2853)), "the inner LCD, stored portrait");
}

fn dpi(d: Density) -> f32 {
    match d {
        Density::Dpi(v) => v as f32,
        Density::Scale(_) => f32::NAN,
    }
}

// --- mesh ---

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// The volume the parts enclose if, with positions welded, every edge is shared by exactly two
/// triangles that run it in opposite directions (closed and consistently wound).
fn closed_volume(m: &Mesh, parts: &[Part]) -> Result<f32, String> {
    let mut ids: HashMap<[i64; 3], usize> = HashMap::new();
    let mut edges: HashMap<(usize, usize), u32> = HashMap::new();
    let mut volume = 0.0;
    for part in parts {
        for t in m.part_indices(part).chunks(3) {
            let p = [0, 1, 2].map(|i| m.positions[t[i] as usize]);
            let id = p.map(|q| {
                let key = q.map(|c| (f64::from(c) * 1000.0).round() as i64);
                let next = ids.len();
                *ids.entry(key).or_insert(next)
            });
            if id[0] == id[1] || id[1] == id[2] || id[0] == id[2] {
                continue;
            }
            for (a, b) in [(id[0], id[1]), (id[1], id[2]), (id[2], id[0])] {
                *edges.entry((a, b)).or_default() += 1;
            }
            volume += dot(p[0], cross(p[1], p[2])) / 6.0;
        }
    }
    for (&(a, b), &n) in &edges {
        if n != 1 || edges.get(&(b, a)) != Some(&1) {
            return Err(format!("edge {a}-{b} used {n} times, back {:?}", edges.get(&(b, a))));
        }
    }
    Ok(volume)
}

fn bodies_to_mesh() -> Vec<Arc<Body>> {
    let mut out = all().to_vec();
    out.push(for_apple_type(&apple_type("iPhone-18-Pro", "iPhone 18 Pro", "iPhone", (1206, 2622), 3)).expect("generic"));
    let mut square = kind(Platform::Android, FormFactor::Watch).profile();
    square.shape = Shape::Rectangle;
    out.push(for_profile(&square).expect("square watch"));
    out
}

#[test]
fn meshes_are_well_formed() {
    for b in bodies_to_mesh() {
        let m = mesh(&b);
        let id = &b.id;
        let n = m.positions.len();
        assert!(n > 0 && m.normals.len() == n && m.uvs.len() == n, "{id}");
        assert_eq!(m.indices.len() % 3, 0, "{id}");
        assert!(m.indices.iter().all(|&i| (i as usize) < n), "{id}: indices in range");
        assert_eq!(m.parts.iter().map(|p| p.count as usize).sum::<usize>(), m.indices.len(), "{id}: parts cover the indices");
        assert!(m.normals.iter().all(|v| (dot(*v, *v).sqrt() - 1.0).abs() < 1e-4), "{id}: unit normals");
        assert!(m.uvs.iter().all(|uv| uv.iter().all(|c| (0.0..=1.0).contains(c))), "{id}: uvs in 0..1");
        let panels = b.panels().len();
        for panel in 0..panels {
            // The shell and the screens set into it close up, and face out.
            let outer: Vec<Part> =
                m.parts.iter().filter(|p| p.panel == panel && matches!(p.role, Role::Shell | Role::Screen(_))).copied().collect();
            assert_eq!(outer.iter().filter(|p| p.role == Role::Shell).count(), 1, "{id}: one shell per panel");
            let v = closed_volume(&m, &outer).unwrap_or_else(|e| panic!("{id} panel {panel}: {e}"));
            let p = b.panels()[panel];
            let slab = p.size[0] * p.size[1] * b.size[2];
            assert!(v > 0.6 * slab && v <= slab * 1.001, "{id} panel {panel}: volume {v} of {slab}");
        }
        for part in m.parts.iter().filter(|p| matches!(p.role, Role::Bump | Role::Button | Role::Crown)) {
            let v = closed_volume(&m, &[*part]).unwrap_or_else(|e| panic!("{id} {:?}: {e}", part.role));
            assert!(v > 0.0, "{id} {:?} faces out", part.role);
        }
        // Each triangle winds the way its normals face.
        for part in &m.parts {
            for t in m.part_indices(part).chunks(3) {
                let [a, b2, c] = [0, 1, 2].map(|i| t[i] as usize);
                let g = cross(sub(m.positions[b2], m.positions[a]), sub(m.positions[c], m.positions[a]));
                if dot(g, g) < 1e-12 {
                    continue;
                }
                let avg = [0, 1, 2].map(|k| m.normals[a][k] + m.normals[b2][k] + m.normals[c][k]);
                assert!(dot(g, avg) > 0.0, "{id} {:?}: a triangle faces in", part.role);
            }
        }
        // Each screen's UVs span 0..1 (across both panels of a foldable's inner screen), with v
        // growing down the screen, and its face covers the screen's rect.
        for s in &b.screens {
            let verts: Vec<usize> = m
                .parts
                .iter()
                .filter(|p| p.role == Role::Screen(s.display))
                .flat_map(|p| m.part_indices(p).iter().map(|&i| i as usize))
                .collect();
            assert!(!verts.is_empty(), "{id} {:?} is drawn", s.display);
            let span = |f: &dyn Fn(usize) -> f32| verts.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &i| (lo.min(f(i)), hi.max(f(i))));
            for k in 0..2 {
                let (lo, hi) = span(&|i| m.uvs[i][k]);
                assert!(lo.abs() < 1e-4 && (hi - 1.0).abs() < 1e-4, "{id} {:?}: uv {k} spans {lo}..{hi}", s.display);
            }
            let top = verts.iter().copied().max_by(|&i, &j| m.positions[i][1].total_cmp(&m.positions[j][1])).expect("top");
            assert!(m.uvs[top][1] < 1e-4, "{id} {:?}: v is 0 at the top", s.display);
            let [l, r, bottom, t] = s.rect();
            let (x0, x1) = span(&|i| m.positions[i][0]);
            let (y0, y1) = span(&|i| m.positions[i][1]);
            assert!([x0 - l, x1 - r, y0 - bottom, y1 - t].iter().all(|d| d.abs() < 1e-3), "{id} {:?}: face is the rect", s.display);
            let z = m.positions[verts[0]][2];
            let face = if s.face == Face::Front { b.size[2] / 2.0 } else { -b.size[2] / 2.0 };
            assert!((z - face).abs() < 1e-4, "{id} {:?}: on its face", s.display);
        }
    }
}

#[test]
fn meshes_are_cached_per_device_type() {
    let b = get("iPhone-17-Pro").expect("row");
    let first = cached_mesh(&b);
    assert!(Arc::ptr_eq(&first, &cached_mesh(&b)), "the same mesh");
    assert!(Arc::ptr_eq(&b, &get("iPhone-17-Pro").expect("row")), "the same body");
    assert_eq!(*first, mesh(&b));
    let other = cached_mesh(&get("iPhone-17").expect("row"));
    assert!(!Arc::ptr_eq(&first, &other));
}

fn apply(m: &[[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|i| m[0][i] * p[0] + m[1][i] * p[1] + m[2][i] * p[2] + m[3][i])
}

#[test]
fn foldables_have_two_panels_and_a_hinge() {
    for (id, range) in
        [("pixel_fold", fold::pixel_fold().hinge.range), ("surface_duo", fold::dual_screen().hinge.range), ("iPhone-Duo", (0.0, 180.0))]
    {
        let b = get(id).expect("foldable");
        let f = b.fold.expect("a fold");
        assert!(f.hinge.vertical && f.hinge.range == range, "{id}");
        let [p0, p1] = f.panels;
        let (w, h, t) = (b.size[0], b.size[1], b.size[2]);
        assert!(close(p0.size[0] + p1.size[0], w, EXACT) && close(p0.size[1], h, EXACT), "{id}: the panels make the body");
        assert!(p0.center[0] < f.hinge.at && p1.center[0] > f.hinge.at, "{id}: one either side");
        let m = mesh(&b);
        for panel in [0, 1] {
            assert!(m.parts.iter().any(|p| p.panel == panel && p.role == Role::Screen(Display::Inner)), "{id}: inner screen on {panel}");
        }
        // Flat is where the mesh is authored; closed folds panel 1 onto panel 0, screen to screen.
        let flat = f.hinge.transform(HingeAxis::FLAT);
        let ident = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
        assert!(flat.iter().flatten().zip(ident.iter().flatten()).all(|(a, b)| (a - b).abs() < 1e-6), "{id}: flat");
        let closed = f.hinge.transform(range.0);
        let front = [p1.center[0], p1.center[1], t / 2.0];
        let q = apply(&closed, front);
        assert!((q[0] - (2.0 * f.hinge.at - front[0])).abs() < 1e-3 && (q[2] - t / 2.0).abs() < 1e-3, "{id}: closed {q:?}");
        let back = apply(&closed, [p1.center[0], 0.0, -t / 2.0]);
        assert!((back[2] - 1.5 * t).abs() < 1e-3, "{id}: panel 1's back is on top {back:?}");
        if range.1 > 180.0 {
            // Folded all the way back, the backs meet.
            let q = apply(&f.hinge.transform(range.1), [p1.center[0], 0.0, -t / 2.0]);
            assert!((q[0] - (2.0 * f.hinge.at - p1.center[0])).abs() < 1e-3 && (q[2] + t / 2.0).abs() < 1e-3, "{id}: flipped {q:?}");
        }
        // Half open, panel 1 stands up off the front.
        let up = apply(&f.hinge.transform(90.0), [p1.center[0], 0.0, t / 2.0]);
        assert!(up[2] > t && (up[0] - f.hinge.at).abs() < 1e-3, "{id}: half open {up:?}");
    }
    let pf = get("pixel_fold").expect("pixel fold");
    let outer = pf.screen(Display::Outer).expect("outer screen");
    assert_eq!((outer.face, pf.panel_at(outer.center)), (Face::Back, 0), "the cover screen is on the back of the left panel");
    assert!(get("iPhone-17").expect("phone").fold.is_none());
}
