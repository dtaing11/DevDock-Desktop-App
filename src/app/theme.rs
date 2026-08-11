//! Visual identity for Git Manage: deep indigo-slate base with ember/copper
//! primary accent and teal highlights. Intentionally not a GitHub Desktop look.

use egui::{Color32, CornerRadius, Stroke, Visuals};

pub const BG: Color32 = Color32::from_rgb(0x0f, 0x12, 0x18);
pub const PANEL: Color32 = Color32::from_rgb(0x17, 0x1c, 0x26);
pub const PANEL2: Color32 = Color32::from_rgb(0x1e, 0x25, 0x32);
pub const BORDER: Color32 = Color32::from_rgb(0x2c, 0x35, 0x47);
pub const FG: Color32 = Color32::from_rgb(0xe8, 0xe3, 0xd8);
pub const FG_DIM: Color32 = Color32::from_rgb(0x8a, 0x93, 0xa6);
pub const EMBER: Color32 = Color32::from_rgb(0xff, 0x9d, 0x4d);
pub const EMBER_DEEP: Color32 = Color32::from_rgb(0xe0, 0x7b, 0x2a);
pub const TEAL: Color32 = Color32::from_rgb(0x3d, 0xdb, 0xd9);
pub const DANGER: Color32 = Color32::from_rgb(0xff, 0x6b, 0x6b);
pub const ADD: Color32 = Color32::from_rgb(0x7e, 0xe7, 0x87);
pub const DEL: Color32 = Color32::from_rgb(0xff, 0x7b, 0x72);
pub const WARN: Color32 = Color32::from_rgb(0xf0, 0xb4, 0x29);

/// Applies the Git Manage theme to the egui context.
pub fn apply(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();

    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = BG;
    visuals.faint_bg_color = PANEL2;

    visuals.override_text_color = Some(FG);
    visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(10);

    visuals.widgets.noninteractive.bg_fill = PANEL;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.inactive.bg_fill = PANEL2;
    visuals.widgets.inactive.corner_radius = CornerRadius::same(8);
    visuals.widgets.hovered.bg_fill = PANEL2;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, TEAL);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
    visuals.widgets.active.bg_fill = BORDER;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, EMBER);
    visuals.widgets.active.corner_radius = CornerRadius::same(8);
    visuals.widgets.open.bg_fill = PANEL2;

    visuals.selection.bg_fill = EMBER_DEEP.linear_multiply(0.35);
    visuals.selection.stroke = Stroke::new(1.0_f32, EMBER);
    visuals.hyperlink_color = TEAL;

    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    ctx.set_style(style);
}
