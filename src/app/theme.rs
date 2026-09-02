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

/// One palette. Two exist: the dock-at-night original, and a light one for
/// people who work in daylight or need the contrast.
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub panel2: Color32,
    pub border: Color32,
    pub fg: Color32,
    pub fg_dim: Color32,
    pub ember: Color32,
    pub ember_deep: Color32,
    pub teal: Color32,
    pub danger: Color32,
    pub add: Color32,
    pub del: Color32,
    pub warn: Color32,
    pub hover_wash: Color32,
    pub select_wash: Color32,
}

/// The original: deep indigo base, a single ember accent.
pub const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x0f, 0x12, 0x18),
    panel: Color32::from_rgb(0x17, 0x1c, 0x26),
    panel2: Color32::from_rgb(0x1e, 0x25, 0x32),
    border: Color32::from_rgb(0x2c, 0x35, 0x47),
    fg: Color32::from_rgb(0xe8, 0xe3, 0xd8),
    fg_dim: Color32::from_rgb(0x8a, 0x93, 0xa6),
    ember: Color32::from_rgb(0xff, 0x9d, 0x4d),
    ember_deep: Color32::from_rgb(0xe0, 0x7b, 0x2a),
    teal: Color32::from_rgb(0x3d, 0xdb, 0xd9),
    danger: Color32::from_rgb(0xff, 0x6b, 0x6b),
    add: Color32::from_rgb(0x7e, 0xe7, 0x87),
    del: Color32::from_rgb(0xff, 0x7b, 0x72),
    warn: Color32::from_rgb(0xf0, 0xb4, 0x29),
    hover_wash: Color32::from_rgba_premultiplied(10, 36, 36, 40),
    select_wash: Color32::from_rgba_premultiplied(48, 30, 14, 60),
};

/// The same identity in daylight: warm paper, the accents darkened enough
/// to carry on white. A light theme that is the dark one inverted reads as
/// washed out, so every accent is picked again rather than flipped.
pub const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xfa, 0xf8, 0xf4),
    panel: Color32::from_rgb(0xf1, 0xed, 0xe6),
    panel2: Color32::from_rgb(0xe6, 0xe1, 0xd8),
    border: Color32::from_rgb(0xd0, 0xc8, 0xba),
    fg: Color32::from_rgb(0x24, 0x28, 0x30),
    fg_dim: Color32::from_rgb(0x69, 0x70, 0x7e),
    ember: Color32::from_rgb(0xc2, 0x5b, 0x0a),
    ember_deep: Color32::from_rgb(0x9c, 0x45, 0x05),
    teal: Color32::from_rgb(0x0d, 0x71, 0x74),
    danger: Color32::from_rgb(0xc0, 0x2a, 0x2a),
    add: Color32::from_rgb(0x1c, 0x7a, 0x33),
    del: Color32::from_rgb(0xb3, 0x2c, 0x24),
    warn: Color32::from_rgb(0x9a, 0x6b, 0x00),
    hover_wash: Color32::from_rgba_premultiplied(10, 30, 30, 18),
    select_wash: Color32::from_rgba_premultiplied(60, 36, 12, 30),
};

/// Which palette is in use. An atomic rather than a lock: it is read
/// hundreds of times per frame and written when someone flips a switch.
static LIGHT_MODE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Switches the palette. Call [`apply`] afterwards to restyle egui itself.
pub fn set_light(light: bool) {
    LIGHT_MODE.store(light, std::sync::atomic::Ordering::Relaxed);
}

pub fn is_light() -> bool {
    LIGHT_MODE.load(std::sync::atomic::Ordering::Relaxed)
}

/// The palette in force.
pub fn palette() -> Palette {
    if is_light() { LIGHT } else { DARK }
}

pub fn bg() -> Color32 {
    palette().bg
}
pub fn panel() -> Color32 {
    palette().panel
}
pub fn panel2() -> Color32 {
    palette().panel2
}
pub fn border() -> Color32 {
    palette().border
}
pub fn fg() -> Color32 {
    palette().fg
}
pub fn fg_dim() -> Color32 {
    palette().fg_dim
}
pub fn ember() -> Color32 {
    palette().ember
}
pub fn ember_deep() -> Color32 {
    palette().ember_deep
}
pub fn teal() -> Color32 {
    palette().teal
}
pub fn danger() -> Color32 {
    palette().danger
}
pub fn add() -> Color32 {
    palette().add
}
pub fn del() -> Color32 {
    palette().del
}
pub fn warn() -> Color32 {
    palette().warn
}
pub fn hover_wash() -> Color32 {
    palette().hover_wash
}
pub fn select_wash() -> Color32 {
    palette().select_wash
}

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

/// Section header inside a panel: small, dim, upper case, and — now that
/// there is a face for it — actually heavier than the text it labels.
pub fn overline(text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase()).font(semibold(10.0)).color(fg_dim())
}

/// A heading in the interface, at one of the type scale's sizes.
pub fn heading(text: &str, size: f32) -> egui::RichText {
    egui::RichText::new(text).font(semibold(size)).color(fg())
}

