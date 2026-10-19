//! Compact Painter-style visual tokens for Kalium's native desktop UI.
//!
//! Colors and dimensions are approximations calibrated from public screenshots;
//! this module contains no Adobe assets or branding.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Pos2, Shape, Stroke, TextStyle,
    Visuals,
};
use std::ops::RangeInclusive;

pub struct PainterClassic;

/// Compact two-line brush control: label and editable value above a thin slider.
/// The caller supplies the same inclusive range for both controls so pointer
/// adjustment and typed input share clamping behavior.
pub fn toolbar_parameter<Num: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    value: &mut Num,
    range: RangeInclusive<Num>,
    label: &str,
    speed: f64,
) -> egui::Response {
    ui.push_id(("toolbar-parameter", label), |ui| {
        let block = ui.allocate_ui_with_layout(
            egui::vec2(116.0, 32.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(2.0, 0.0);
                ui.spacing_mut().interact_size.y = 14.0;
                ui.spacing_mut().slider_width = 112.0;
                ui.spacing_mut().slider_rail_height = 2.0;
                let heading = ui.horizontal(|ui| {
                    let label_response = ui.label(egui::RichText::new(label).small().weak());
                    ui.add_space((ui.available_width() - 36.0).max(0.0));
                    let drag = ui.add_sized(
                        [36.0, 14.0],
                        egui::DragValue::new(value)
                            .range(range.clone())
                            .speed(speed),
                    );
                    let label_id = label_response.id;
                    let drag = drag.labelled_by(label_id);
                    drag.widget_info(|| {
                        let mut info =
                            egui::WidgetInfo::drag_value(ui.is_enabled(), value.to_f64());
                        info.label = Some(label.to_owned());
                        info
                    });
                    (drag.union(label_response), label_id)
                });

                let slider = ui.add_sized(
                    [112.0, 14.0],
                    egui::Slider::new(value, range)
                        .show_value(false)
                        .text("")
                        .handle_shape(egui::style::HandleShape::Circle),
                );
                let slider = slider.labelled_by(heading.inner.1);
                slider.widget_info(|| {
                    egui::WidgetInfo::slider(ui.is_enabled(), value.to_f64(), label)
                });
                heading.inner.0.union(slider)
            },
        );
        block.inner
    })
    .inner
}

impl PainterClassic {
    pub const WINDOW: Color32 = Color32::from_rgb(48, 48, 51);
    pub const PANEL: Color32 = Color32::from_rgb(45, 46, 49);
    pub const RAISED: Color32 = Color32::from_rgb(52, 53, 57);
    pub const TAB: Color32 = Color32::from_rgb(41, 42, 45);
    pub const INPUT: Color32 = Color32::from_rgb(35, 36, 39);
    pub const CANVAS: Color32 = Color32::from_rgb(31, 31, 33);
    pub const HAIRLINE: Color32 = Color32::from_rgb(25, 26, 29);
    pub const BORDER: Color32 = Color32::from_rgb(66, 67, 71);
    pub const TEXT: Color32 = Color32::from_rgb(201, 201, 203);
    pub const MUTED: Color32 = Color32::from_rgb(147, 148, 152);
    pub const HOVER: Color32 = Color32::from_rgb(58, 59, 62);
    pub const SELECTED: Color32 = Color32::from_rgb(49, 95, 128);
    /// Low-contrast surface under a selected tree row; the blue edge carries
    /// the selection cue used by the reference layer stack.
    pub const SELECTED_SURFACE: Color32 = Color32::from_rgb(47, 54, 61);
    pub const SELECTED_EDGE: Color32 = Color32::from_rgb(66, 132, 173);
    pub const FOCUS: Color32 = Color32::from_rgb(105, 168, 209);

