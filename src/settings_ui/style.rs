//! Colors, fonts, glyphs, and the egui style QuickDictate applies
//! to its Settings window.

use super::*;

/// Set once `apply_fonts` manages to load a Windows icon font (Segoe Fluent
/// Icons / MDL2 Assets). Section headers only draw their leading glyph when
/// this is true, so a machine missing the font degrades to plain titles rather
/// than tofu boxes.
pub(crate) static ICONS_OK: AtomicBool = AtomicBool::new(false);
pub(crate) fn icons_available() -> bool {
    ICONS_OK.load(Ordering::Relaxed)
}
pub(crate) fn c((r, g, b): (u8, u8, u8)) -> Color32 {
    Color32::from_rgb(r, g, b)
}
pub(crate) fn accent() -> Color32 {
    c(theme::ACCENT_RGB)
}
pub(crate) fn accent_hot() -> Color32 {
    c(theme::ACCENT_HOT_RGB)
}
pub(crate) fn accent_press() -> Color32 {
    c(theme::ACCENT_PRESS_RGB)
}
pub(crate) fn bg() -> Color32 {
    c(theme::bg_rgb())
}
pub(crate) fn surface() -> Color32 {
    c(theme::surface_rgb())
}
pub(crate) fn input_bg() -> Color32 {
    c(theme::input_rgb())
}
pub(crate) fn border() -> Color32 {
    c(theme::border_rgb())
}
pub(crate) fn text() -> Color32 {
    c(theme::text_rgb())
}
pub(crate) fn muted() -> Color32 {
    c(theme::muted_rgb())
}
pub(crate) fn good() -> Color32 {
    Color32::from_rgb(63, 185, 80)
}
pub(crate) fn bad() -> Color32 {
    Color32::from_rgb(220, 90, 90)
}
pub(crate) const ROUND: u8 = 6;
/// The QuickDictate logo as a window icon (same art as the tray/exe icon).
pub(crate) fn icon_data() -> egui::IconData {
    let (rgba, width, height) = crate::icon::rgba(256);
    egui::IconData {
        rgba,
        width,
        height,
    }
}
/// Use the system's Segoe UI (+ semibold for headings) so the window reads as
/// native Windows instead of egui's bundled font. Silently keeps the default
/// if the font files are missing.
pub(crate) fn apply_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    if let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\segoeui.ttf") {
        fonts
            .font_data
            .insert("segoe".into(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "segoe".into());
    }
    if let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\seguisb.ttf") {
        fonts.font_data.insert(
            "segoe-semibold".into(),
            egui::FontData::from_owned(bytes).into(),
        );
        fonts.families.insert(
            egui::FontFamily::Name("semibold".into()),
            vec!["segoe-semibold".into()],
        );
    }
    // Native Windows icon font for the section-header glyphs. Prefer Segoe
    // Fluent Icons (Win 11); fall back to Segoe MDL2 Assets (Win 10). Both share
    // the same PUA codepoints for the classic glyphs we use (E7xx/E8xx), so the
    // icons render identically whichever one is present. Isolated in its own
    // "icons" family so its private-use glyphs never leak into body text.
    let icon_font = std::fs::read(r"C:\Windows\Fonts\SegoeIcons.ttf")
        .or_else(|_| std::fs::read(r"C:\Windows\Fonts\segmdl2.ttf"));
    if let Ok(bytes) = icon_font {
        fonts
            .font_data
            .insert("icons".into(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .insert(egui::FontFamily::Name("icons".into()), vec!["icons".into()]);
        ICONS_OK.store(true, Ordering::Relaxed);
    }
    ctx.set_fonts(fonts);
}
pub(crate) fn semibold(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("semibold".into()))
}
/// A glyph from the Windows icon font (see `apply_fonts`). Used for the small
/// accent-blue symbol that leads each section header.
pub(crate) fn icon_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("icons".into()))
}
/// Label for the overflow (⋯) menu button: the Windows "More" icon (three dots)
/// when the icon font is present, else an ASCII fallback. Plain Segoe UI lacks
/// the U+22EF ellipsis glyph, so a raw "\u{22EF}" would render as tofu.
pub(crate) fn overflow_glyph() -> RichText {
    if icons_available() {
        RichText::new("\u{E712}").font(icon_font(16.0)) // MDL2 "More"
    } else {
        RichText::new("...").strong()
    }
}
/// Down-chevron label for the Save split-button dropdown, white-on-accent.
/// Uses the icon font's ChevronDown when available, else an ASCII "v".
pub(crate) fn chevron_down_glyph() -> RichText {
    if icons_available() {
        RichText::new("\u{E70D}") // MDL2 "ChevronDown"
            .font(icon_font(12.0))
            .color(Color32::WHITE)
    } else {
        RichText::new("v").color(Color32::WHITE)
    }
}
/// Label for deleting an installed local model. The text fallback keeps the
/// action unambiguous if the Windows icon font is unavailable.
pub(crate) fn delete_glyph() -> RichText {
    if icons_available() {
        RichText::new("\u{E74D}").font(icon_font(15.0)) // MDL2 "Delete"
    } else {
        RichText::new("Remove")
    }
}
/// SageThumbs-flavoured egui visuals: theme surfaces, hairline borders, the
/// brand blue for selection/links, generous rounding.
pub(crate) fn apply_style(ctx: &egui::Context) {
    let dark = theme::is_dark();
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.override_text_color = Some(text());
    v.panel_fill = bg();
    v.window_fill = surface();
    v.window_stroke = Stroke::new(1.0, border());
    v.window_corner_radius = CornerRadius::same(10);
    v.selection.bg_fill = accent().gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, accent());
    v.hyperlink_color = accent();
    v.slider_trailing_fill = true;
    // TextEdit wells use extreme_bg_color, not the widget fills.
    v.extreme_bg_color = input_bg();

    let set = |w: &mut egui::style::WidgetVisuals, fill: Color32| {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.corner_radius = CornerRadius::same(ROUND);
        w.bg_stroke = Stroke::new(1.0, border());
        w.fg_stroke = Stroke::new(1.0, text());
    };
    set(&mut v.widgets.inactive, input_bg());
    set(&mut v.widgets.hovered, input_bg().gamma_multiply(1.15));
    set(&mut v.widgets.active, input_bg().gamma_multiply(0.9));
    set(&mut v.widgets.open, input_bg());
    v.widgets.noninteractive.bg_fill = surface();
    v.widgets.noninteractive.corner_radius = CornerRadius::same(ROUND);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border());
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text());
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, accent());

    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 26.0;
    });
}
