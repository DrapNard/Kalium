// Kalium experimental GPU compute brush. Author: DrapNard / contributors.
// Part of the AGPL-3.0-only Kalium source release.
// Each dispatch writes independent texels into one storage buffer; no races.
struct Stamp {
    width: u32,
    height: u32,
    radius: f32,
    opacity: f32,
    hardness: f32,
    center_x: f32,
    center_y: f32,
    erase: u32,
    color: vec4<u32>,
};
@group(0) @binding(0) var<storage, read_write> rgba_pixels: array<u32>;
@group(0) @binding(1) var<uniform> brush: Stamp;

fn byte(value: u32, shift: u32) -> f32 {
    return f32((value >> shift) & 255u);
}
fn pack(r: f32, g: f32, b: f32, a: f32) -> u32 {
    return u32(clamp(round(r), 0.0, 255.0)) |
          (u32(clamp(round(g), 0.0, 255.0)) << 8u) |
          (u32(clamp(round(b), 0.0, 255.0)) << 16u) |
          (u32(clamp(round(a), 0.0, 255.0)) << 24u);
}

@compute @workgroup_size(8,8,1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= brush.width || id.y >= brush.height) { return; }
    let dx = f32(id.x) - brush.center_x;
    let dy = f32(id.y) - brush.center_y;
    let distance = length(vec2<f32>(dx,dy)) / max(brush.radius, 1.0);
    if (distance > 1.0) { return; }
    var feather = 1.0;
    if (distance > brush.hardness) {
        let t = clamp((distance-brush.hardness)/max(1.0-brush.hardness, 0.0001),0.0,1.0);
        feather = 1.0-t*t*(3.0-2.0*t);
    }
    let amount = clamp(brush.opacity*feather,0.0,1.0);
    if (amount <= 0.0) {return;}
    let index = id.y*brush.width+id.x;
    let source = rgba_pixels[index];
    let old_alpha = byte(source,24u)/255.0;
    if (brush.erase != 0u) {
        rgba_pixels[index]=pack(byte(source,0u),byte(source,8u),
                                byte(source,16u),old_alpha*(1.0-amount)*255.0);
        return;
    }
    let alpha = amount + old_alpha*(1.0-amount);
    let denominator = max(alpha,0.000001);
    let r = (f32(brush.color.x)*amount + byte(source,0u)*old_alpha*(1.0-amount))/denominator;
    let g = (f32(brush.color.y)*amount + byte(source,8u)*old_alpha*(1.0-amount))/denominator;
    let b = (f32(brush.color.z)*amount + byte(source,16u)*old_alpha*(1.0-amount))/denominator;
    rgba_pixels[index]=pack(r,g,b,alpha*255.0);
}