    /// Small native layer glyphs keep toolbars legible without shipping an icon set.
    pub fn layer_icon(painter: &egui::Painter, rect: egui::Rect, kind: &str, color: Color32) {
        let stroke = Stroke::new(1.0_f32, color);
        let body = rect.shrink(4.0);
        match kind {
            "paint" => {
                painter.line_segment(
                    [body.left_bottom() + egui::vec2(1.0, -1.0), body.right_top()],
                    stroke,
                );
                painter.line_segment(
                    [
                        body.left_bottom(),
                        body.left_bottom() + egui::vec2(3.0, 0.0),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [body.right_top(), body.right_top() + egui::vec2(-1.0, 3.0)],
                    stroke,
                );
            }
            "erase" => {
                painter.line_segment(
                    [
                        body.left_center() + egui::vec2(1.0, 2.0),
                        body.right_center(),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [body.left_center() + egui::vec2(1.0, 2.0), body.center_top()],
                    stroke,
                );
                painter.line_segment([body.center_top(), body.right_center()], stroke);
                painter.line_segment(
                    [
                        body.left_center() + egui::vec2(1.0, 2.0),
                        body.left_bottom(),
                    ],
                    stroke,
                );
                painter.line_segment([body.left_bottom(), body.right_bottom()], stroke);
            }
            "sample" => {
                let center = body.center() + egui::vec2(-1.0, 1.0);
                painter.circle_stroke(center, body.width().min(body.height()) * 0.25, stroke);
                painter.line_segment([center + egui::vec2(2.0, -2.0), body.right_top()], stroke);
                painter.circle_filled(body.right_top(), 1.2, color);
            }
            "group" => {
                let folder = egui::Rect::from_min_max(body.min + egui::vec2(0.0, 2.0), body.max);
                painter.rect_stroke(folder, CornerRadius::ZERO, stroke, egui::StrokeKind::Inside);
                painter.line_segment(
                    [folder.left_top(), folder.left_top() + egui::vec2(3.0, 0.0)],
                    stroke,
                );
                painter.line_segment(
                    [folder.left_top() + egui::vec2(0.0, -1.0), folder.left_top()],
                    stroke,
                );
            }
            "fill" => {
                painter.rect_filled(body.shrink(1.0), CornerRadius::ZERO, color);
                painter.rect_stroke(body, CornerRadius::ZERO, stroke, egui::StrokeKind::Inside);
            }
            "duplicate" => {
                painter.rect_stroke(
                    body.translate(egui::vec2(-1.5, -1.5)),
                    CornerRadius::ZERO,
                    stroke,
                    egui::StrokeKind::Inside,
                );
                painter.rect_stroke(body, CornerRadius::ZERO, stroke, egui::StrokeKind::Inside);
            }
            "delete" => {
                painter.line_segment([body.left_top(), body.right_top()], stroke);
                painter.line_segment(
                    [body.left_top() + egui::vec2(1.0, 1.0), body.left_bottom()],
                    stroke,
                );
                painter.line_segment(
                    [
                        body.right_top() + egui::vec2(-1.0, 1.0),
                        body.right_bottom(),
                    ],
                    stroke,
                );
                painter.line_segment([body.left_bottom(), body.right_bottom()], stroke);
                painter.line_segment(
                    [
                        body.center_top() + egui::vec2(-2.0, -1.0),
                        body.center_top() + egui::vec2(2.0, -1.0),
                    ],
                    stroke,
                );
            }
            "up" | "down" => {
                let direction = if kind == "up" { -1.0 } else { 1.0 };
                let center = body.center();
                let tip = egui::pos2(center.x, center.y + direction * 3.0);
                let tail = egui::pos2(center.x, center.y - direction * 3.0);
                painter.line_segment([tail, tip], stroke);
                painter.line_segment([tip, tip + egui::vec2(-3.0, -direction * 3.0)], stroke);
                painter.line_segment([tip, tip + egui::vec2(3.0, -direction * 3.0)], stroke);
            }
            "mask" => {
                painter.circle_stroke(
                    body.center(),
                    body.width().min(body.height()) * 0.48,
                    stroke,
                );
                painter.line_segment([body.center_top(), body.center_bottom()], stroke);
                painter.line_segment([body.center(), body.right_center()], stroke);
            }
            "mask_white" | "mask_black" => {
                painter.circle_stroke(
                    body.center(),
                    body.width().min(body.height()) * 0.48,
                    stroke,
                );
                let half = egui::Rect::from_min_max(
                    body.left_top(),
                    egui::pos2(body.center().x, body.bottom()),
                );
                if kind == "mask_white" {
                    painter.rect_filled(half.shrink(1.5), CornerRadius::ZERO, color);
                }
                painter.line_segment([body.center_top(), body.center_bottom()], stroke);
            }
            "history" => {
                let radius = body.width().min(body.height()) * 0.43;
                painter.circle_stroke(body.center(), radius, stroke);
                painter.line_segment(
                    [
                        body.center(),
                        body.center() + egui::vec2(0.0, -radius * 0.65),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        body.center(),
                        body.center() + egui::vec2(radius * 0.55, radius * 0.25),
                    ],
                    stroke,
                );
            }
            "eye" | "eye_off" => {
                let eye_body = rect.shrink(2.0);
                let eye = egui::Rect::from_center_size(
                    eye_body.center(),
                    egui::vec2(eye_body.width(), eye_body.height() * 0.6),
                );
                for y in [eye.top(), eye.bottom()] {
                    painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                        [
                            eye.left_center(),
                            egui::pos2(eye.left() + eye.width() * 0.25, y),
                            egui::pos2(eye.right() - eye.width() * 0.25, y),
                            eye.right_center(),
                        ],
                        false,
                        Color32::TRANSPARENT,
                        stroke,
                    ));
                }
                painter.circle_stroke(eye.center(), eye.height() * 0.23, stroke);
                painter.circle_filled(eye.center(), 1.0, color);
                if kind == "eye_off" {
                    painter.line_segment(
                        [body.left_bottom(), body.right_top()],
                        Stroke::new(1.0_f32, Self::MUTED),
                    );
                }
            }
            _ => {}
        }
    }

