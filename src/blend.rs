//! Blend functions shared by the legacy flat compositor and recursive layer-tree compositor.
//!
//! Formula implementations are adapted from SubCraft's MIT-licensed blend, compositor and
//! normal modules at commit `71e49fde7e4e1dba188a6589c9e5a139d1e3d9d5`; attribution and license
//! text are preserved in `third_party/subcraft` and `THIRD_PARTY_NOTICES.md`.
//!
//! Color callers pass linear RGB; data callers pass normalized channel values. Normal-map
//! functions use encoded tangent-space RGB and return encoded RGB. These operations intentionally
//! omit folder passthrough, channel-disable, and alpha-ignoring replace modes: those need layer
//! graph semantics, not just a portable per-pixel blend function.

use crate::document::BlendMode;

#[inline]
fn safe_div(numerator: f32, denominator: f32) -> f32 {
    numerator
        / if denominator.abs() < 1.0e-6 {
            1.0e-6_f32.copysign(denominator)
        } else {
            denominator
        }
}

fn color_burn(backdrop: f32, source: f32) -> f32 {
    if backdrop >= 1.0 {
        1.0
    } else if source <= 0.0 {
        0.0
    } else {
        1.0 - ((1.0 - backdrop) / source).min(1.0)
    }
}

fn color_dodge(backdrop: f32, source: f32) -> f32 {
    if backdrop <= 0.0 {
        0.0
    } else if source >= 1.0 {
        1.0
    } else {
        (backdrop / (1.0 - source)).min(1.0)
    }
}

fn hard_light(backdrop: f32, source: f32) -> f32 {
    if source <= 0.5 {
        backdrop * 2.0 * source
    } else {
        let s = 2.0 * source - 1.0;
        backdrop + s - backdrop * s
    }
}

fn soft_light(backdrop: f32, source: f32) -> f32 {
    let backdrop = backdrop.max(0.0);
    if source <= 0.5 {
        backdrop - (1.0 - 2.0 * source) * backdrop * (1.0 - backdrop)
    } else {
        let d = if backdrop <= 0.25 {
            ((16.0 * backdrop - 12.0) * backdrop + 4.0) * backdrop
        } else {
            backdrop.sqrt()
        };
        backdrop + (2.0 * source - 1.0) * (d - backdrop)
    }
}

fn separable(mode: BlendMode, backdrop: f32, source: f32) -> f32 {
    match mode {
        BlendMode::Multiply => backdrop * source,
        BlendMode::Add => backdrop + source,
        BlendMode::Divide => safe_div(backdrop, source),
        BlendMode::InverseDivide => safe_div(source, backdrop),
        BlendMode::Darken => backdrop.min(source),
        BlendMode::Lighten => backdrop.max(source),
        BlendMode::Subtract => backdrop - source,
        BlendMode::InverseSubtract => source - backdrop,
        BlendMode::Difference => (backdrop - source).abs(),
        BlendMode::Exclusion => backdrop + source - 2.0 * backdrop * source,
        BlendMode::SignedAddition => backdrop + 2.0 * source - 1.0,
        BlendMode::Overlay => hard_light(source, backdrop),
        BlendMode::Screen => backdrop + source - backdrop * source,
        BlendMode::LinearBurn => backdrop + source - 1.0,
        BlendMode::ColorBurn => color_burn(backdrop, source),
        BlendMode::ColorDodge => color_dodge(backdrop, source),
        BlendMode::SoftLight => soft_light(backdrop, source),
        BlendMode::HardLight => hard_light(backdrop, source),
        BlendMode::VividLight => {
            if source <= 0.5 {
                color_burn(backdrop, 2.0 * source)
            } else {
                color_dodge(backdrop, 2.0 * source - 1.0)
            }
        }
        BlendMode::LinearLight => backdrop + 2.0 * source - 1.0,
        BlendMode::PinLight => {
            if source <= 0.5 {
                backdrop.min(2.0 * source)
            } else {
                backdrop.max(2.0 * source - 1.0)
            }
        }
        _ => source,
    }
}

fn rgb_to_hsv(rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let saturation = if max > 0.0 { delta / max } else { 0.0 };
    let hue = if delta <= 1.0e-8 {
        0.0
    } else if max == r {
        ((g - b) / delta).rem_euclid(6.0) / 6.0
    } else if max == g {
        ((b - r) / delta + 2.0) / 6.0
    } else {
        ((r - g) / delta + 4.0) / 6.0
    };
    [hue, saturation, max]
}

