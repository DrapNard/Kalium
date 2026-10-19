// SubCraft viewport: metal/roughness PBR with a key light and a procedural sky environment.

// Adapted from SubCraft 71e49fde, crates/render/src/viewport.wgsl.
// Copyright (c) Aalzard and the SubCraft contributors, MIT license.
// Full upstream MIT notice is retained in THIRD_PARTY_NOTICES.md.
// Kalium adaptation: existing mesh layout, derivative tangent frame, bounded
// eight-channel textures, fixed procedural studio, no external environment assets.
struct Globals { view_proj: mat4x4<f32>, eye: vec4<f32> };
@group(0) @binding(0) var<uniform> g: Globals;
@group(1) @binding(0) var samp: sampler;
@group(1) @binding(1) var t_base: texture_2d<f32>;
@group(1) @binding(2) var t_rough: texture_2d<f32>;
@group(1) @binding(3) var t_metal: texture_2d<f32>;
@group(1) @binding(4) var t_normal: texture_2d<f32>;
@group(1) @binding(5) var t_emissive: texture_2d<f32>;
@group(1) @binding(6) var t_height: texture_2d<f32>;
@group(1) @binding(7) var t_ao: texture_2d<f32>;
@group(1) @binding(8) var t_opacity: texture_2d<f32>;

