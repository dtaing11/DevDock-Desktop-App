//! Visual identity for DevDock.
//!
//! Direction: a "dock at night" instrument panel. Deep indigo base, a single
//! ember accent spent only on primary actions and the current selection,
//! teal reserved for remote/informational accents. Everything else stays
//! quiet: muted text, hairline borders, generous spacing.
//!
//! The design system lives here: color tokens, a 4px spacing grid, a type
//! scale, and shared component constants. Views should consume these
//! instead of inventing values.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, Visuals};

// ---------------------------------------------------------------------------
// Color tokens
// ---------------------------------------------------------------------------

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

/// Subtle hover wash (teal at low alpha) used by list rows.
pub const HOVER_WASH: Color32 = Color32::from_rgba_premultiplied(10, 36, 36, 40);
/// Selection wash (ember at low alpha) used by selected rows.
pub const SELECT_WASH: Color32 = Color32::from_rgba_premultiplied(48, 30, 14, 60);

// ---------------------------------------------------------------------------
// Spacing grid (4px base) and component constants
// ---------------------------------------------------------------------------

/// Base spacing unit; use multiples of this everywhere.
pub const UNIT: f32 = 4.0;
/// Standard control height for small panel buttons.
pub const CONTROL_SM: f32 = 24.0;
/// Standard control height for primary inputs/buttons.
pub const CONTROL_MD: f32 = 30.0;
/// Toolbar segment height.
pub const SEGMENT_H: f32 = 48.0;
/// Corner radius scale.
pub const RADIUS_SM: u8 = 6;
pub const RADIUS_MD: u8 = 8;
pub const RADIUS_LG: u8 = 12;

// ---------------------------------------------------------------------------
// Type scale
// ---------------------------------------------------------------------------

/// Section headers inside panels (small caps feel via spacing + color).
pub fn overline(text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase())
        .size(10.0)
        .color(FG_DIM)
        .letter_spacing_note()
}

/// Extension trait workaround: egui has no letter spacing; emulate the
/// overline style with size + weight only.
trait OverlineExt {
    fn letter_spacing_note(self) -> Self;
}
impl OverlineExt for egui::RichText {
    fn letter_spacing_note(self) -> Self {
        self.strong()
    }
}

/// Applies the DevDock theme to the egui context.
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    let mut visuals = Visuals::dark();

    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = BG;
    visuals.faint_bg_color = PANEL2;

    visuals.override_text_color = Some(FG);
    visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.window_corner_radius = CornerRadius::same(RADIUS_LG);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS_MD);
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };
    visuals.popup_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: Color32::from_black_alpha(100),
    };

    visuals.widgets.noninteractive.bg_fill = PANEL;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.inactive.bg_fill = PANEL2;
    visuals.widgets.inactive.corner_radius = CornerRadius::same(RADIUS_MD);
    visuals.widgets.hovered.bg_fill = PANEL2;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, TEAL);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(RADIUS_MD);
    // Pressed state: clearly different from hover so clicks visibly land.
    visuals.widgets.active.bg_fill = EMBER_DEEP.linear_multiply(0.45);
    visuals.widgets.active.weak_bg_fill = EMBER_DEEP.linear_multiply(0.45);
    visuals.widgets.active.bg_stroke = Stroke::new(2.0_f32, EMBER);
    visuals.widgets.active.corner_radius = CornerRadius::same(RADIUS_MD);
    visuals.widgets.active.expansion = -1.0; // slight press-down effect
    visuals.widgets.open.bg_fill = PANEL2;

    visuals.selection.bg_fill = EMBER_DEEP.linear_multiply(0.35);
    visuals.selection.stroke = Stroke::new(1.0_f32, EMBER);
    visuals.hyperlink_color = TEAL;

    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    // 4px grid: consistent rhythm everywhere.
    style.spacing.item_spacing = egui::vec2(2.0 * UNIT, 1.5 * UNIT);
    style.spacing.button_padding = egui::vec2(3.0 * UNIT, 1.5 * UNIT);
    style.spacing.menu_margin = egui::Margin::same((2.0 * UNIT) as i8);
    style.spacing.window_margin = egui::Margin::same((4.0 * UNIT) as i8);
    style.spacing.interact_size = egui::vec2(40.0, CONTROL_SM);

    // Type scale: clear hierarchy between body, small, and headings.
    use egui::TextStyle::*;
    style.text_styles.insert(Heading, FontId::proportional(17.0));
    style.text_styles.insert(Body, FontId::proportional(13.5));
    style.text_styles.insert(Button, FontId::proportional(13.0));
    style.text_styles.insert(Small, FontId::proportional(11.0));
    style.text_styles.insert(Monospace, FontId::monospace(12.5));

    ctx.set_style(style);
}

/// Bundles Inter (UI) and JetBrains Mono (code/diffs) into the binary so the
/// app looks the same on every machine, with egui's defaults as glyph
/// fallback (emoji, symbols).
fn install_fonts(ctx: &egui::Context) {
    const INTER: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
    const MONO: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");

    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("inter".into(), FontData::from_static(INTER).into());
    fonts.font_data.insert("jetbrains-mono".into(), FontData::from_static(MONO).into());

    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "inter".into());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "jetbrains-mono".into());

    ctx.set_fonts(fonts);
}