    /// Independent, monochrome category glyphs for the Assets filter row.
    /// Unsupported categories are displayed disabled by the caller.
    pub fn asset_icon(painter: &egui::Painter, rect: egui::Rect, kind: &str, color: Color32) {
        let stroke = Stroke::new(1.0_f32, color);
        let body = rect.shrink(5.0);
        match kind {
            "material" => {
                painter.circle_stroke(
                    body.center(),
                    body.width().min(body.height()) * 0.48,
                    stroke,
                );
                painter.circle_filled(body.center() + egui::vec2(-2.0, -2.0), 1.3, color);
                painter.line_segment(
                    [
                        body.center() + egui::vec2(1.5, -3.5),
                        body.center() + egui::vec2(3.5, -1.5),
                    ],
                    stroke,
                );
            }
            "alpha" => {
                painter.circle_stroke(
                    body.center(),
                    body.width().min(body.height()) * 0.45,
                    stroke,
                );
                painter.circle_filled(body.center(), body.width().min(body.height()) * 0.18, color);
                painter.circle_filled(body.center() + egui::vec2(3.0, -3.0), 1.0, Self::TAB);
            }
            "brush" => {
                painter.line_segment(
                    [body.left_bottom() + egui::vec2(1.0, -1.0), body.right_top()],
                    stroke,
                );
                painter.line_segment(
                    [
                        body.left_bottom(),
                        body.left_bottom() + egui::vec2(3.0, 0.0),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        body.left_bottom(),
                        body.left_bottom() + egui::vec2(0.0, -3.0),
                    ],
                    stroke,
                );
            }
            "texture" => {
                let cell = body.width().min(body.height()) * 0.42;
                for (x, y) in [(0.0, 0.0), (1.0, 1.0)] {
                    painter.rect_filled(
                        egui::Rect::from_min_size(
                            body.center() + egui::vec2(x * cell - cell, y * cell - cell),
                            egui::vec2(cell, cell),
                        ),
                        CornerRadius::ZERO,
                        color,
                    );
                }
                painter.rect_stroke(body, CornerRadius::ZERO, stroke, egui::StrokeKind::Inside);
            }
            "filter" => {
                for (index, offset) in [-3.0_f32, 0.0, 3.0].into_iter().enumerate() {
                    let y = body.center().y + offset;
                    painter.line_segment(
                        [egui::pos2(body.left(), y), egui::pos2(body.right(), y)],
                        stroke,
                    );
                    let x = body.left() + body.width() * [0.68, 0.36, 0.56][index];
                    painter.circle_filled(egui::pos2(x, y), 1.3, color);
                }
            }
            "generator" => {
                let left = body.left_center();
                let center = body.center();
                let right = body.right_center();
                painter.line_segment([left, center], stroke);
                painter.line_segment([center, right], stroke);
                for point in [left, center, right] {
                    painter.circle_filled(point, 1.8, color);
                }
            }
            _ => {}
        }
    }

