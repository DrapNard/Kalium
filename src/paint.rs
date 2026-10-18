use crate::document::{Channel, Document};
use crate::uv::UvCoverage;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum BrushMode {
    Paint,
    Erase,
    MaskWhite,
    MaskBlack,
    Sample,
    Bucket,
}

impl BrushMode {
    pub const ALL: [Self; 6] = [
        Self::Paint,
        Self::Erase,
        Self::MaskWhite,
        Self::MaskBlack,
        Self::Sample,
        Self::Bucket,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Paint => "Paint",
            Self::Erase => "Erase",
            Self::MaskWhite => "Mask +",
            Self::MaskBlack => "Mask −",
            Self::Sample => "Eyedropper",
            Self::Bucket => "Flood Fill",
        }
    }
    pub fn is_mask(self) -> bool {
        matches!(self, Self::MaskWhite | Self::MaskBlack)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Brush {
    pub radius: u32,
    pub opacity: f32,
    pub hardness: f32,
    pub spacing: f32,
    pub mode: BrushMode,
    pub color: [u8; 4],
    pub grayscale: u8,
    pub mirror_u: bool,
    #[serde(default)]
    pub tolerance: u8,
}

impl Default for Brush {
    fn default() -> Self {
        Self {
            radius: 22,
            opacity: 0.85,
            hardness: 0.65,
            spacing: 0.25,
            mode: BrushMode::Paint,
            color: [225, 99, 137, 255],
            grayscale: 175,
            mirror_u: false,
            tolerance: 0,
        }
    }
}

impl Brush {
    pub fn validate(&self) -> Result<()> {
        if !(1..=4096).contains(&self.radius) {
            bail!("Brush radius must be 1..=4096");
        }
        if !self.opacity.is_finite() || !(0.0..=1.0).contains(&self.opacity) {
            bail!("Invalid opacity");
        }
        if !self.hardness.is_finite() || !(0.0..=1.0).contains(&self.hardness) {
            bail!("Invalid hardness");
        }
        if !self.spacing.is_finite() || !(0.1..=4.0).contains(&self.spacing) {
            bail!("Invalid spacing");
        }
        Ok(())
    }
    #[allow(dead_code)] // Public API for future brush plugins; GUI uses the UV-aware variant.
    pub fn stamp(&self, doc: &mut Document, channel: Channel, uv: [f32; 2]) {
        self.stamp_masked(doc, channel, uv, None);
    }
    pub fn stamp_masked(
        &self,
        doc: &mut Document,
        channel: Channel,
        uv: [f32; 2],
        coverage: Option<&UvCoverage>,
    ) {
        self.stamp_masked_recording(doc, channel, uv, coverage, &mut |_, _, _, _, _| true);
    }

    fn stamp_masked_recording<F>(
        &self,
        doc: &mut Document,
        channel: Channel,
        uv: [f32; 2],
        coverage: Option<&UvCoverage>,
        before_rect: &mut F,
    ) -> bool
    where
        F: FnMut(&Document, u32, u32, u32, u32) -> bool,
    {
        if matches!(self.mode, BrushMode::Sample | BrushMode::Bucket) {
            return true;
        }
        if uv.iter().any(|v| !v.is_finite()) {
            return true;
        }
        let size = doc.size;
        let radius = self.radius.max(1) as f32;
        let cx = uv[0] * (size - 1) as f32;
        let cy = uv[1] * (size - 1) as f32;
        let min_x = (cx - radius - 1.0).floor().max(0.0) as u32;
        let min_y = (cy - radius - 1.0).floor().max(0.0) as u32;
        let max_x = (cx + radius + 1.0).ceil().min((size - 1) as f32) as u32;
        let max_y = (cy + radius + 1.0).ceil().min((size - 1) as f32) as u32;
        if min_x <= max_x && min_y <= max_y && !before_rect(doc, min_x, min_y, max_x, max_y) {
            return false;
        }
        let mode = self.mode;
        let hardness = self.hardness.clamp(0.0, 1.0);
        let target = if channel.is_color() {
            self.color
        } else {
            [self.grayscale, self.grayscale, self.grayscale, 255]
        };
        let layer = doc.selected_layer_mut();
        if mode.is_mask() {
            for y in min_y..=max_y {
                for x in min_x..=max_x {
                    if coverage.is_some_and(|cover| !cover.contains(x, y)) {
                        continue;
                    }
                    let dx = x as f32 - cx;
                    let dy = y as f32 - cy;
                    let distance = (dx * dx + dy * dy).sqrt() / radius;
                    if distance > 1.0 {
                        continue;
                    }
                    let feather = if distance <= hardness {
                        1.0
                    } else {
                        let t =
                            ((distance - hardness) / (1.0 - hardness).max(0.0001)).clamp(0.0, 1.0);
                        1.0 - t * t * (3.0 - 2.0 * t)
                    };
                    let a = (self.opacity * feather).clamp(0.0, 1.0);
                    if a <= 0.0 {
                        continue;
                    }
                    let pixel = layer.mask.get_pixel_mut(x, y);
                    let target = if mode == BrushMode::MaskWhite {
                        255.0
                    } else {
                        0.0
                    };
                    pixel.0[0] = (pixel.0[0] as f32 * (1.0 - a) + target * a).round() as u8;
                }
            }
        }
        if !mode.is_mask() {
            let image = layer.pixels_mut(channel, size);
            for y in min_y..=max_y {
                for x in min_x..=max_x {
                    if coverage.is_some_and(|cover| !cover.contains(x, y)) {
                        continue;
                    }
                    let dx = x as f32 - cx;
                    let dy = y as f32 - cy;
                    let distance = (dx * dx + dy * dy).sqrt() / radius;
                    if distance > 1.0 {
                        continue;
                    }
                    let feather = if distance <= hardness {
                        1.0
                    } else {
                        let t =
                            ((distance - hardness) / (1.0 - hardness).max(0.0001)).clamp(0.0, 1.0);
                        1.0 - t * t * (3.0 - 2.0 * t)
                    };
                    let a = (self.opacity * feather).clamp(0.0, 1.0);
                    if a <= 0.0 {
                        continue;
                    }
                    let pixel = image.get_pixel_mut(x, y);
                    if mode == BrushMode::Erase {
                        pixel.0[3] = (pixel.0[3] as f32 * (1.0 - a)).round() as u8;
                    } else if channel == Channel::Normal {
                        let source_alpha = a * (target[3] as f32 / 255.0);
                        let destination_alpha = pixel.0[3] as f32 / 255.0;
                        let output_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
                        if output_alpha <= 0.0 {
                            continue;
                        }
                        let source = decode_normal(&target[..3]);
                        let destination = decode_normal(&pixel.0[..3]);
                        let blended = normalize3([
                            (source[0] * source_alpha
                                + destination[0] * destination_alpha * (1.0 - source_alpha))
                                / output_alpha,
                            (source[1] * source_alpha
                                + destination[1] * destination_alpha * (1.0 - source_alpha))
                                / output_alpha,
                            (source[2] * source_alpha
                                + destination[2] * destination_alpha * (1.0 - source_alpha))
                                / output_alpha,
                        ]);
                        encode_normal(blended, &mut pixel.0[..3]);
                        pixel.0[3] = (output_alpha * 255.0).round() as u8;
                    } else if matches!(channel, Channel::BaseColor | Channel::Emissive) {
                        // Base Color and Emissive pixels store straight-alpha,
                        // sRGB-encoded color.
                        // Composite in linear light, then convert the resulting straight
                        // color back to sRGB for storage.
                        let source_alpha = a * (target[3] as f32 / 255.0);
                        let destination_alpha = pixel.0[3] as f32 / 255.0;
                        let output_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
                        if output_alpha <= 0.0 {
                            continue;
                        }
                        for (c, target_channel) in target.iter().take(3).enumerate() {
                            let source = srgb_to_linear(*target_channel);
                            let destination = srgb_to_linear(pixel.0[c]);
                            let output = (source * source_alpha
                                + destination * destination_alpha * (1.0 - source_alpha))
                                / output_alpha;
                            pixel.0[c] = linear_to_srgb(output);
                        }
                        pixel.0[3] = (output_alpha * 255.0).round() as u8;
                    } else {
                        let old_a = pixel.0[3] as f32 / 255.0;
                        let new_a = a + old_a * (1.0 - a);
                        for (c, target_channel) in target.iter().take(3).enumerate() {
                            pixel.0[c] = ((*target_channel as f32 * a
                                + pixel.0[c] as f32 * old_a * (1.0 - a))
                                / new_a.max(1e-6))
                            .round()
                            .clamp(0.0, 255.0) as u8;
                        }
                        pixel.0[3] = (new_a * 255.0).round() as u8;
                    }
                }
            }
        }
        true
    }

    #[allow(dead_code)] // Public API for callers without an atlas coverage mask.
    pub fn stroke(
        &self,
        doc: &mut Document,
        channel: Channel,
        from: Option<[f32; 2]>,
        to: [f32; 2],
    ) {
        self.stroke_masked(doc, channel, from, to, None);
    }
    pub fn stroke_masked(
        &self,
        doc: &mut Document,
        channel: Channel,
        from: Option<[f32; 2]>,
        to: [f32; 2],
        coverage: Option<&UvCoverage>,
    ) {
        self.stroke_masked_recording(doc, channel, from, to, coverage, |_, _, _, _, _| true);
    }

    pub fn stroke_masked_recording<F>(
        &self,
        doc: &mut Document,
        channel: Channel,
        from: Option<[f32; 2]>,
        to: [f32; 2],
        coverage: Option<&UvCoverage>,
        mut before_rect: F,
    ) where
        F: FnMut(&Document, u32, u32, u32, u32) -> bool,
    {
        let mut stamp = |uv: [f32; 2], doc: &mut Document| {
            if !self.stamp_masked_recording(doc, channel, uv, coverage, &mut before_rect) {
                return false;
            }
            if self.mirror_u {
                return self.stamp_masked_recording(
                    doc,
                    channel,
                    [1.0 - uv[0], uv[1]],
                    coverage,
                    &mut before_rect,
                );
            }
            true
        };
        let Some(from) = from else {
            stamp(to, doc);
            return;
        };
        let du = from[0] - to[0];
        let dv = from[1] - to[1];
        let dist = (du * du + dv * dv).sqrt() * doc.size as f32;
        // UV islands must not be connected by a long cross-atlas stroke.
        if dist > doc.size as f32 * 0.24 {
            stamp(to, doc);
            return;
        }
        let spacing = (self.radius.max(1) as f32 * self.spacing.max(0.1)).max(1.0);
        let steps = (dist / spacing).ceil().clamp(1.0, 1024.0) as usize;
        for i in 1..=steps {
            let t = i as f32 / steps as f32;
            if !stamp(
                [
                    from[0] + (to[0] - from[0]) * t,
                    from[1] + (to[1] - from[1]) * t,
                ],
                doc,
            ) {
                return;
            }
        }
    }
}

fn srgb_to_linear(encoded: u8) -> f32 {
    let value = encoded as f32 / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(linear: f32) -> u8 {
    let value = linear.clamp(0.0, 1.0);
    let encoded = if value <= 0.0031308 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

fn decode_normal(encoded: &[u8]) -> [f32; 3] {
    normalize3([
        encoded[0] as f32 / 127.5 - 1.0,
        encoded[1] as f32 / 127.5 - 1.0,
        encoded[2] as f32 / 127.5 - 1.0,
    ])
}

fn normalize3(vector: [f32; 3]) -> [f32; 3] {
    let length_squared = vector.iter().map(|value| value * value).sum::<f32>();
    if length_squared <= 1.0e-12 {
        [0.0, 0.0, 1.0]
    } else {
        let inverse_length = length_squared.sqrt().recip();
        vector.map(|value| value * inverse_length)
    }
}

fn encode_normal(normal: [f32; 3], encoded: &mut [u8]) {
    for axis in 0..3 {
        encoded[axis] = ((normal[axis] * 0.5 + 0.5) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn painting_changes_only_pixels_in_brush() {
        let mut doc = Document::new(64);
        let brush = Brush {
            radius: 5,
            opacity: 1.0,
            hardness: 1.0,
            ..Brush::default()
        };
        brush.stamp(&mut doc, Channel::BaseColor, [0.5, 0.5]);
        let pixels = doc
            .selected_layer()
            .pixels
            .get(&Channel::BaseColor)
            .unwrap();
        assert_eq!(pixels.get_pixel(0, 0).0, [0, 0, 0, 0]);
        assert!(pixels.get_pixel(32, 32).0[3] > 0);
    }
    #[test]
    fn eraser_removes_alpha() {
        let mut doc = Document::new(64);
        let mut brush = Brush {
            radius: 6,
            opacity: 1.0,
            hardness: 1.0,
            ..Brush::default()
        };
        brush.stamp(&mut doc, Channel::BaseColor, [0.5, 0.5]);
        brush.mode = BrushMode::Erase;
        brush.stamp(&mut doc, Channel::BaseColor, [0.5, 0.5]);
        let pixels = doc
            .selected_layer()
            .pixels
            .get(&Channel::BaseColor)
            .unwrap();
        assert_eq!(pixels.get_pixel(32, 32).0[3], 0);
    }

    #[test]
    fn base_color_blends_in_linear_light_with_straight_alpha() {
        let mut doc = Document::new(64);
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(32, 32, image::Rgba([0, 0, 255, 255]));
        let brush = Brush {
            radius: 1,
            opacity: 0.5,
            hardness: 1.0,
            color: [255, 0, 0, 255],
            ..Brush::default()
        };

        brush.stamp(&mut doc, Channel::BaseColor, [0.5, 0.5]);

        let pixel = doc.selected_layer().pixels[&Channel::BaseColor].get_pixel(32, 32);
        assert_eq!(pixel.0, [188, 0, 188, 255]);
    }

    #[test]
    fn emissive_blends_in_linear_light_with_straight_alpha() {
        let mut doc = Document::new(64);
        doc.selected_layer_mut()
            .pixels_mut(Channel::Emissive, 64)
            .put_pixel(32, 32, image::Rgba([0, 0, 255, 255]));
        let brush = Brush {
            radius: 1,
            opacity: 0.5,
            hardness: 1.0,
            color: [255, 0, 0, 255],
            ..Brush::default()
        };

        brush.stamp(&mut doc, Channel::Emissive, [0.5, 0.5]);

        let pixel = doc.selected_layer().pixels[&Channel::Emissive].get_pixel(32, 32);
        assert_eq!(pixel.0[0], 188);
        assert_eq!(pixel.0[1], 0);
        assert!((187..=188).contains(&pixel.0[2]));
        assert_eq!(pixel.0[3], 255);
    }

    #[test]
    fn emissive_paint_over_transparency_keeps_target_rgb_and_coverage() {
        let mut doc = Document::new(64);
        let brush = Brush {
            radius: 1,
            opacity: 0.5,
            hardness: 1.0,
            color: [31, 127, 233, 255],
            ..Brush::default()
        };

        brush.stamp(&mut doc, Channel::Emissive, [0.5, 0.5]);

        let pixel = doc.selected_layer().pixels[&Channel::Emissive].get_pixel(32, 32);
        assert_eq!(pixel.0, [31, 127, 233, 128]);
    }

    #[test]
    fn normal_brush_interpolates_orientations_and_renormalizes() {
        let mut doc = Document::new(64);
        doc.selected_layer_mut()
            .pixels_mut(Channel::Normal, 64)
            .put_pixel(32, 32, image::Rgba([128, 128, 255, 255]));
        let brush = Brush {
            radius: 1,
            opacity: 0.5,
            hardness: 1.0,
            color: [218, 128, 218, 255],
            ..Brush::default()
        };

        brush.stamp(&mut doc, Channel::Normal, [0.5, 0.5]);

        let pixel = doc.selected_layer().pixels[&Channel::Normal].get_pixel(32, 32);
        let decoded = decode_normal(&pixel.0[..3]);
        assert!((pixel.0[0] as i16 - 176).abs() <= 2);
        assert!((pixel.0[1] as i16 - 128).abs() <= 1);
        assert!((pixel.0[2] as i16 - 245).abs() <= 2);
        assert_eq!(pixel.0[3], 255);
        assert!((decoded.iter().map(|value| value * value).sum::<f32>() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn repeated_normal_stamps_remain_unit_length_and_transparent_stamps_keep_coverage() {
        let mut doc = Document::new(64);
        let brush = Brush {
            radius: 1,
            opacity: 0.2,
            hardness: 1.0,
            color: [196, 72, 230, 128],
            ..Brush::default()
        };
        for _ in 0..32 {
            brush.stamp(&mut doc, Channel::Normal, [0.5, 0.5]);
        }

        let pixel = doc.selected_layer().pixels[&Channel::Normal].get_pixel(32, 32);
        let raw = [
            pixel.0[0] as f32 / 127.5 - 1.0,
            pixel.0[1] as f32 / 127.5 - 1.0,
            pixel.0[2] as f32 / 127.5 - 1.0,
        ];
        let raw_length = raw.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((raw_length - 1.0).abs() < 0.015);
        assert!(pixel.0[3] > 240);

        let mut transparent = Document::new(64);
        let once = Brush {
            opacity: 0.5,
            ..brush
        };
        once.stamp(&mut transparent, Channel::Normal, [0.5, 0.5]);
        let pixel = transparent.selected_layer().pixels[&Channel::Normal].get_pixel(32, 32);
        assert_eq!(pixel.0[3], 64);
        let expected = decode_normal(&[196, 72, 230]);
        let mut expected_encoded = [0; 3];
        encode_normal(expected, &mut expected_encoded);
        assert_eq!(&pixel.0[..3], &expected_encoded);
    }

    #[test]
    fn erasing_normal_paint_removes_coverage_without_changing_orientation() {
        let mut doc = Document::new(64);
        let brush = Brush {
            radius: 1,
            opacity: 1.0,
            hardness: 1.0,
            color: [218, 128, 218, 255],
            ..Brush::default()
        };
        brush.stamp(&mut doc, Channel::Normal, [0.5, 0.5]);
        let before = doc.selected_layer().pixels[&Channel::Normal]
            .get_pixel(32, 32)
            .0;
        let eraser = Brush {
            mode: BrushMode::Erase,
            opacity: 0.5,
            ..brush
        };
        eraser.stamp(&mut doc, Channel::Normal, [0.5, 0.5]);
        let after = doc.selected_layer().pixels[&Channel::Normal]
            .get_pixel(32, 32)
            .0;
        assert_eq!(&after[..3], &before[..3]);
        assert_eq!(after[3], 128);
    }

    #[test]
    fn base_color_paint_over_transparency_keeps_target_rgb_and_coverage() {
        let mut doc = Document::new(64);
        let brush = Brush {
            radius: 1,
            opacity: 0.5,
            hardness: 1.0,
            color: [31, 127, 233, 255],
            ..Brush::default()
        };

        brush.stamp(&mut doc, Channel::BaseColor, [0.5, 0.5]);

        let pixel = doc.selected_layer().pixels[&Channel::BaseColor].get_pixel(32, 32);
        assert_eq!(pixel.0, [31, 127, 233, 128]);
    }

    #[test]
    fn zero_opacity_base_color_paint_does_not_change_pixel() {
        let mut doc = Document::new(64);
        let before = [45, 120, 210, 173];
        doc.selected_layer_mut()
            .pixels_mut(Channel::BaseColor, 64)
            .put_pixel(32, 32, image::Rgba(before));
        let brush = Brush {
            radius: 1,
            opacity: 0.0,
            hardness: 1.0,
            color: [255, 0, 0, 255],
            ..Brush::default()
        };

        brush.stamp(&mut doc, Channel::BaseColor, [0.5, 0.5]);

        let pixel = doc.selected_layer().pixels[&Channel::BaseColor].get_pixel(32, 32);
        assert_eq!(pixel.0, before);
    }
}

#[cfg(test)]
mod uv_masked_tests {
    use super::*;
    use crate::mesh::{Mesh, Vertex};

    #[test]
    fn disconnected_stroke_jump_stamps_destination_once() {
        let mut doc = Document::new(64);
        let brush = Brush {
            radius: 2,
            opacity: 0.5,
            hardness: 1.0,
            spacing: 0.5,
            mirror_u: true,
            ..Brush::default()
        };
        let from = [2.0 / 63.0, 2.0 / 63.0];
        let to = [48.0 / 63.0, 32.0 / 63.0];

        brush.stroke_masked(&mut doc, Channel::BaseColor, Some(from), to, None);

        let pixels = &doc.selected_layer().pixels[&Channel::BaseColor];
        // A second application of 50% opacity would raise alpha from 128 to 192.
        assert_eq!(pixels.get_pixel(48, 32).0[3], 128);
        assert_eq!(pixels.get_pixel(15, 32).0[3], 128);
        // The disconnected source island is not painted by the jump.
        assert_eq!(pixels.get_pixel(2, 2).0[3], 0);
    }

    #[test]
    fn connected_stroke_still_resamples_between_endpoints() {
        let mut doc = Document::new(64);
        let brush = Brush {
            radius: 2,
            opacity: 0.5,
            hardness: 1.0,
            spacing: 0.5,
            ..Brush::default()
        };
        let from = [10.0 / 63.0, 32.0 / 63.0];
        let to = [24.0 / 63.0, 32.0 / 63.0];

        brush.stroke_masked(&mut doc, Channel::BaseColor, Some(from), to, None);

        let pixels = &doc.selected_layer().pixels[&Channel::BaseColor];
        assert!(pixels.get_pixel(17, 32).0[3] > 0);
        assert!(pixels.get_pixel(24, 32).0[3] > 0);
    }

    #[test]
    fn masked_paint_does_not_touch_uv_atlas_background() {
        let mesh = Mesh {
            name: "tri".into(),
            vertices: vec![
                Vertex {
                    position: [0., 0., 0.],
                    normal: [0., 0., 1.],
                    uv: [0.1, 0.1],
                    extensions: Default::default(),
                },
                Vertex {
                    position: [1., 0., 0.],
                    normal: [0., 0., 1.],
                    uv: [0.6, 0.1],
                    extensions: Default::default(),
                },
                Vertex {
                    position: [0., 1., 0.],
                    normal: [0., 0., 1.],
                    uv: [0.1, 0.6],
                    extensions: Default::default(),
                },
            ],
            indices: vec![0, 1, 2],
            materials: vec![],
            triangle_materials: vec![],
            extensions: Default::default(),
        };
        let coverage = UvCoverage::generate(&mesh, 64).unwrap();
        let mut doc = Document::new(64);
        let brush = Brush {
            radius: 25,
            opacity: 1.,
            hardness: 1.,
            ..Brush::default()
        };
        brush.stamp_masked(&mut doc, Channel::BaseColor, [0.50, 0.50], Some(&coverage));
        let pixels = &doc.selected_layer().pixels[&Channel::BaseColor];
        assert_eq!(pixels.get_pixel(32, 32).0[3], 0);
        assert!(pixels.get_pixel(20, 20).0[3] > 0);
    }
}