struct VsIn {
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

@vertex
fn vs_main(v: VsIn) -> VsOut {
    var o: VsOut;
    o.clip = g.view_proj * vec4<f32>(v.pos, 1.0);
    o.world = v.pos;
    o.normal = v.normal;
    o.uv = v.uv;
    return o;
}

const PI: f32 = 3.14159265;

fn sky(d: vec3<f32>) -> vec3<f32> {
    let y = clamp(d.y, -1.0, 1.0);
    if (y < 0.0) {
        return mix(vec3<f32>(0.20, 0.22, 0.26), vec3<f32>(0.07, 0.06, 0.05), smoothstep(0.0, 0.35, -y));
    }
    return mix(vec3<f32>(0.20, 0.22, 0.26), vec3<f32>(0.34, 0.40, 0.50), smoothstep(0.0, 0.8, y));
}

// A round studio softbox seen in reflection: angular radius grows with roughness while total
// energy stays roughly constant, so smooth surfaces show a small bright highlight and rough
// surfaces a wide dim one.
fn softbox(r: vec3<f32>, dir: vec3<f32>, radius: f32, intensity: f32, rough: f32) -> f32 {
    let w = radius + rough * rough * 1.6;
    let ang = acos(clamp(dot(r, dir), -1.0, 1.0));
    let falloff = 1.0 - smoothstep(w * 0.6, w, ang);
    return intensity * (radius * radius) / (w * w) * falloff;
}

// Soft-edged band: 1 within `half` of the center, edges widened by `soft`.
fn band(d: f32, half: f32, soft: f32) -> f32 {
    return 1.0 - smoothstep(half - soft, half + soft, d);
}

// Studio environment seen in reflections: a ring of eight light panels around the horizon,
// a round overhead light and the key softbox. Edge softness grows with roughness and panel
// brightness drops as they spread, so every surface reflects crisp shapes when smooth and a
// soft wash when rough, from any viewing angle.
fn studio(r: vec3<f32>, key: vec3<f32>, rough: f32) -> vec3<f32> {
    let soft = 0.02 + rough * 0.75;
    let step = PI / 4.0;
    let az = atan2(r.z, r.x);
    let da = abs(az / step - round(az / step)) * step;
    let el = asin(clamp(r.y, -1.0, 1.0));
    let half_a = 0.27;
    let half_e = 0.36;
    let spread = (half_a * half_e) / ((half_a + soft) * (half_e + soft));
    let panels = band(da, half_a, soft) * band(abs(el - 0.05), half_e, soft) * 1.8 * spread;
    let top_half = 0.35;
    let top = band(acos(clamp(r.y, -1.0, 1.0)), top_half, soft) * 1.2 * (top_half * top_half) / ((top_half + soft) * (top_half + soft));
    let warm = vec3<f32>(1.0, 0.96, 0.9);
    return warm * (panels + top + softbox(r, key, 0.22, 2.0, rough));
}

// Diffuse environment light seen by a normal.
fn irradiance(n: vec3<f32>) -> vec3<f32> {
    return mix(vec3<f32>(0.16, 0.14, 0.12), vec3<f32>(0.55, 0.62, 0.75), n.y * 0.5 + 0.5);
}
fn environment(r: vec3<f32>, key: vec3<f32>, rough: f32) -> vec3<f32> {
    return mix(sky(r), irradiance(r), rough) + studio(r, key, rough);
}

// Analytic split-sum environment BRDF (Karis, "Physically Based Shading on Mobile").
fn env_brdf(f0: vec3<f32>, roughness: f32, nov: f32) -> vec3<f32> {
    let c0 = vec4<f32>(-1.0, -0.0275, -0.572, 0.022);
    let c1 = vec4<f32>(1.0, 0.0425, 1.04, -0.04);
    let r = roughness * c0 + c1;
    let a004 = min(r.x * r.x, exp2(-9.28 * nov)) * r.x + r.y;
    let ab = vec2<f32>(-1.04, 1.04) * a004 + r.zw;
    return f0 * ab.x + ab.y;
}

fn aces(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_main(in: VsOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    let base = textureSample(t_base, samp, in.uv).rgb;
    let rough = clamp(textureSample(t_rough, samp, in.uv).r, 0.04, 1.0);
    let metal = clamp(textureSample(t_metal, samp, in.uv).r, 0.0, 1.0);
    let ntex = textureSample(t_normal, samp, in.uv).rgb * 2.0 - 1.0;
    let emissive = textureSample(t_emissive, samp, in.uv).rgb;
    let ao = clamp(textureSample(t_ao, samp, in.uv).r, 0.0, 1.0);

    // Height -> tangent-space bump normal (central differences, one texel apart). UV v grows
    // downward while the bitangent points up the image (OpenGL convention), hence +dh/dv for y.
    let texel = 1.0 / vec2<f32>(textureDimensions(t_height));
    let h_l = textureSample(t_height, samp, in.uv - vec2<f32>(texel.x, 0.0)).r;
    let h_r = textureSample(t_height, samp, in.uv + vec2<f32>(texel.x, 0.0)).r;
    let h_u = textureSample(t_height, samp, in.uv - vec2<f32>(0.0, texel.y)).r;
    let h_d = textureSample(t_height, samp, in.uv + vec2<f32>(0.0, texel.y)).r;
    let dhdu = (h_r - h_l) / (2.0 * texel.x);
    let dhdv = (h_d - h_u) / (2.0 * texel.y);
    let bump = normalize(vec3<f32>(-dhdu * 0.015, dhdv * 0.015, 1.0));
    // Combine with the normal map (whiteout).
    let nts = normalize(vec3<f32>(ntex.xy + bump.xy, max(ntex.z, 1e-3) * bump.z));

    var n0 = normalize(select(vec3<f32>(0.0, 0.0, 1.0), in.normal, dot(in.normal, in.normal) > 1e-12));
    if (!front) {
        n0 = -n0;
    }
    // Cotangent frame from position/UV derivatives works for imported meshes
    // without tangent streams and respects mirrored UV handedness.
    let px = dpdx(in.world);
    let py = dpdy(in.world);
    let ux = dpdx(in.uv);
    let uy = dpdy(in.uv);
    let determinant = ux.x * uy.y - ux.y * uy.x;
    let sign_uv = select(-1.0, 1.0, determinant >= 0.0);
    let raw_t = (px * uy.y - py * ux.y) * sign_uv;
    let fallback_axis = select(vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), abs(n0.y) > 0.95);
    let fallback_t = normalize(cross(fallback_axis, n0));
    let projected_t = raw_t - n0 * dot(n0, raw_t);
    let t = normalize(select(fallback_t, projected_t, dot(projected_t, projected_t) > 1e-12));
    let b = cross(n0, t) * sign_uv;
    let n = normalize(t * nts.x + b * nts.y + n0 * nts.z);

    let v = normalize(g.eye.xyz - in.world);
    let l = normalize(vec3<f32>(-0.35, 0.72, 0.60));
    let h = normalize(v + l);
    let nol = max(dot(n, l), 0.0);
    let nov = max(dot(n, v), 1e-4);
    let noh = max(dot(n, h), 0.0);
    let voh = max(dot(v, h), 0.0);

    let f0 = mix(vec3<f32>(0.04), base, metal);
    let diffuse = base * (1.0 - metal);

    let a = rough * rough;
    let a2 = a * a;
    let dd = noh * noh * (a2 - 1.0) + 1.0;
    let d = a2 / (PI * dd * dd);
    let k = (rough + 1.0) * (rough + 1.0) / 8.0;
    let gv = nov / (nov * (1.0 - k) + k);
    let gl = nol / (nol * (1.0 - k) + k);
    let f = f0 + (1.0 - f0) * pow(1.0 - voh, 5.0);
    let spec = d * gv * gl * f / max(4.0 * nov * nol, 1e-4);
    let kd = (1.0 - f) * diffuse / PI;
    let light = vec3<f32>(2.5, 2.4, 2.25);
    let direct = (kd + spec) * light * nol;

    let r = reflect(-v, n);
    let env_spec = environment(r, l, rough);
    // Ambient (environment) light is occluded by AO; specular occlusion approximated by AO too.
    let ambient = (diffuse * irradiance(n) + env_brdf(f0, rough, nov) * env_spec) * 0.65 * ao;

    let opacity = textureSample(t_opacity, samp, in.uv).r;
    if (opacity < 0.5) { discard; }
    let color = direct + ambient + emissive;
    return vec4<f32>(aces(color), 1.0);
}
