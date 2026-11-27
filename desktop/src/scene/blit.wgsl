// The resolved scene into the slot.
struct Blit { rect: vec4f, frame: vec4f, flags: vec4f }
@group(0) @binding(0) var<uniform> bl: Blit;
@group(0) @binding(1) var scene: texture_2d<f32>;
@group(0) @binding(2) var bsamp: sampler;

struct B { @builtin(position) pos: vec4f, @location(0) uv: vec2f }

@vertex fn blit_vs(@builtin(vertex_index) vi: u32) -> B {
    let c = vec2f(f32(vi & 1u), f32(vi >> 1u));
    let l = (bl.rect.xy + c * bl.rect.zw - bl.frame.xy) / bl.frame.zw;
    return B(vec4f(l.x * 2.0 - 1.0, 1.0 - l.y * 2.0, 0.0, 1.0), c);
}

fn encode(x: vec3f) -> vec3f {
    return select(1.055 * pow(x, vec3f(1.0 / 2.4)) - 0.055, x * 12.92, x <= vec3f(0.0031308));
}

@fragment fn blit_fs(b: B) -> @location(0) vec4f {
    let c = textureSample(scene, bsamp, b.uv);
    if (bl.flags.x > 0.5) { return vec4f(encode(c.rgb), c.a); }
    return c;
}