    /// Draw a small material preview using a shaded mesh, not an external bitmap.
    pub fn material_sphere(painter: &egui::Painter, center: Pos2, radius: f32, base: Color32) {
        const SEGMENTS: usize = 48;
        const RINGS: usize = 12;
        let mut mesh = egui::epaint::Mesh::default();
        let lit_color = |nx: f32, ny: f32| {
            let nz = (1.0 - nx * nx - ny * ny).max(0.0).sqrt();
            let diffuse = (nx * -0.42 + ny * -0.52 + nz * 0.74).max(0.0);
            let light = (0.20 + diffuse * 0.72).clamp(0.0, 1.0);
            Color32::from_rgb(
                (f32::from(base.r()) * light).round() as u8,
                (f32::from(base.g()) * light).round() as u8,
                (f32::from(base.b()) * light).round() as u8,
            )
        };
        mesh.colored_vertex(center, lit_color(0.0, 0.0));
        for ring in 1..=RINGS {
            let normalized_radius = ring as f32 / RINGS as f32;
            for segment in 0..SEGMENTS {
                let angle = std::f32::consts::TAU * segment as f32 / SEGMENTS as f32;
                let nx = normalized_radius * angle.cos();
                let ny = normalized_radius * angle.sin();
                mesh.colored_vertex(
                    center + egui::vec2(nx * radius, ny * radius),
                    lit_color(nx, ny),
                );
            }
        }
        for segment in 0..SEGMENTS {
            let next = (segment + 1) % SEGMENTS;
            mesh.add_triangle(0, 1 + segment as u32, 1 + next as u32);
        }
        for ring in 1..RINGS {
            let inner = 1 + ((ring - 1) * SEGMENTS) as u32;
            let outer = 1 + (ring * SEGMENTS) as u32;
            for segment in 0..SEGMENTS {
                let next = (segment + 1) % SEGMENTS;
                let inner_a = inner + segment as u32;
                let inner_b = inner + next as u32;
                let outer_a = outer + segment as u32;
                let outer_b = outer + next as u32;
                mesh.add_triangle(inner_a, outer_a, outer_b);
                mesh.add_triangle(inner_a, outer_b, inner_b);
            }
        }
        painter.add(Shape::mesh(mesh));
        painter.circle_filled(
            center + egui::vec2(-radius * 0.34, -radius * 0.40),
            radius * 0.11,
            Color32::from_white_alpha(118),
        );
        painter.circle_stroke(center, radius, Stroke::new(0.7_f32, Self::HAIRLINE));
    }

    pub fn gpu_canvas_clear() -> [f64; 4] {
        fn srgb_to_linear(channel: u8) -> f64 {
            let value = f64::from(channel) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        let [r, g, b, _] = Self::CANVAS.to_array();
        [srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b), 1.0]
    }