fn hsv_to_rgb(hsv: [f32; 3]) -> [f32; 3] {
    let [h, s, v] = hsv;
    let h6 = h.rem_euclid(1.0) * 6.0;
    let sector = h6.floor() as i32;
    let f = h6 - h6.floor();
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match sector {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

/// Compute a blend result before coverage interpolation. Color values must be linear RGB.
/// Scalar data should be replicated into all three components. Outputs are clamped to 0..1
/// because Kalium's current image storage uses 8-bit channel values.
pub(crate) fn blend_rgb(mode: BlendMode, backdrop: [f32; 3], source: [f32; 3]) -> [f32; 3] {
    let value = match mode {
        BlendMode::Tint | BlendMode::Saturation | BlendMode::Color | BlendMode::Value => {
            let b = rgb_to_hsv(backdrop);
            let s = rgb_to_hsv(source);
            match mode {
                BlendMode::Tint => hsv_to_rgb([s[0], b[1], b[2]]),
                BlendMode::Saturation => hsv_to_rgb([b[0], s[1], b[2]]),
                BlendMode::Color => hsv_to_rgb([s[0], s[1], b[2]]),
                BlendMode::Value => hsv_to_rgb([b[0], b[1], s[2]]),
                _ => unreachable!(),
            }
        }
        BlendMode::Normal
        | BlendMode::Detail
        | BlendMode::NormalMapCombine
        | BlendMode::NormalMapDetail
        | BlendMode::NormalMapInverseDetail => source,
        _ => std::array::from_fn(|i| separable(mode, backdrop[i], source[i])),
    };
    value.map(|v| {
        if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            0.0
        }
    })
}

fn decode_normal(encoded: [f32; 3]) -> [f32; 3] {
    normalize([
        encoded[0] * 2.0 - 1.0,
        encoded[1] * 2.0 - 1.0,
        encoded[2] * 2.0 - 1.0,
    ])
}

fn encode_normal(normal: [f32; 3]) -> [f32; 3] {
    normalize(normal).map(|v| v * 0.5 + 0.5)
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if !length.is_finite() || length < 1.0e-12 {
        [0.0, 0.0, 1.0]
    } else {
        [v[0] / length, v[1] / length, v[2] / length]
    }
}

/// Whiteout normal composition, with encoded tangent-space inputs and output.
pub(crate) fn whiteout_normal(base: [f32; 3], detail: [f32; 3]) -> [f32; 3] {
    let a = decode_normal(base);
    let b = decode_normal(detail);
    encode_normal([a[0] + b[0], a[1] + b[1], a[2] * b[2]])
}

/// Reoriented normal mapping, with encoded tangent-space inputs and output.
pub(crate) fn reoriented_normal(base: [f32; 3], detail: [f32; 3]) -> [f32; 3] {
    let a = decode_normal(base);
    let b = decode_normal(detail);
    let t = [a[0], a[1], a[2] + 1.0];
    let u = [-b[0], -b[1], b[2]];
    if t[2].abs() < 1.0e-6 {
        return encode_normal([-u[0], -u[1], -u[2]]);
    }
    let dot = t[0] * u[0] + t[1] * u[1] + t[2] * u[2];
    encode_normal([
        t[0] * dot / t[2] - u[0],
        t[1] * dot / t[2] - u[1],
        t[2] * dot / t[2] - u[2],
    ])
}

/// Blend an encoded normal pair and interpolate by coverage. `Detail` retains Kalium's
/// historical alpha-weighted detail behavior; the explicit normal-map modes use the standard
/// blend-then-interpolate behavior.
pub(crate) fn blend_normal(
    mode: BlendMode,
    backdrop: [f32; 3],
    source: [f32; 3],
    coverage: f32,
    backdrop_coverage: f32,
) -> [f32; 3] {
    let coverage = if coverage.is_finite() {
        coverage.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let backdrop_coverage = if backdrop_coverage.is_finite() {
        backdrop_coverage.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if coverage <= 0.0 {
        return decode_normal(backdrop);
    }
    let base = decode_normal(backdrop);
    let detail = decode_normal(source);
    if mode == BlendMode::Detail && backdrop_coverage > 1.0e-8 {
        let weighted = normalize([
            detail[0] * coverage,
            detail[1] * coverage,
            1.0 + (detail[2] - 1.0) * coverage,
        ]);
        return decode_normal(reoriented_normal(
            encode_normal(base),
            encode_normal(weighted),
        ));
    }
    if backdrop_coverage <= 1.0e-8 {
        return detail;
    }
    let blended = match mode {
        BlendMode::NormalMapCombine => decode_normal(whiteout_normal(backdrop, source)),
        BlendMode::NormalMapDetail => decode_normal(reoriented_normal(backdrop, source)),
        BlendMode::NormalMapInverseDetail => decode_normal(reoriented_normal(source, backdrop)),
        _ => detail,
    };
    let mixed = [
        (1.0 - backdrop_coverage) * detail[0] + backdrop_coverage * blended[0],
        (1.0 - backdrop_coverage) * detail[1] + backdrop_coverage * blended[1],
        (1.0 - backdrop_coverage) * detail[2] + backdrop_coverage * blended[2],
    ];
    normalize([
        coverage * mixed[0] + (1.0 - coverage) * backdrop_coverage * base[0],
        coverage * mixed[1] + (1.0 - coverage) * backdrop_coverage * base[1],
        coverage * mixed[2] + (1.0 - coverage) * backdrop_coverage * base[2],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(mode: BlendMode, backdrop: f32, source: f32) -> f32 {
        blend_rgb(mode, [backdrop; 3], [source; 3])[0]
    }

    #[test]
    fn legacy_modes_keep_their_original_blend_values() {
        assert!((gray(BlendMode::Multiply, 0.6, 0.5) - 0.3).abs() < 1.0e-6);
        assert!((gray(BlendMode::Add, 0.6, 0.5) - 1.0).abs() < 1.0e-6);
        assert!((gray(BlendMode::Normal, 0.6, 0.5) - 0.5).abs() < 1.0e-6);
        assert!((gray(BlendMode::Detail, 0.6, 0.5) - 0.5).abs() < 1.0e-6);
    }

    #[test]
    fn separable_modes_have_numeric_reference_values() {
        assert!((gray(BlendMode::Screen, 0.6, 0.5) - 0.8).abs() < 1.0e-6);
        assert!((gray(BlendMode::Subtract, 0.6, 0.5) - 0.1).abs() < 1.0e-6);
        assert!((gray(BlendMode::Darken, 0.6, 0.5) - 0.5).abs() < 1.0e-6);
        assert!((gray(BlendMode::Difference, 0.6, 0.5) - 0.1).abs() < 1.0e-6);
        assert!((gray(BlendMode::Overlay, 0.6, 0.5) - 0.6).abs() < 1.0e-6);
        assert!((gray(BlendMode::SoftLight, 0.6, 0.5) - 0.6).abs() < 1.0e-6);
        assert!((gray(BlendMode::SignedAddition, 0.6, 0.5) - 0.6).abs() < 1.0e-6);
        assert!((gray(BlendMode::LinearLight, 0.6, 0.5) - 0.6).abs() < 1.0e-6);
    }

    #[test]
    fn hsv_modes_take_components_from_expected_inputs() {
        let red = [1.0, 0.0, 0.0];
        let blue = [0.0, 0.0, 0.4];
        let value = blend_rgb(BlendMode::Value, red, blue);
        assert!((value[0] - 0.4).abs() < 1.0e-6);
        assert!(value[1].abs() < 1.0e-6 && value[2].abs() < 1.0e-6);
        let tint = blend_rgb(BlendMode::Tint, red, blue);
        assert!(tint[0] < 1.0e-6 && tint[1] < 1.0e-6 && (tint[2] - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn safe_division_stays_finite_and_clamped() {
        assert_eq!(gray(BlendMode::Divide, 1.0, 0.0), 1.0);
        assert_eq!(gray(BlendMode::InverseDivide, 1.0, 0.0), 0.0);
        assert!((gray(BlendMode::Divide, 0.4, 0.5) - 0.8).abs() < 1.0e-6);
    }

    #[test]
    fn normal_map_modes_preserve_unit_length_and_flat_identity() {
        let base = [0.8, 0.5, 0.9];
        let flat = [0.5, 0.5, 1.0];
        for mode in [
            BlendMode::Detail,
            BlendMode::NormalMapCombine,
            BlendMode::NormalMapDetail,
            BlendMode::NormalMapInverseDetail,
        ] {
            let output = blend_normal(mode, base, flat, 0.5, 1.0);
            let decoded = decode_normal(encode_normal(output));
            let length =
                (decoded[0] * decoded[0] + decoded[1] * decoded[1] + decoded[2] * decoded[2])
                    .sqrt();
            assert!((length - 1.0).abs() < 1.0e-6, "{mode:?} -> {output:?}");
            assert!(output.iter().all(|v| v.is_finite()));
        }
        let flat_detail = blend_normal(BlendMode::Detail, base, flat, 0.75, 1.0);
        let decoded_base = decode_normal(base);
        for i in 0..3 {
            assert!((flat_detail[i] - decoded_base[i]).abs() < 1.0e-6);
        }
    }
}