/// Emphasised body text: the same size as its surroundings, heavier.
pub fn strong(text: &str) -> egui::RichText {
    egui::RichText::new(text).font(semibold(TEXT)).color(fg())
}

// The type scale. Sizes are named so a view asks for a role rather than a
// number, which is what keeps two panels showing the same kind of thing at
// the same size.

/// Dialog and view titles.
pub const TITLE: f32 = 16.0;
/// Section titles within a view.
pub const SUBTITLE: f32 = 13.5;
/// Body text.
pub const TEXT: f32 = 13.0;
/// Secondary text: hints, counts, timestamps.
pub const SMALL: f32 = 11.5;

/// Runs `f` against a throwaway context that has the app's fonts installed.
///
/// `egui::__run_test_ctx` gives a context with no bound font families, so any
/// view that asks for the semibold or italic face panics on it — which is a
/// property of the test harness, not of the view. Every UI test goes through
/// here so it starts the way the app does.
#[doc(hidden)]
pub fn run_test_ctx(mut f: impl FnMut(&egui::Context)) {
    let ctx = egui::Context::default();
    // Before the first frame, not during one: fonts installed mid-frame only
    // take effect on the next, which is too late for the code being tested.
    apply(&ctx);
    // Two passes, because the first lays out before the font atlas is warm.
    for _ in 0..2 {
        let _ = ctx.run(Default::default(), &mut f);
    }
}

/// Applies the DevDock theme to the egui context.
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    // Start from egui's own light or dark base so the parts this file does
    // not set — scrollbars, text selection, disabled widgets — are right
    // for the palette rather than dark-on-light.
    let mut visuals = if is_light() { Visuals::light() } else { Visuals::dark() };

    visuals.panel_fill = bg();
    visuals.window_fill = panel();
    visuals.extreme_bg_color = bg();
    visuals.faint_bg_color = panel2();

    visuals.override_text_color = Some(fg());
    visuals.window_stroke = Stroke::new(1.0_f32, border());
    visuals.window_corner_radius = CornerRadius::same(RADIUS_LG);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS_MD);
    // A shadow tuned for a dark ground is a smear on a light one.
    let shadow_alpha = if is_light() { 40 } else { 120 };
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(shadow_alpha),
    };
    visuals.popup_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: Color32::from_black_alpha(shadow_alpha.saturating_sub(20)),
    };

    visuals.widgets.noninteractive.bg_fill = panel();
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, border());
    visuals.widgets.inactive.bg_fill = panel2();
    visuals.widgets.inactive.corner_radius = CornerRadius::same(RADIUS_MD);
    visuals.widgets.hovered.bg_fill = panel2();
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, teal());
    visuals.widgets.hovered.corner_radius = CornerRadius::same(RADIUS_MD);
    // Pressed state: clearly different from hover so clicks visibly land.
    visuals.widgets.active.bg_fill = ember_deep().linear_multiply(0.45);
    visuals.widgets.active.weak_bg_fill = ember_deep().linear_multiply(0.45);
    visuals.widgets.active.bg_stroke = Stroke::new(2.0_f32, ember());
    visuals.widgets.active.corner_radius = CornerRadius::same(RADIUS_MD);
    visuals.widgets.active.expansion = -1.0; // slight press-down effect
    visuals.widgets.open.bg_fill = panel2();

    visuals.selection.bg_fill = ember_deep().linear_multiply(0.35);
    visuals.selection.stroke = Stroke::new(1.0_f32, ember());
    visuals.hyperlink_color = teal();

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
///
/// Three weights of Inter, not one. egui has no synthetic bold: text is drawn
/// from the glyphs of whichever face it is given, so with only a regular face
/// loaded, "bold" can be nothing but a brighter colour — which is why every
/// heading and every `**bold**` in a rendered document used to read as plain
/// text with the contrast turned up. Italic is the same story. Both are named
/// families here, and [`semibold`] and [`italic`] are how the rest of the app
/// asks for them.
fn install_fonts(ctx: &egui::Context) {
    const INTER: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
    const INTER_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");
    const INTER_ITALIC: &[u8] = include_bytes!("../../assets/fonts/Inter-Italic.ttf");
    const MONO: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");

    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("inter".into(), FontData::from_static(INTER).into());
    fonts
        .font_data
        .insert("inter-semibold".into(), FontData::from_static(INTER_SEMIBOLD).into());
    fonts.font_data.insert("inter-italic".into(), FontData::from_static(INTER_ITALIC).into());
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
    // The named families fall back to the regular face for anything the
    // weight does not cover, and to egui's own for symbols and emoji.
    for (name, file) in
        [(SEMIBOLD, "inter-semibold"), (ITALIC, "inter-italic")]
    {
        fonts.families.insert(
            FontFamily::Name(name.into()),
            vec![file.into(), "inter".into(), "NotoEmoji-Regular".into()],
        );
    }

    ctx.set_fonts(fonts);
}

/// Name of the semibold font family.
const SEMIBOLD: &str = "semibold";
/// Name of the italic font family.
const ITALIC: &str = "italic";

/// A semibold font of `size`, for headings and emphasis.
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

/// An italic font of `size`.
pub fn italic(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ITALIC.into()))
}
