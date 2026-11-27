// The device body: Blinn-Phong under one directional light and ambient; the live screen shows
// the flat view's picture (sampled from Rune's backdrop copy) under a faint glass highlight.

struct Frame {
    view_proj: mat4x4f,
    eye: vec4f,
    // xyz: towards the light (world); w: ambient.
    light: vec4f,
    // The screen's rect in the backdrop, in its UVs: x, y, w, h.
    uv_rect: vec4f,
    // x: the backdrop holds sRGB-encoded values; y: quarter turns the body is rolled by.
    flags: vec4f,
}

struct Draw {
    model: mat4x4f,
    // rgb: linear base colour; a: 1 for the live screen.
    base: vec4f,
    // x: specular strength, y: shininess.
    params: vec4f,
}

@group(0) @binding(0) var<uniform> fr: Frame;
@group(0) @binding(1) var screen: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(1) @binding(0) var<uniform> dr: Draw;

struct V {
    @builtin(position) pos: vec4f,
    @location(0) world: vec3f,
    @location(1) n: vec3f,
    @location(2) uv: vec2f,
}

@vertex fn vs(@location(0) p: vec3f, @location(1) n: vec3f, @location(2) uv: vec2f) -> V {
    let w = dr.model * vec4f(p, 1.0);
    var o: V;
    o.pos = fr.view_proj * w;
    o.world = w.xyz;
    o.n = (dr.model * vec4f(n, 0.0)).xyz;
    o.uv = uv;
    return o;
}

fn decode(c: vec3f) -> vec3f {
    return select(pow((c + 0.055) / 1.055, vec3f(2.4)), c / 12.92, c <= vec3f(0.04045));
}

// Where a screen-face UV falls in the upright frame with the body rolled q quarters clockwise
// (scene::frame_uv).
fn turned(uv: vec2f, q: f32) -> vec2f {
    if (q < 0.5) { return uv; }
    if (q < 1.5) { return vec2f(1.0 - uv.y, uv.x); }
    if (q < 2.5) { return vec2f(1.0 - uv.x, 1.0 - uv.y); }
    return vec2f(uv.y, 1.0 - uv.x);
}

@fragment fn fs(v: V, @builtin(front_facing) facing: bool) -> @location(0) vec4f {
    let t = clamp(turned(v.uv, fr.flags.y), vec2f(0.0), vec2f(1.0));
    var frame = textureSample(screen, samp, fr.uv_rect.xy + t * fr.uv_rect.zw).rgb;
    if (fr.flags.x > 0.5) { frame = decode(frame); }
    var n = normalize(v.n);
    if (!facing) { n = -n; }
    let l = normalize(fr.light.xyz);
    let e = normalize(fr.eye.xyz - v.world);
    let h = normalize(l + e);
    let spec = pow(max(dot(n, h), 0.0), dr.params.y) * dr.params.x;
    if (dr.base.a > 0.5) {
        return vec4f(frame + vec3f(spec), 1.0);
    }
    let diffuse = max(dot(n, l), 0.0);
    let ambient = fr.light.w;
    return vec4f(dr.base.rgb * (ambient + (1.0 - ambient) * diffuse) + vec3f(spec), 1.0);
}