    pub fn dock_style(egui_style: &egui::Style) -> egui_dock::Style {
        let mut style = egui_dock::Style::from_egui(egui_style);
        style.dock_area_padding = Some(Margin::ZERO);
        style.main_surface_border_stroke = Stroke::new(1.0_f32, Self::HAIRLINE);
        style.main_surface_border_rounding = CornerRadius::ZERO;
        style.tab_bar.height = 22.0;
        style.tab_bar.bg_fill = Self::TAB;
        style.tab_bar.hline_color = Self::BORDER;
        style.tab_bar.corner_radius = CornerRadius::ZERO;
        style.tab_bar.fill_tab_bar = false;
        style.tab.active.bg_fill = Self::PANEL;
        style.tab.active.text_color = Self::TEXT;
        style.tab.active.outline_color = Self::HAIRLINE;
        style.tab.inactive.bg_fill = Self::TAB;
        style.tab.inactive.text_color = Self::MUTED;
        style.tab.inactive.outline_color = Self::HAIRLINE;
        style.tab.focused.bg_fill = Self::PANEL;
        style.tab.focused.text_color = Self::TEXT;
        style.tab.focused.outline_color = Self::HAIRLINE;
        style.tab.hovered.bg_fill = Self::HOVER;
        style.tab.hovered.text_color = Self::TEXT;
        style.tab.hovered.outline_color = Self::BORDER;
        style.tab.hline_below_active_tab_name = false;
        style.tab.minimum_width = Some(42.0);
        style.tab.tab_body.bg_fill = Self::PANEL;
        style.tab.tab_body.stroke = Stroke::new(1.0_f32, Self::HAIRLINE);
        style.tab.tab_body.inner_margin = Margin::same(3);
        style.tab.tab_body.corner_radius = CornerRadius::ZERO;
        for tab in [
            &mut style.tab.active,
            &mut style.tab.inactive,
            &mut style.tab.focused,
            &mut style.tab.hovered,
            &mut style.tab.inactive_with_kb_focus,
            &mut style.tab.active_with_kb_focus,
            &mut style.tab.focused_with_kb_focus,
        ] {
            tab.corner_radius = CornerRadius::ZERO;
        }
        style.separator.color_idle = Self::BORDER;
        style.separator.color_hovered = Self::FOCUS;
        style.separator.color_dragged = Self::FOCUS;
        style
    }

    pub fn apply(ctx: &egui::Context) {
        let mut visuals = Visuals::dark();
        visuals.override_text_color = Some(Self::TEXT);
        visuals.panel_fill = Self::PANEL;
        visuals.window_fill = Self::WINDOW;
        visuals.extreme_bg_color = Self::INPUT;
        visuals.faint_bg_color = Self::RAISED;
        visuals.code_bg_color = Self::INPUT;
        visuals.hyperlink_color = Self::FOCUS;
        visuals.selection.bg_fill = Self::SELECTED;
        visuals.selection.stroke = Stroke::new(1.0_f32, Self::FOCUS);
        visuals.window_corner_radius = CornerRadius::ZERO;
        visuals.menu_corner_radius = CornerRadius::ZERO;
        visuals.window_shadow = egui::epaint::Shadow::NONE;
        visuals.popup_shadow = egui::epaint::Shadow::NONE;
        visuals.button_frame = true;

        let border = Stroke::new(1.0_f32, Self::BORDER);
        visuals.widgets.noninteractive.bg_fill = Self::PANEL;
        visuals.widgets.noninteractive.bg_stroke = border;
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, Self::TEXT);
        visuals.widgets.inactive.bg_fill = Self::RAISED;
        visuals.widgets.inactive.weak_bg_fill = Self::INPUT;
        visuals.widgets.inactive.bg_stroke = border;
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, Self::TEXT);
        visuals.widgets.hovered.bg_fill = Self::HOVER;
        visuals.widgets.hovered.weak_bg_fill = Self::HOVER;
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Self::FOCUS);
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Self::TEXT);
        visuals.widgets.active.bg_fill = Self::SELECTED;
        visuals.widgets.active.weak_bg_fill = Self::SELECTED;
        visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, Self::FOCUS);
        visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, Self::TEXT);
        visuals.widgets.open.bg_fill = Self::RAISED;
        visuals.widgets.open.weak_bg_fill = Self::RAISED;
        visuals.widgets.open.bg_stroke = border;
        visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, Self::TEXT);
        for widget in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            widget.corner_radius = CornerRadius::ZERO;
            widget.expansion = 0.0;
        }

        ctx.set_visuals(visuals);
        let mut style = (*ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(4.0, 2.0);
        style.spacing.window_margin = Margin::same(4);
        style.spacing.menu_margin = Margin::same(4);
        style.spacing.button_padding = egui::vec2(4.0, 1.0);
        style.spacing.interact_size = egui::vec2(18.0, 19.0);
        style.spacing.indent = 12.0;
        style.spacing.slider_width = 92.0;
        style.spacing.slider_rail_height = 3.0;
        style.spacing.combo_width = 88.0;
        style.spacing.text_edit_width = 150.0;
        style.spacing.icon_width = 12.0;
        style.spacing.icon_width_inner = 7.0;
        style.spacing.icon_spacing = 3.0;
        style.spacing.menu_width = 260.0;
        style.spacing.combo_height = 280.0;
        style.spacing.scroll = egui::style::ScrollStyle::thin();
        for (text_style, size) in [
            (TextStyle::Small, 10.0),
            (TextStyle::Body, 11.5),
            (TextStyle::Button, 11.0),
            (TextStyle::Monospace, 11.0),
            (TextStyle::Heading, 12.5),
        ] {
            style
                .text_styles
                .insert(text_style, FontId::new(size, FontFamily::Proportional));
        }
        style.drag_value_text_style = TextStyle::Small;
        style.visuals.selection.bg_fill = Self::SELECTED;
        ctx.set_style(style);
    }
}

#[cfg(test)]
mod toolbar_parameter_tests {
    use super::toolbar_parameter;
    use eframe::egui::{self, Event, Id, Key, Pos2, RawInput, Rect, Vec2};

    fn frame(ctx: &egui::Context, value: &mut f32, events: Vec<Event>) -> (Rect, Id, bool) {
        let _ = ctx.run(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(360.0, 100.0))),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let response = toolbar_parameter(ui, value, 0.0..=1.0, "Flow", 0.01);
                    ctx.data_mut(|data| {
                        data.insert_temp(
                            Id::new("toolbar-parameter-test-response"),
                            (response.rect, response.id, response.changed()),
                        );
                    });
                });
            },
        );
        ctx.data(|data| {
            data.get_temp::<(Rect, Id, bool)>(Id::new("toolbar-parameter-test-response"))
                .expect("parameter response should be captured")
        })
    }

    #[test]
    fn slider_pointer_edit_changes_value_and_union_response() {
        let ctx = egui::Context::default();
        let mut value = 0.25;
        let (rect, _, _) = frame(&ctx, &mut value, Vec::new());
        let position = Pos2::new(rect.left() + 56.0, rect.top() + 25.0);
        let (_, _, changed) = frame(
            &ctx,
            &mut value,
            vec![
                Event::PointerMoved(position),
                Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(value > 0.25);
        assert!(changed, "the union response should propagate slider edits");
    }

    #[test]
    fn typed_numeric_edit_is_clamped_to_the_shared_range() {
        let ctx = egui::Context::default();
        let mut value = 0.25;
        let (rect, _, _) = frame(&ctx, &mut value, Vec::new());
        let position = Pos2::new(rect.right() - 18.0, rect.top() + 7.0);
        let _ = frame(
            &ctx,
            &mut value,
            vec![
                Event::PointerMoved(position),
                Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let _ = frame(
            &ctx,
            &mut value,
            vec![Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        let _ = ctx.run(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(360.0, 100.0))),
                events: vec![
                    Event::Text("1.75".into()),
                    Event::Key {
                        key: Key::Enter,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    toolbar_parameter(ui, &mut value, 0.0..=1.0, "Flow", 0.01);
                });
            },
        );
        assert_eq!(value, 1.0);
    }

    #[test]
    fn toolbar_parameter_ids_are_scoped_by_label() {
        let ctx = egui::Context::default();
        let mut size = 22.0_f32;
        let mut flow = 0.85_f32;
        let mut ids = None;
        let _ = ctx.run(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(420.0, 100.0))),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        let size_response =
                            toolbar_parameter(ui, &mut size, 1.0..=180.0, "Size", 1.0);
                        let flow_response =
                            toolbar_parameter(ui, &mut flow, 0.01..=1.0, "Flow", 0.01);
                        ids = Some((size_response.id, flow_response.id));
                    });
                });
            },
        );
        let (size_id, flow_id) = ids.unwrap();
        assert_ne!(size_id, flow_id);
    }
}
