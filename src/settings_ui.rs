//! Settings window (tray → "Settings…") — an egui form over `settings.json`,
//! skinned to the SageThumbs 2K "2026" look: the brand blue #4a90f5 on custom
//! rounded checkboxes and primary buttons, carded sections on the theme
//! surface, Segoe UI (loaded from the system) instead of egui's default font,
//! and API keys / text replacements managed in centered modals rather than
//! inline walls of text. Key testing probes every key **in parallel** against
//! the real provider API (the same probe prewarm uses).
//!
//! The JSON file stays the source of truth — this is just a friendly editor.
//!
//! ## Headless screenshots (UI testing without screen control)
//! Set `QUICKDICTATE_UI_SHOT=<path.png>` and the window captures *itself* via
//! egui's viewport screenshot a few frames after opening, writing the PNG to
//! that path (`QUICKDICTATE_UI_OPEN=keys|replacements` first opens a modal).
//! `scripts/ui_shot.ps1` wraps the whole loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};

use crate::config::Config;
use crate::state::App;
use crate::theme;

/// Whether the settings window is currently *visible*.
///
/// winit only permits ONE event loop per process (a second `EventLoop::build`
/// returns `RecreationAttempt`), so we can't tear the window down and re-create
/// it on the next open. Instead the loop stays alive for the process's life and
/// we hide / show its window. This flag tracks that visibility so a repeat
/// "Settings" click can tell "already open → just focus" from "hidden →
/// re-seed and reveal". See [`LAUNCHED`] and [`SHOW_REQUESTED`].
static OPEN: AtomicBool = AtomicBool::new(false);

/// Whether the one-per-process settings event loop has been started. Once true
/// it stays true: the loop runs until the app exits (winit can't recreate it).
static LAUNCHED: AtomicBool = AtomicBool::new(false);

/// A pending request (from the tray thread) for the running loop to reveal its
/// window. Consumed in [`SettingsApp::logic`], which also wakes on it.
static SHOW_REQUESTED: AtomicBool = AtomicBool::new(false);

/// A clone of the settings window's egui [`egui::Context`], stashed when the
/// loop starts so the tray thread can wake a hidden window via
/// `request_repaint` (which makes eframe call `logic` even while hidden).
static SETTINGS_CTX: OnceLock<egui::Context> = OnceLock::new();

/// Set once `apply_fonts` manages to load a Windows icon font (Segoe Fluent
/// Icons / MDL2 Assets). Section headers only draw their leading glyph when
/// this is true, so a machine missing the font degrades to plain titles rather
/// than tofu boxes.
static ICONS_OK: AtomicBool = AtomicBool::new(false);

fn icons_available() -> bool {
    ICONS_OK.load(Ordering::Relaxed)
}

// ---- Palette (egui-side) --------------------------------------------------

fn c((r, g, b): (u8, u8, u8)) -> Color32 {
    Color32::from_rgb(r, g, b)
}
fn accent() -> Color32 {
    c(theme::ACCENT_RGB)
}
fn accent_hot() -> Color32 {
    c(theme::ACCENT_HOT_RGB)
}
fn accent_press() -> Color32 {
    c(theme::ACCENT_PRESS_RGB)
}
fn bg() -> Color32 {
    c(theme::bg_rgb())
}
fn surface() -> Color32 {
    c(theme::surface_rgb())
}
fn input_bg() -> Color32 {
    c(theme::input_rgb())
}
fn border() -> Color32 {
    c(theme::border_rgb())
}
fn text() -> Color32 {
    c(theme::text_rgb())
}
fn muted() -> Color32 {
    c(theme::muted_rgb())
}
fn good() -> Color32 {
    Color32::from_rgb(63, 185, 80)
}
fn bad() -> Color32 {
    Color32::from_rgb(220, 90, 90)
}

const ROUND: u8 = 6;

/// (id, label) for the provider dropdown. Google only exists in builds with
/// the `google` feature (the published binaries have it).
fn providers() -> Vec<(&'static str, &'static str)> {
    let mut v = vec![
        ("elevenlabs", "ElevenLabs"),
        ("deepgram", "Deepgram"),
        ("openai", "OpenAI"),
        ("assemblyai", "AssemblyAI"),
        ("dashscope", "DashScope (Alibaba)"),
    ];
    if cfg!(feature = "google") {
        v.push(("google", "Google (batch)"));
    }
    v
}

fn provider_label(id: &str) -> &str {
    providers()
        .iter()
        .find(|(pid, _)| *pid == id)
        .map(|(_, l)| *l)
        .unwrap_or("Unknown")
}

fn keys_of<'a>(cfg: &'a mut Config, id: &str) -> &'a mut Vec<String> {
    match id {
        "deepgram" => &mut cfg.deepgram_keys,
        "openai" => &mut cfg.openai_keys,
        "assemblyai" => &mut cfg.assemblyai_keys,
        "dashscope" => &mut cfg.dashscope_keys,
        "google" => &mut cfg.google_keys,
        _ => &mut cfg.elevenlabs_keys,
    }
}

/// `sk_c35a…dad4d0` — enough to recognize a key, never the whole secret.
fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 12 {
        return key.to_string();
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 6..].iter().collect();
    format!("{head}\u{2026}{tail}")
}

// ---- Hotkey recording ------------------------------------------------------

/// Map an egui key to QuickDictate's hotkey name (matching `hotkeys::vk_for`);
/// `None` for keys the parser doesn't support (F25+, symbols, keypad).
fn egui_key_name(key: egui::Key) -> Option<&'static str> {
    use egui::Key::*;
    Some(match key {
        A => "a",
        B => "b",
        C => "c",
        D => "d",
        E => "e",
        F => "f",
        G => "g",
        H => "h",
        I => "i",
        J => "j",
        K => "k",
        L => "l",
        M => "m",
        N => "n",
        O => "o",
        P => "p",
        Q => "q",
        R => "r",
        S => "s",
        T => "t",
        U => "u",
        V => "v",
        W => "w",
        X => "x",
        Y => "y",
        Z => "z",
        Num0 => "0",
        Num1 => "1",
        Num2 => "2",
        Num3 => "3",
        Num4 => "4",
        Num5 => "5",
        Num6 => "6",
        Num7 => "7",
        Num8 => "8",
        Num9 => "9",
        F1 => "f1",
        F2 => "f2",
        F3 => "f3",
        F4 => "f4",
        F5 => "f5",
        F6 => "f6",
        F7 => "f7",
        F8 => "f8",
        F9 => "f9",
        F10 => "f10",
        F11 => "f11",
        F12 => "f12",
        F13 => "f13",
        F14 => "f14",
        F15 => "f15",
        F16 => "f16",
        F17 => "f17",
        F18 => "f18",
        F19 => "f19",
        F20 => "f20",
        F21 => "f21",
        F22 => "f22",
        F23 => "f23",
        F24 => "f24",
        Space => "space",
        Enter => "enter",
        Tab => "tab",
        Backspace => "backspace",
        Delete => "delete",
        Insert => "insert",
        Home => "home",
        End => "end",
        PageUp => "pageup",
        PageDown => "pagedown",
        ArrowUp => "up",
        ArrowDown => "down",
        ArrowLeft => "left",
        ArrowRight => "right",
        _ => return None,
    })
}

/// Build a combo string ("ctrl+shift+f14") from a captured key + modifiers.
fn combo_from_event(key: egui::Key, mods: egui::Modifiers) -> Option<String> {
    let name = egui_key_name(key)?;
    let mut parts: Vec<&str> = Vec::new();
    if mods.ctrl || mods.command {
        parts.push("ctrl");
    }
    if mods.alt {
        parts.push("alt");
    }
    if mods.shift {
        parts.push("shift");
    }
    parts.push(name);
    Some(parts.join("+"))
}

// ---- Text-replacement bulk editor ------------------------------------------

/// Serialize replacements to `from => to` lines for the bulk text editor.
fn replacements_to_text(rows: &[(String, String)]) -> String {
    rows.iter()
        .map(|(f, t)| format!("{f} => {t}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse `from => to` (or `from = to`) lines back into rows. Blank lines and
/// lines with no separator are skipped.
fn text_to_replacements(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (f, t) = if let Some(i) = line.find("=>") {
                (&line[..i], &line[i + 2..])
            } else if let Some(i) = line.find('=') {
                (&line[..i], &line[i + 1..])
            } else {
                return None;
            };
            let f = f.trim().to_string();
            (!f.is_empty()).then(|| (f, t.trim().to_string()))
        })
        .collect()
}

/// The QuickDictate logo as a window icon (same art as the tray/exe icon).
fn icon_data() -> egui::IconData {
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
fn apply_fonts(ctx: &egui::Context) {
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

fn semibold(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("semibold".into()))
}

/// A glyph from the Windows icon font (see `apply_fonts`). Used for the small
/// accent-blue symbol that leads each section header.
fn icon_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("icons".into()))
}

/// Label for the overflow (⋯) menu button: the Windows "More" icon (three dots)
/// when the icon font is present, else an ASCII fallback. Plain Segoe UI lacks
/// the U+22EF ellipsis glyph, so a raw "\u{22EF}" would render as tofu.
fn overflow_glyph() -> RichText {
    if icons_available() {
        RichText::new("\u{E712}").font(icon_font(16.0)) // MDL2 "More"
    } else {
        RichText::new("...").strong()
    }
}

/// Down-chevron label for the Save split-button dropdown, white-on-accent.
/// Uses the icon font's ChevronDown when available, else an ASCII "v".
fn chevron_down_glyph() -> RichText {
    if icons_available() {
        RichText::new("\u{E70D}") // MDL2 "ChevronDown"
            .font(icon_font(12.0))
            .color(Color32::WHITE)
    } else {
        RichText::new("v").color(Color32::WHITE)
    }
}

/// SageThumbs-flavoured egui visuals: theme surfaces, hairline borders, the
/// brand blue for selection/links, generous rounding.
fn apply_style(ctx: &egui::Context) {
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

// ---- Custom widgets -------------------------------------------------------

/// SageThumbs-style checkbox: rounded square, brand-blue fill + white check
/// when on, input-well + hairline border when off. The whole row is clickable.
fn blue_check(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    let box_side = 18.0;
    let gap = 8.0;
    let text_galley = ui.painter().layout_no_wrap(
        label.to_string(),
        egui::TextStyle::Body.resolve(ui.style()),
        text(),
    );
    let desired = egui::vec2(box_side + gap + text_galley.size().x, box_side.max(20.0));
    let (rect, mut resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let box_rect = egui::Rect::from_min_size(
            egui::pos2(rect.min.x, rect.center().y - box_side / 2.0),
            egui::vec2(box_side, box_side),
        );
        let hovered = resp.hovered();
        if *on {
            let fill = if resp.is_pointer_button_down_on() {
                accent_press()
            } else if hovered {
                accent_hot()
            } else {
                accent()
            };
            p.rect_filled(box_rect, CornerRadius::same(5), fill);
            // White check mark.
            let s = box_side;
            let a = box_rect.min + egui::vec2(0.24 * s, 0.52 * s);
            let b = box_rect.min + egui::vec2(0.43 * s, 0.72 * s);
            let d = box_rect.min + egui::vec2(0.78 * s, 0.30 * s);
            let stroke = Stroke::new(2.0, Color32::WHITE);
            p.line_segment([a, b], stroke);
            p.line_segment([b, d], stroke);
        } else {
            p.rect_filled(box_rect, CornerRadius::same(5), input_bg());
            p.rect_stroke(
                box_rect,
                CornerRadius::same(5),
                Stroke::new(1.0, if hovered { accent() } else { border() }),
                egui::StrokeKind::Inside,
            );
        }
        p.galley(
            egui::pos2(
                box_rect.max.x + gap,
                rect.center().y - text_galley.size().y / 2.0,
            ),
            text_galley,
            text(),
        );
    }
    resp
}

/// A plain seconds text box bound to a `u64` **millisecond** config field, with
/// a small "s" unit label beside it. The config stores durations in ms, but
/// users think in seconds, so we present seconds (one decimal) and write the
/// rounded ms value back. Returns the text field's response so the caller can
/// attach a hover tooltip. `id_source` must be unique per field.
///
/// This is a normal input box — no click-and-drag (which felt "dumb" for a
/// value you just want to type). Two immediate-mode wrinkles it handles:
///
/// * **Don't fight the typist.** If we rebuilt the text from `ms` every frame,
///   typing "1." would reformat to "1.0" mid-keystroke and the decimal could
///   never be entered. So while the box is focused we keep the user's raw text
///   in egui memory and leave it alone; we only re-sync the display from `ms`
///   (tidying "2" -> "2.0") once focus leaves.
/// * **Only clamp on an actual edit.** A power user can hand-set a value
///   *outside* `range_secs` in settings.json; merely opening Settings must show
///   it as-is, not silently clamp it back into the draft (which would clobber
///   their choice on the next Save). So we parse + clamp + write only on the
///   frames the text actually changes — matching the old control's `Edits`
///   clamping, not `Always`.
fn secs_input(
    ui: &mut egui::Ui,
    ms: &mut u64,
    range_secs: std::ops::RangeInclusive<f32>,
    id_source: &str,
) -> egui::Response {
    let id = ui.make_persistent_id(("secs_input", id_source));
    let editing = ui.memory(|m| m.focused()) == Some(id);
    // The tidy display value; also the seed for a fresh edit.
    let canonical = format!("{:.1}", *ms as f32 / 1000.0);
    // While editing, preserve the user's in-progress text; otherwise mirror ms.
    let mut buf = if editing {
        ui.memory_mut(|m| m.data.get_temp::<String>(id))
            .unwrap_or_else(|| canonical.clone())
    } else {
        canonical.clone()
    };

    let resp = ui
        .horizontal(|ui| {
            let r = ui.add(styled_input(&mut buf).id(id).desired_width(48.0));
            ui.add_space(2.0);
            ui.weak("s");
            r
        })
        .inner;

    // Commit only when the text actually changed this frame (see doc comment):
    // parse the number, clamp into range, and write the rounded ms back.
    if resp.changed() {
        if let Ok(secs) = buf.trim().parse::<f32>() {
            let clamped = secs.clamp(*range_secs.start(), *range_secs.end());
            *ms = (clamped * 1000.0).round() as u64;
        }
    }

    ui.memory_mut(|m| m.data.insert_temp(id, buf));
    resp
}

/// A single-line text field at the shared control height. Combo boxes and
/// buttons are `row_height + 2*button_padding.y` tall; giving the text well the
/// same vertical margin makes every input, dropdown and button line up.
fn styled_input(value: &mut String) -> egui::TextEdit<'_> {
    let pad = CTRL_PAD;
    egui::TextEdit::singleline(value).margin(Margin::symmetric(6, pad))
}

/// Vertical inner padding shared by text wells and buttons (see `styled_input`
/// and `apply_style`'s `button_padding.y`), so their heights match.
const CTRL_PAD: i8 = 6;

// Hover-tooltip copy shared between a grid label and its control (and, for the
// hotkeys, the `hotkey_field_ui` helper) so both surfaces explain the same item.
const TIP_LANGUAGE: &str = "BCP-47 language tag for transcription, e.g. en-US, es-ES, or fr-FR.";
const TIP_MODE: &str = "toggle: tap the hotkey to start, tap again to stop.  \
     hold: dictate only while the hotkey is held down.";
const TIP_TOGGLE_HOTKEY: &str = "Tap this key to start dictating; tap again to stop. \
     Click the dot in the field to record a new key.";
const TIP_HOLD_HOTKEY: &str = "Hold this key to dictate; release to stop. \
     Click the dot in the field to record a new key.";
const TIP_REPASTE: &str = "Hold your toggle hotkey this long to re-paste your most recent \
     dictation. Takes effect after a restart.";
const TIP_LISTEN_TAIL: &str = "After you stop talking, QuickDictate keeps listening this long \
     before finalizing — raise it if trailing words get cut off, lower it for a snappier finish. \
     Applies to your next dictation.";

/// Filled brand-blue primary button.
fn accent_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let btn = egui::Button::new(RichText::new(label).color(Color32::WHITE))
        .fill(accent())
        .corner_radius(CornerRadius::same(ROUND))
        .stroke(Stroke::NONE);
    let resp = ui.add(btn);
    if resp.hovered() {
        let r = resp.rect;
        ui.painter()
            .rect_filled(r, CornerRadius::same(ROUND), Color32::from_white_alpha(10));
    }
    resp
}

/// A `menu_button` tinted brand-blue so it reads as the dropdown half of a
/// split button sitting next to an [`accent_button`]. The menu popup itself
/// renders in the global menu style (it's a separate `Area`), so this local
/// visuals tweak only colors the trigger, not the items. Returns the trigger's
/// `Response` (for `.on_hover_text`).
fn accent_menu_button<R>(
    ui: &mut egui::Ui,
    label: RichText,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::Response {
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        for state in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            state.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        }
        w.inactive.weak_bg_fill = accent();
        w.hovered.weak_bg_fill = accent_hot();
        w.active.weak_bg_fill = accent_press();
        w.open.weak_bg_fill = accent_press();
        ui.menu_button(label, add).response
    })
    .inner
}

/// A little status chip (● label) used for key verdicts.
fn chip(ui: &mut egui::Ui, label: &str, color: Color32) {
    let frame = egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(8, 2));
    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.0, color);
            ui.label(RichText::new(label).size(12.0).color(color));
        });
    });
}

/// Card section: surface fill, hairline border, rounded, padded.
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(surface())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A section header: a small accent-blue icon glyph followed by the title.
/// `icon` is a Segoe icon-font codepoint (see `apply_fonts`); it's skipped
/// silently on machines where the icon font failed to load.
fn section_title(ui: &mut egui::Ui, icon: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if icons_available() {
            ui.label(RichText::new(icon).font(icon_font(16.0)).color(accent()));
        }
        ui.label(RichText::new(title).font(semibold(15.0)).color(text()));
    });
    ui.add_space(2.0);
}

// ---- State ----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Verdict {
    Untested,
    Testing,
    Ok,
    Fail,
}

struct KeyRow {
    value: String,
    verdict: Verdict,
}

/// Which hotkey field a "Record" button is currently listening for.
#[derive(Clone, Copy, PartialEq)]
enum HotkeyField {
    Toggle,
    Hold,
}

enum Modal {
    Keys {
        rows: Vec<KeyRow>,
        add_text: String,
    },
    Replacements {
        rows: Vec<(String, String)>,
        add_from: String,
        add_to: String,
        /// Bulk "text editor" mode: edit all replacements as `from = to` lines
        /// so a big set can be pasted/copied at once.
        bulk: bool,
        bulk_text: String,
    },
}

/// Open `quickdictate.log` next to the exe (or the exe folder if no log yet).
/// Moved here from the tray menu.
fn open_log_file() {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let log = dir.join("quickdictate.log");
    if log.exists() {
        let _ = std::process::Command::new("notepad.exe").arg(&log).spawn();
    } else {
        let _ = std::process::Command::new("explorer.exe").arg(&dir).spawn();
    }
}

pub fn show_settings(app: Arc<App>) {
    // The window's winit event loop can only be created ONCE per process. If
    // it's already running, don't spawn a second `run_native` (that would fail
    // with `RecreationAttempt` and silently do nothing — the old "won't reopen"
    // bug). Instead ask the live loop to reveal its (possibly hidden) window and
    // wake it so `logic` runs and acts on the request.
    if LAUNCHED.swap(true, Ordering::AcqRel) {
        SHOW_REQUESTED.store(true, Ordering::Release);
        if let Some(ctx) = SETTINGS_CTX.get() {
            ctx.request_repaint();
        }
        return;
    }

    OPEN.store(true, Ordering::Release);
    std::thread::Builder::new()
        .name("qd-settings".into())
        .spawn(move || {
            let options = eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default()
                    .with_inner_size([600.0, 772.0])
                    .with_min_inner_size([520.0, 480.0])
                    .with_icon(Arc::new(icon_data())),
                // The tray thread owns the "main" loop; winit on Windows is
                // fine running this window's loop on a worker thread.
                event_loop_builder: Some(Box::new(|builder| {
                    use winit::platform::windows::EventLoopBuilderExtWindows;
                    builder.with_any_thread(true);
                })),
                ..Default::default()
            };
            let result = eframe::run_native(
                "QuickDictate Settings",
                options,
                Box::new(move |cc| {
                    apply_fonts(&cc.egui_ctx);
                    apply_style(&cc.egui_ctx);
                    // Everything ~10% smaller than the (slightly oversized)
                    // default. A single zoom scales fonts, spacing, control
                    // heights and margins together for a uniform trim.
                    cc.egui_ctx.set_zoom_factor(0.9);
                    // Stash the context so a later "Settings" click (from the
                    // tray thread) can wake this loop even while it's hidden.
                    let _ = SETTINGS_CTX.set(cc.egui_ctx.clone());
                    Ok(Box::new(SettingsApp::new(app)))
                }),
            );
            if let Err(e) = result {
                tracing::error!("settings window: {e}");
            }
            // The loop returns only on real shutdown (or an error). winit won't
            // let us build another, so `LAUNCHED` intentionally stays set.
            OPEN.store(false, Ordering::Release);
        })
        .ok();
}

// ---- Connections settings-sync UI state ------------------------------------

/// Visible state of the opt-in "Sync settings with Connections" control.
#[derive(Clone, Copy, PartialEq)]
enum SyncPhase {
    /// No creds on disk — show the opt-in button.
    SignedOut,
    /// Interactive sign-in underway (browser round-trip).
    SigningIn,
    /// Creds present — synced (a background pull/push may still be in flight).
    SignedIn,
}

/// Results streamed back from a sync worker thread, drained each frame.
enum SyncEvent {
    /// Sign-in or silent resume finished.
    Connected(Result<crate::sync::Connected, String>),
    /// A Save-triggered push finished.
    Pushed(Result<u64, String>),
    /// Disconnect finished (remote doc deleted + local creds dropped).
    Disconnected,
}

/// UI-side sync state (the mechanics live in `crate::sync`).
struct SyncUi {
    phase: SyncPhase,
    email: String,
    /// Display name from /oauth/userinfo, shown next to the status note. Empty for creds saved
    /// before we fetched it (backfilled on the next silent resume) → the UI then just omits it.
    name: String,
    /// Avatar texture (uploaded on the UI thread from decoded bytes a sync worker returns). `None`
    /// until a resume/sign-in resolves the profile picture, or if there is none.
    avatar: Option<egui::TextureHandle>,
    /// One-line status/error under the control.
    note: String,
    is_error: bool,
    /// Receiver for the currently in-flight worker (if any).
    rx: Option<mpsc::Receiver<SyncEvent>>,
    /// Fire the silent resume-pull exactly once, on the first frame.
    resume_kicked: bool,
}

struct SettingsApp {
    app: Arc<App>,
    draft: Config,
    modal: Option<Modal>,
    /// Which hotkey field (if any) is currently recording a keypress.
    recording: Option<HotkeyField>,
    /// Latest per-key verdicts for the active provider (fed by parallel tests).
    verdicts: Vec<(String, bool)>,
    test_rx: Option<mpsc::Receiver<(String, bool)>>,
    testing_left: usize,
    status: String,
    /// Connections settings-sync control state.
    sync: SyncUi,
    // -- headless screenshot hook (QUICKDICTATE_UI_SHOT) --
    shot_path: Option<String>,
    frames: u32,
    shot_requested: bool,
}

impl SettingsApp {
    fn new(app: Arc<App>) -> Self {
        let draft = (*app.config.load_full()).clone();
        // Seed the sync control from any DPAPI-sealed creds already on disk.
        let creds = crate::sync::load_creds();
        let signed_in = creds.is_some();
        let (email, name) = creds.map(|c| (c.email, c.name)).unwrap_or_default();
        let sync = SyncUi {
            phase: if signed_in {
                SyncPhase::SignedIn
            } else {
                SyncPhase::SignedOut
            },
            email,
            name,
            avatar: None,
            note: String::new(),
            is_error: false,
            rx: None,
            resume_kicked: false,
        };
        Self {
            app,
            draft,
            modal: None,
            recording: None,
            verdicts: Vec::new(),
            test_rx: None,
            testing_left: 0,
            status: String::new(),
            sync,
            shot_path: std::env::var("QUICKDICTATE_UI_SHOT").ok(),
            frames: 0,
            shot_requested: false,
        }
    }

    /// Reset the editable draft and transient UI state so a re-opened (was
    /// hidden, now shown) window looks exactly like a fresh open — the same
    /// state [`SettingsApp::new`] builds — rather than showing whatever was left
    /// on screen when the user last closed it. Deliberately drops any unsaved
    /// edits, which matches the previous behavior (a brand-new window per open).
    fn reseed_for_reopen(&mut self) {
        self.draft = (*self.app.config.load_full()).clone();
        self.modal = None;
        self.recording = None;
        self.verdicts.clear();
        self.test_rx = None;
        self.testing_left = 0;
        self.status.clear();

        // Re-seed the sync control from creds on disk and re-arm the one-shot
        // silent resume-pull so a re-open also refreshes from the cloud.
        let creds = crate::sync::load_creds();
        self.sync.phase = if creds.is_some() {
            SyncPhase::SignedIn
        } else {
            SyncPhase::SignedOut
        };
        let (email, name) = creds.map(|c| (c.email, c.name)).unwrap_or_default();
        self.sync.email = email;
        self.sync.name = name;
        self.sync.avatar = None;
        self.sync.note.clear();
        self.sync.is_error = false;
        self.sync.rx = None;
        self.sync.resume_kicked = false;
    }

    // ---- Connections settings-sync ---------------------------------------

    /// Spawn a sync worker; its `SyncEvent` result is drained in `update`.
    /// Only one runs at a time (`self.sync.rx`), which serializes the
    /// sign-in / resume / push / disconnect operations.
    fn spawn_sync(
        &mut self,
        ctx: &egui::Context,
        job: impl FnOnce() -> SyncEvent + Send + 'static,
    ) -> bool {
        // One operation at a time: never overwrite a live receiver (that would
        // silently drop the running job's result).
        if self.sync.rx.is_some() {
            return false;
        }
        let (tx, rx) = mpsc::channel();
        self.sync.rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("qd-sync".into())
            .spawn(move || {
                let ev = job();
                let _ = tx.send(ev);
                ctx.request_repaint();
            })
            .ok();
        true
    }

    /// Drain finished sync work and reflect it into the UI + local config.
    fn drain_sync(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if let Some(rx) = &self.sync.rx {
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
        }
        if events.is_empty() {
            return;
        }
        self.sync.rx = None; // one operation per receiver
        for e in events {
            match e {
                SyncEvent::Connected(Ok(c)) => {
                    self.sync.phase = SyncPhase::SignedIn;
                    self.sync.email = c.email.clone();
                    self.sync.name = c.name.clone();
                    if let Some((w, h, rgba)) = &c.avatar {
                        let img = egui::ColorImage::from_rgba_unmultiplied(
                            [*w as usize, *h as usize],
                            rgba,
                        );
                        self.sync.avatar =
                            Some(ctx.load_texture("cnx-avatar", img, egui::TextureOptions::LINEAR));
                    }
                    self.sync.is_error = false;
                    if let Some(remote) = &c.remote {
                        if crate::sync::apply_synced_to_config(&mut self.draft, remote) {
                            // Persist + hot-store so the pulled prefs take effect.
                            let path = Config::settings_path();
                            let _ = self.draft.save(&path);
                            self.app.config.store(Arc::new(self.draft.clone()));
                            self.sync.note = "Updated from your Connections account.".into();
                        } else {
                            self.sync.note = "Synced \u{2014} already up to date.".into();
                        }
                    } else if c.seeded {
                        self.sync.note = "Synced \u{2014} your settings are now backed up.".into();
                    } else {
                        self.sync.note = "Synced.".into();
                    }
                }
                SyncEvent::Connected(Err(e)) => {
                    // If creds still decrypt we're really signed in; a failed
                    // resume/pull is non-fatal (local settings keep working).
                    self.sync.phase = if crate::sync::is_signed_in() {
                        SyncPhase::SignedIn
                    } else {
                        SyncPhase::SignedOut
                    };
                    self.sync.is_error = true;
                    self.sync.note = format!("Sync problem: {e}");
                }
                SyncEvent::Pushed(Ok(_)) => {
                    self.sync.is_error = false;
                    self.sync.note = "Saved and synced to your Connections account.".into();
                }
                SyncEvent::Pushed(Err(e)) => {
                    self.sync.is_error = true;
                    self.sync.note = format!("Saved locally, but cloud sync failed: {e}");
                }
                SyncEvent::Disconnected => {
                    self.sync.phase = SyncPhase::SignedOut;
                    self.sync.email.clear();
                    self.sync.name.clear();
                    self.sync.avatar = None;
                    self.sync.is_error = false;
                    self.sync.note = "Disconnected. Settings stay on this device.".into();
                }
            }
        }
    }

    /// Start an interactive sign-in (opens the system browser).
    fn begin_sign_in(&mut self, ctx: &egui::Context) {
        if self.sync.rx.is_some() {
            return;
        }
        let snapshot = crate::sync::config_to_synced(&self.draft);
        self.sync.phase = SyncPhase::SigningIn;
        self.sync.note.clear();
        self.sync.is_error = false;
        self.spawn_sync(ctx, move || {
            SyncEvent::Connected(
                crate::sync::connect_and_reconcile(snapshot).map_err(|e| e.to_string()),
            )
        });
    }

    fn active_keys(&mut self) -> Vec<String> {
        let id = self.draft.stt_provider.clone();
        keys_of(&mut self.draft, &id).clone()
    }

    /// While a hotkey field is recording, capture the next real keypress into
    /// it (Escape cancels). Modifier-only presses are ignored (egui only fires
    /// `Key` events for actual keys, carrying modifiers alongside).
    fn capture_hotkey(&mut self, ctx: &egui::Context) {
        let Some(field) = self.recording else {
            return;
        };
        let captured = ctx.input(|i| {
            for ev in &i.events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } = ev
                {
                    if *key == egui::Key::Escape {
                        return Some(None);
                    }
                    if let Some(combo) = combo_from_event(*key, *modifiers) {
                        return Some(Some(combo));
                    }
                }
            }
            None
        });
        match captured {
            Some(Some(combo)) => {
                match field {
                    HotkeyField::Toggle => self.draft.toggle_hotkey = combo,
                    HotkeyField::Hold => self.draft.hold_hotkey = combo,
                }
                self.recording = None;
            }
            Some(None) => self.recording = None, // Escape cancelled
            None => ctx.request_repaint(),       // keep listening
        }
    }

    fn validate(&self) -> Result<(), String> {
        crate::hotkeys::parse_combo(&self.draft.toggle_hotkey)
            .map_err(|e| format!("Toggle hotkey: {e}"))?;
        crate::hotkeys::parse_combo(&self.draft.hold_hotkey)
            .map_err(|e| format!("Hold hotkey: {e}"))?;
        Ok(())
    }

    fn save(&mut self) -> bool {
        if let Err(e) = self.validate() {
            self.status = format!("Not saved — {e}");
            return false;
        }
        let path = Config::settings_path();
        match self.draft.save(&path) {
            Ok(()) => {
                // Hot-store so per-session settings (paste policy, provider,
                // replacements) apply immediately; hotkeys/prewarm need restart.
                self.app.config.store(Arc::new(self.draft.clone()));
                self.status = "Saved. Hotkey and key changes apply after restart.".into();
                tracing::info!("settings saved via UI to {}", path.display());
                true
            }
            Err(e) => {
                self.status = format!("Save failed: {e}");
                false
            }
        }
    }

    fn save_and_restart(&mut self) {
        if !self.save() {
            return;
        }
        // If syncing, push the latest to the cloud *before* relaunching so the
        // restart never races the network. Best-effort and time-bounded — a
        // dead link won't hold the restart hostage.
        if crate::sync::is_signed_in() {
            let snapshot = crate::sync::config_to_synced(&self.draft);
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(crate::sync::push_now(snapshot));
            });
            let _ = rx.recv_timeout(std::time::Duration::from_secs(6));
        }
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).spawn();
        }
        self.app.shutdown.store(true, Ordering::Release);
    }

    /// Kick off PARALLEL probes for `keys`; verdicts stream back into
    /// `test_rx` and are drained in `update`.
    fn start_key_test(&mut self, ctx: &egui::Context, keys: Vec<String>) {
        if keys.is_empty() || self.test_rx.is_some() {
            return;
        }
        let mut cfg = self.draft.clone();
        cfg.stt_provider = self.draft.stt_provider.clone();
        let (tx, rx) = mpsc::channel();
        self.test_rx = Some(rx);
        self.testing_left = keys.len();
        if let Some(Modal::Keys { rows, .. }) = &mut self.modal {
            for r in rows.iter_mut() {
                if keys.contains(&r.value) {
                    r.verdict = Verdict::Testing;
                }
            }
        }
        let repaint = ctx.clone();
        crate::stt::spawn_key_test(
            &self.app,
            cfg,
            keys,
            Arc::new(move |key, ok| {
                let _ = tx.send((key, ok));
                repaint.request_repaint();
            }),
        );
    }

    fn drain_verdicts(&mut self) {
        let mut done = Vec::new();
        if let Some(rx) = &self.test_rx {
            while let Ok(v) = rx.try_recv() {
                done.push(v);
            }
        }
        for (key, ok) in done {
            self.testing_left = self.testing_left.saturating_sub(1);
            self.verdicts.retain(|(k, _)| *k != key);
            self.verdicts.push((key.clone(), ok));
            if let Some(Modal::Keys { rows, .. }) = &mut self.modal {
                if let Some(r) = rows.iter_mut().find(|r| r.value == key) {
                    r.verdict = if ok { Verdict::Ok } else { Verdict::Fail };
                }
            }
        }
        if self.testing_left == 0 {
            self.test_rx = None;
        }
    }

    // ---- headless screenshot hook ----------------------------------------

    fn screenshot_hook(&mut self, ctx: &egui::Context) {
        let Some(path) = self.shot_path.clone() else {
            return;
        };
        self.frames += 1;
        let mode = std::env::var("QUICKDICTATE_UI_OPEN").unwrap_or_default();
        // Let fonts/layout settle, optionally auto-open a modal for the shot.
        if self.frames == 5 {
            match mode.as_str() {
                "keys" | "keys-test" => self.open_keys_modal(),
                "replacements" => self.open_replacements_modal(),
                "replacements-bulk" => {
                    self.open_replacements_modal();
                    if let Some(Modal::Replacements {
                        rows,
                        bulk,
                        bulk_text,
                        ..
                    }) = &mut self.modal
                    {
                        *bulk_text = replacements_to_text(rows);
                        *bulk = true;
                    }
                }
                _ => {}
            }
        }
        // keys-test: also press "Test all" and shoot once the (parallel)
        // verdicts are in — a headless end-to-end test of the probe pipeline.
        if mode == "keys-test" && self.frames == 20 {
            let keys = self.active_keys();
            self.start_key_test(ctx, keys);
        }
        let ready = if mode == "keys-test" {
            self.frames > 25 && self.test_rx.is_none() && !self.verdicts.is_empty()
        } else {
            self.frames == 14
        };
        if ready && !self.shot_requested {
            self.shot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            let (w, h) = (img.size[0] as u32, img.size[1] as u32);
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|p| p.to_array()).collect();
            if let Some(buf) = image::RgbaImage::from_raw(w, h, bytes) {
                // Write atomically (tmp + rename) so a watcher never observes a
                // half-written / zero-byte file mid-encode. Force PNG — the
                // ".tmp" extension would otherwise defeat format-from-extension.
                let tmp = format!("{path}.tmp");
                match buf.save_with_format(&tmp, image::ImageFormat::Png) {
                    Ok(()) => {
                        let _ = std::fs::rename(&tmp, &path);
                        tracing::info!("settings ui screenshot -> {path}");
                    }
                    Err(e) => tracing::error!("screenshot save failed: {e}"),
                }
            }
        }
        ctx.request_repaint(); // keep frames flowing while the hook is armed
    }

    fn open_keys_modal(&mut self) {
        let rows = self
            .active_keys()
            .into_iter()
            .map(|value| {
                let verdict = self
                    .verdicts
                    .iter()
                    .find(|(k, _)| *k == value)
                    .map(|(_, ok)| if *ok { Verdict::Ok } else { Verdict::Fail })
                    .unwrap_or(Verdict::Untested);
                KeyRow { value, verdict }
            })
            .collect();
        self.modal = Some(Modal::Keys {
            rows,
            add_text: String::new(),
        });
    }

    fn open_replacements_modal(&mut self) {
        let rows = self
            .draft
            .text_replacements
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.modal = Some(Modal::Replacements {
            rows,
            add_from: String::new(),
            add_to: String::new(),
            bulk: false,
            bulk_text: String::new(),
        });
    }

    // ---- modal rendering ---------------------------------------------------

    /// Centered modal card (native `egui::Modal` — handles the dim backdrop,
    /// centering, and Escape/backdrop-to-close). Returns true when the user
    /// dismissed it via backdrop/Escape so the caller can treat that as Cancel.
    fn modal_frame(
        ctx: &egui::Context,
        title: &str,
        width: f32,
        add: impl FnOnce(&mut egui::Ui),
    ) -> bool {
        // egui's default modal frame (`Frame::popup`) hugs the content with a
        // ~6px margin, which reads as cramped for the form-style modals. Give it
        // generous left/right (and a bit of top/bottom) breathing room.
        let frame = egui::Frame::popup(&ctx.global_style()).inner_margin(Margin::symmetric(22, 18));
        egui::Modal::new(egui::Id::new("qd_modal"))
            .backdrop_color(Color32::from_black_alpha(140))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.label(RichText::new(title).font(semibold(16.0)).color(text()));
                ui.add_space(10.0);
                add(ui);
            })
            .should_close()
    }
}

enum ModalAction {
    None,
    Commit,
    Cancel,
}

impl eframe::App for SettingsApp {
    // Runs every frame BEFORE `ui` — and, crucially, also while the window is
    // hidden whenever someone calls `request_repaint` (eframe 0.35). That's the
    // hook that lets the tray re-open us after a "close": we never tear down the
    // one winit event loop this process is allowed (a second one fails to
    // build), we just hide the window and un-hide it on the next request.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Real shutdown (tray "Quit"): actually let the window close so the loop
        // ends and the process can exit cleanly.
        if self.app.shutdown.load(Ordering::Acquire) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // A "Settings" click arrived while we were already running: reveal the
        // window. If it had been hidden, re-seed to a clean slate first so a
        // re-open looks exactly like a fresh open (not the leftover state from
        // when it was last closed).
        if SHOW_REQUESTED.swap(false, Ordering::AcqRel) {
            let was_hidden = !OPEN.swap(true, Ordering::AcqRel);
            if was_hidden {
                self.reseed_for_reopen();
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }

        // Intercept the window close (X button / Alt-F4): cancel the actual
        // close and just hide instead, keeping the event loop alive so Settings
        // can be opened again.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            OPEN.store(false, Ordering::Release);
        }
    }

    // egui 0.35: the framework hands us a root `Ui` (no panel) instead of the
    // old `update(ctx, frame)`. We wrap it in a CentralPanel for the bg + margin.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_verdicts();
        self.drain_sync(&ctx);
        self.capture_hotkey(&ctx);
        self.screenshot_hook(&ctx);

        // On the first frame, if we opened already signed in, silently resume
        // and pull so this machine picks up settings changed on another device.
        if !self.sync.resume_kicked {
            self.sync.resume_kicked = true;
            if crate::sync::is_signed_in() {
                self.spawn_sync(&ctx, || {
                    SyncEvent::Connected(crate::sync::resume_and_pull().map_err(|e| e.to_string()))
                });
            }
        }

        let testing = self.test_rx.is_some();

        // ---- Bottom action bar (pinned; removes the old empty bottom gap) ---
        // About at the far left, Save / Save & Restart at the far right. Clicks
        // are captured into locals and acted on after the panel closures so we
        // never call &mut self methods through nested borrows.
        let mut do_about = false;
        let mut do_save = false;
        let mut do_save_restart = false;
        egui::Panel::bottom("qd_actions")
            .frame(egui::Frame::new().fill(bg()).inner_margin(Margin {
                left: 16,
                right: 16,
                top: 8,
                bottom: 10,
            }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("About").clicked() {
                        do_about = true;
                    }
                    // Overflow menu (⋯): the less-used utilities that used to be a
                    // loose button row at the bottom of the settings body.
                    ui.menu_button(overflow_glyph(), |ui| {
                        ui.set_min_width(170.0);
                        if ui.button("Check for updates").clicked() {
                            // The About window runs the check and shows the result.
                            crate::about::show_about();
                        }
                        if ui.button("Open log file").clicked() {
                            open_log_file();
                        }
                        if ui.button("Edit settings.json").clicked() {
                            let path = Config::settings_path();
                            let _ = std::process::Command::new("notepad.exe").arg(&path).spawn();
                        }
                    })
                    .response
                    .on_hover_text("More: check for updates, open the log, edit settings.json");

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Tighter spacing so Save + its dropdown read as one split
                        // button: [ Save ][▾]. The arrow reveals "Save and restart".
                        ui.spacing_mut().item_spacing.x = 4.0;
                        accent_menu_button(ui, chevron_down_glyph(), |ui| {
                            ui.set_min_width(150.0);
                            if ui.button("Save and restart").clicked() {
                                do_save_restart = true;
                            }
                        })
                        .on_hover_text("More save options");
                        if accent_button(ui, "Save").clicked() {
                            do_save = true;
                        }
                        // Save status fills the gap between the menu and Save.
                        if !self.status.is_empty() {
                            ui.add_space(6.0);
                            ui.label(RichText::new(self.status.clone()).color(muted()));
                        }
                    });
                });
            });

        // ---- Scrollable settings body ---------------------------------------
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(bg()).inner_margin(Margin {
                left: 16,
                right: 16,
                top: 16,
                bottom: 4,
            }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // The old logo / "QuickDictate Settings" / version
                        // header was removed — the window title bar already
                        // names the app and the version lives in About.
                        self.onboarding_banner(ui);
                        self.provider_card(ui, &ctx, testing);
                        ui.add_space(10.0);
                        self.dictation_card(ui);
                        ui.add_space(10.0);
                        // Per-app profiles now live inside the Application card
                        // (toggle + read-only list), so there's no standalone
                        // profiles section. Check-for-updates / log / settings.json
                        // moved to the ⋯ overflow menu in the bottom bar.
                        self.application_card(ui);
                        ui.add_space(10.0);
                        self.sync_card(ui, &ctx);
                        ui.add_space(12.0);
                    });
            });

        // Act on pinned-bar clicks with a clean &mut self.
        if do_about {
            crate::about::show_about();
        }
        if do_save_restart {
            self.save_and_restart();
        }
        if do_save && self.save() && self.sync.phase == SyncPhase::SignedIn {
            if self.sync.rx.is_none() {
                let snapshot = crate::sync::config_to_synced(&self.draft);
                self.sync.note = "Saving to your Connections account\u{2026}".into();
                self.sync.is_error = false;
                self.spawn_sync(&ctx, move || {
                    SyncEvent::Pushed(crate::sync::push_now(snapshot).map_err(|e| e.to_string()))
                });
            } else {
                self.sync.note = "Saved locally \u{2014} cloud sync busy, it'll catch up.".into();
            }
        }

        self.render_modal(&ctx);
    }
}

impl SettingsApp {
    /// First-run onboarding banner, pinned above the provider card while *no*
    /// provider has any key. QuickDictate is unusable until a key is added, so
    /// when we auto-open Settings at launch (see `main`) this makes the very
    /// first action obvious instead of leaving the user to guess. It reads the
    /// live draft, so it vanishes the instant a key is saved into any provider.
    fn onboarding_banner(&mut self, ui: &mut egui::Ui) {
        if !self.draft.providers_with_keys().is_empty() {
            return;
        }
        let acc = accent();
        egui::Frame::new()
            .fill(acc.gamma_multiply(0.16))
            .stroke(Stroke::new(1.0, acc.gamma_multiply(0.55)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("Add an API key to get started")
                        .font(semibold(15.0))
                        .color(text()),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "QuickDictate is bring-your-own-key. Pick a provider below, then \
                         \"Manage keys\u{2026}\" to paste a key from any one of them \
                         (ElevenLabs, Deepgram, OpenAI, AssemblyAI, DashScope, or Google). \
                         Hit Save & Restart when you're done. Free tiers/trials exist for \
                         several providers — signup links are in the README.",
                    )
                    .size(12.5)
                    .color(muted()),
                );
                ui.add_space(8.0);
                if accent_button(ui, "Manage keys\u{2026}").clicked() {
                    self.open_keys_modal();
                }
            });
        ui.add_space(10.0);
    }

    fn provider_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, testing: bool) {
        card(ui, |ui| {
            section_title(ui, "\u{E720}", "Speech-to-text provider");
            // Dropdown + key actions all on one row (saves a whole row).
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("provider")
                    .width(200.0)
                    .selected_text(provider_label(&self.draft.stt_provider))
                    .show_ui(ui, |ui| {
                        for (id, label) in providers() {
                            if ui
                                .selectable_value(
                                    &mut self.draft.stt_provider,
                                    id.to_string(),
                                    label,
                                )
                                .changed()
                            {
                                self.verdicts.clear();
                            }
                        }
                    })
                    .response
                    .on_hover_text(
                        "Which speech-to-text service transcribes your dictation. Add its \
                         API keys with Manage keys.",
                    );
                if accent_button(ui, "Manage keys\u{2026}")
                    .on_hover_text("Add, remove, or paste API keys for the selected provider.")
                    .clicked()
                {
                    self.open_keys_modal();
                }
                if ui
                    .add_enabled(!testing, egui::Button::new("Test all keys"))
                    .on_hover_text("Check every saved key for this provider against its live API.")
                    .clicked()
                {
                    let keys = self.active_keys();
                    self.start_key_test(ctx, keys);
                }
            });

            // DashScope's region toggle only applies to that provider, so it
            // sits on its own line and only when DashScope is selected.
            if self.draft.stt_provider == "dashscope" {
                ui.add_space(6.0);
                blue_check(ui, &mut self.draft.dashscope_intl, "International account")
                    .on_hover_text(
                        "Use DashScope's international endpoint instead of the mainland-China one.",
                    );
            }

            // (The "N key(s) configured" line was removed as noise — the
            // Manage keys… modal shows the actual keys and their verdicts.)
            let ok_count = self.verdicts.iter().filter(|(_, ok)| *ok).count();
            let fail_count = self.verdicts.iter().filter(|(_, ok)| !*ok).count();
            if ok_count > 0 || fail_count > 0 || testing {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ok_count > 0 {
                        chip(ui, &format!("{ok_count} working"), good());
                    }
                    if fail_count > 0 {
                        chip(ui, &format!("{fail_count} failing"), bad());
                    }
                    if testing {
                        ui.add(egui::Spinner::new().size(14.0));
                        ui.label(RichText::new("testing\u{2026}").color(muted()));
                    }
                });
            }
        });
    }

    /// A hotkey text field with a small, subtle "record" dot tucked into its
    /// right edge (instead of a separate wide button). Click the dot to arm
    /// capture — the next keypress fills the field; click again (or Esc) to
    /// cancel. Armed = a solid accent dot; the field greys while listening so
    /// the keypress can't also land in the text well.
    fn hotkey_field_ui(&mut self, ui: &mut egui::Ui, field: HotkeyField) {
        let recording = self.recording == Some(field);
        let value = match field {
            HotkeyField::Toggle => &mut self.draft.toggle_hotkey,
            HotkeyField::Hold => &mut self.draft.hold_hotkey,
        };
        // The record dot floats over the field's right edge; padding-right on
        // the well keeps typed text from sliding under it.
        let resp = ui.add_enabled(
            !recording,
            styled_input(value).desired_width(140.0).margin(Margin {
                left: 6,
                // Right padding reserves room for the record dot so typed text
                // (even a long combo) never slides under it.
                right: 26,
                top: CTRL_PAD,
                bottom: CTRL_PAD,
            }),
        );
        let resp = resp.on_hover_text(match field {
            HotkeyField::Toggle => TIP_TOGGLE_HOTKEY,
            HotkeyField::Hold => TIP_HOLD_HOTKEY,
        });

        let side = (resp.rect.height() - 6.0).max(12.0);
        let dot_rect = egui::Rect::from_center_size(
            egui::pos2(resp.rect.right() - side / 2.0 - 4.0, resp.rect.center().y),
            egui::vec2(side, side),
        );
        let tag = match field {
            HotkeyField::Toggle => "toggle",
            HotkeyField::Hold => "hold",
        };
        let id = ui.make_persistent_id(("hotkey_record", tag));
        // Sense the click on the dot's rect. Added AFTER the text field, so it
        // sits on top and wins the click over the well beneath it.
        let hit = ui.interact(dot_rect, id, egui::Sense::click());
        let center = dot_rect.center();
        let r = side * 0.26;
        {
            let p = ui.painter();
            if recording {
                p.circle_filled(center, r, accent());
                p.circle_stroke(
                    center,
                    r + 2.5,
                    Stroke::new(1.5, accent().gamma_multiply(0.45)),
                );
            } else {
                let col = if hit.hovered() { accent() } else { muted() };
                p.circle_stroke(center, r, Stroke::new(1.6, col));
                p.circle_filled(center, r * 0.5, col);
            }
        }
        let hit = hit
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(if recording {
                "Listening — press a key (Esc to cancel)"
            } else {
                "Record hotkey"
            });
        if hit.clicked() {
            self.recording = if recording { None } else { Some(field) };
        }
    }

    fn dictation_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            section_title(ui, "\u{E765}", "Dictation");

            // ---- Top: a 2×2 block of labeled inputs / dropdowns ----------
            // Two independent columns, each a [label | control] mini-grid, so
            // the left half's widths never couple to the right half's (a single
            // 4-column grid let the wide Mode/Hold side squeeze the Language/
            // Toggle side). Visually: Language / Mode on top, hotkeys below.
            ui.columns(2, |cols| {
                egui::Grid::new("dict_left")
                    .num_columns(2)
                    .spacing([10.0, 10.0])
                    .show(&mut cols[0], |ui| {
                        ui.label("Language (BCP-47)").on_hover_text(TIP_LANGUAGE);
                        ui.add(styled_input(&mut self.draft.language).desired_width(130.0))
                            .on_hover_text(TIP_LANGUAGE);
                        ui.end_row();
                        ui.label("Toggle hotkey").on_hover_text(TIP_TOGGLE_HOTKEY);
                        self.hotkey_field_ui(ui, HotkeyField::Toggle);
                        ui.end_row();
                    });
                egui::Grid::new("dict_right")
                    .num_columns(2)
                    .spacing([10.0, 10.0])
                    .show(&mut cols[1], |ui| {
                        ui.label("Mode").on_hover_text(TIP_MODE);
                        egui::ComboBox::from_id_salt("mode")
                            .width(120.0)
                            .selected_text(self.draft.mode.clone())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.draft.mode,
                                    "toggle".into(),
                                    "toggle",
                                );
                                ui.selectable_value(&mut self.draft.mode, "hold".into(), "hold");
                            })
                            .response
                            .on_hover_text(TIP_MODE);
                        ui.end_row();
                        ui.label("Hold hotkey").on_hover_text(TIP_HOLD_HOTKEY);
                        self.hotkey_field_ui(ui, HotkeyField::Hold);
                        ui.end_row();
                    });
            });

            // No separator here: the timing row sits snug under the 2×2 block
            // above (same 10px inter-row gap the grids use) so it reads as one
            // group and the card stays short.
            ui.add_space(10.0);

            // ---- Timing levers ------------------------------------------
            // Two "how long" knobs users asked to tune (both stored in ms),
            // laid out label|control in two columns to match the inputs above
            // (they used to be long full-width sliders):
            //  • Hold-to-re-paste: how long holding the toggle hotkey replays
            //    your last dictation. It's a hotkey timing, wired up at launch,
            //    so it applies after a restart.
            //  • Keep-listening tail: how long QuickDictate keeps capturing
            //    after you stop talking before finalizing. Read per session,
            //    so it applies on your next dictation — no restart needed.
            ui.columns(2, |cols| {
                egui::Grid::new("dict_timing_left")
                    .num_columns(2)
                    .spacing([10.0, 10.0])
                    .show(&mut cols[0], |ui| {
                        ui.label("Hold to re-paste").on_hover_text(TIP_REPASTE);
                        secs_input(
                            ui,
                            &mut self.draft.reinsert_hold_ms,
                            0.5..=4.0,
                            "reinsert_hold",
                        )
                        .on_hover_text(TIP_REPASTE);
                        ui.end_row();
                    });
                egui::Grid::new("dict_timing_right")
                    .num_columns(2)
                    .spacing([10.0, 10.0])
                    .show(&mut cols[1], |ui| {
                        ui.label("Keep listening after you stop")
                            .on_hover_text(TIP_LISTEN_TAIL);
                        secs_input(ui, &mut self.draft.listen_tail_ms, 0.3..=3.0, "listen_tail")
                            .on_hover_text(TIP_LISTEN_TAIL);
                        ui.end_row();
                    });
            });

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);

            // ---- Bottom: two columns of checkboxes ----------------------
            // Left column carries the longer labels; the right column ends
            // with the Text-replacements editor button.
            let repl_count = self.draft.text_replacements.len();
            ui.columns(2, |cols| {
                let left = &mut cols[0];
                blue_check(
                    left,
                    &mut self.draft.auto_space,
                    "Auto space between pastes",
                )
                .on_hover_text(
                    "Insert a space before each pasted result so words don't run together.",
                );
                blue_check(
                    left,
                    &mut self.draft.auto_newline,
                    "Auto newline after pastes",
                )
                .on_hover_text("Add a line break after each pasted result.");
                blue_check(
                    left,
                    &mut self.draft.delay_output_till_release,
                    "Hold pastes until release (hybrid)",
                )
                .on_hover_text(
                    "Buffer the transcription and paste it all when you release the hotkey, \
                     instead of streaming words as you speak.",
                );
                blue_check(
                    left,
                    &mut self.draft.enable_text_replacements,
                    "Enable text replacements",
                )
                .on_hover_text(
                    "Apply your misheard-phrase \u{2192} replacement rules to every transcription.",
                );

                let right = &mut cols[1];
                blue_check(right, &mut self.draft.auto_punct, "Auto punctuation").on_hover_text(
                    "Let the provider add commas, periods, and capitalization automatically.",
                );
                blue_check(
                    right,
                    &mut self.draft.mouse_follower_enabled,
                    "Show the cursor pip",
                )
                .on_hover_text("Show a small dot near your text cursor while dictation is active.");
                blue_check(right, &mut self.draft.enable_sound, "Start/stop sounds")
                    .on_hover_text("Play a short sound when dictation starts and stops.");
                right.add_space(4.0);
                // A plain button in a column would stretch full-width (columns
                // use a justified layout); a horizontal wrapper lets it size to
                // its content instead.
                let mut open = false;
                right.horizontal(|ui| {
                    if ui
                        .button(format!("Text replacements\u{2026} ({repl_count})"))
                        .on_hover_text("Edit your misheard-phrase \u{2192} replacement rules.")
                        .clicked()
                    {
                        open = true;
                    }
                });
                if open {
                    self.open_replacements_modal();
                }
            });
        });
    }

    fn application_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            section_title(ui, "\u{E713}", "Application");
            // Seven toggles split across two columns. The wordiest options are
            // trimmed to short labels with the detail moved into their hover
            // tooltips. "Enable per-app profiles" lives here too — it used to
            // sit in its own near-empty card.
            ui.columns(2, |cols| {
                let left = &mut cols[0];
                blue_check(
                    left,
                    &mut self.draft.prewarm_keys,
                    "Probe keys at startup (prewarm)",
                )
                .on_hover_text("On launch, warm up your API keys so the first dictation is fast.");
                blue_check(left, &mut self.draft.run_at_startup, "Start with Windows")
                    .on_hover_text(
                        "Launch QuickDictate automatically when you sign in to Windows.",
                    );
                blue_check(
                    left,
                    &mut self.draft.update_auto_check,
                    "Check for updates daily",
                )
                .on_hover_text("Automatically check for a newer QuickDictate release once a day.");
                blue_check(
                    left,
                    &mut self.draft.profiles_enabled,
                    "Enable per-app profiles",
                )
                .on_hover_text(
                    "Apply per-application overrides for punctuation, spacing, and \
                     replacements based on the app you're typing into.",
                );

                let right = &mut cols[1];
                blue_check(
                    right,
                    &mut self.draft.enable_logging,
                    "Write quickdictate.log",
                )
                .on_hover_text("Write a log file next to the app for troubleshooting.");
                blue_check(
                    right,
                    &mut self.draft.log_transcripts,
                    "Log full dictated text",
                )
                .on_hover_text(
                    "Deep debugging only \u{2014} records the actual text you dictate into \
                         the log file. Leave off for privacy.",
                );
                blue_check(
                    right,
                    &mut self.draft.voice_commands,
                    "\u{201c}Scratch that\u{201d} voice command",
                )
                .on_hover_text(
                    "Say \u{201c}scratch that\u{201d} to automatically undo your last paste.",
                );
            });

            // Read-only "Active profiles" list — shown only when a power user has
            // actually added `profiles` to settings.json. With none configured,
            // the toggle above is the whole story and we don't waste a row on a
            // "None configured" line.
            if !self.draft.profiles.is_empty() {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label(RichText::new("Active profiles").size(12.0).color(muted()));
                ui.add_space(2.0);
                for p in &self.draft.profiles {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&p.name).color(text()));
                        ui.label(RichText::new(p.match_.join(", ")).size(12.0).color(muted()));
                    });
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Edit settings.json to add, remove, or reorder profiles.")
                        .size(12.0)
                        .color(muted()),
                );
            }
        });
    }

    /// Opt-in "Sync settings with Connections" control (spec §6.8): four states
    /// (signed out / signing in / signed in / error) plus a one-line privacy
    /// note. Button clicks are captured into locals and acted on after the card
    /// closure to keep `&mut self` calls out of nested borrows.
    fn sync_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut do_sign_in = false;
        let mut do_disconnect = false;
        card(ui, |ui| {
            section_title(ui, "\u{E895}", "Settings sync");
            let working = self.sync.rx.is_some();
            match self.sync.phase {
                SyncPhase::SignedOut => {
                    if accent_button(ui, "Sync settings")
                        .on_hover_text(
                            "Sign in with a free Connections account to back up your preferences \
                             \u{2014} hotkeys, providers, text replacements (never your API keys) \
                             \u{2014} and sync them to every device you dictate on.",
                        )
                        .clicked()
                        && !working
                    {
                        do_sign_in = true;
                    }
                }
                SyncPhase::SigningIn => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0));
                        ui.label(
                            RichText::new(
                                "Waiting for sign-in \u{2014} finish in your browser\u{2026}",
                            )
                            .color(muted()),
                        );
                    });
                }
                SyncPhase::SignedIn => {
                    ui.horizontal(|ui| {
                        chip(ui, "Synced", good());
                        // Status note sits inline next to the chip (it used to
                        // read "as <account>"; the note is more useful here).
                        // The chip already says "Synced", so drop that redundant
                        // prefix from the note ("Synced \u{2014} already up to
                        // date." -> "already up to date."; bare "Synced." -> "").
                        if !self.sync.note.is_empty() {
                            let note = self.sync.note.clone();
                            let inline = note
                                .strip_prefix("Synced \u{2014} ")
                                .or_else(|| note.strip_prefix("Synced."))
                                .unwrap_or(note.as_str())
                                .trim();
                            if !inline.is_empty() {
                                let col = if self.sync.is_error { bad() } else { text() };
                                ui.label(RichText::new(inline.to_string()).color(col));
                            }
                        }
                        // The signed-in account avatar + name, to the right of the status note. The
                        // avatar (circular) is uploaded once userinfo resolves the profile picture;
                        // the name is muted secondary context. Older creds saved before we fetched
                        // them have neither until the next silent resume.
                        if let Some(tex) = &self.sync.avatar {
                            ui.add(
                                egui::Image::from_texture(egui::load::SizedTexture::new(
                                    tex.id(),
                                    egui::vec2(18.0, 18.0),
                                ))
                                .corner_radius(9),
                            );
                        }
                        if !self.sync.name.is_empty() {
                            ui.label(
                                RichText::new(format!("\u{00b7} {}", self.sync.name)).color(muted()),
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add_enabled(!working, egui::Button::new("Stop syncing"))
                                .on_hover_text(
                                    "Disconnect this device and delete your synced settings \
                                         from the cloud.",
                                )
                                .clicked()
                            {
                                do_disconnect = true;
                            }
                            if working {
                                ui.add(egui::Spinner::new().size(14.0));
                            }
                        });
                    });
                }
            }
            // When signed in the note is shown inline next to the chip above,
            // so only render this separate row in the other phases.
            if !matches!(self.sync.phase, SyncPhase::SignedIn) && !self.sync.note.is_empty() {
                ui.add_space(4.0);
                let col = if self.sync.is_error { bad() } else { muted() };
                ui.label(RichText::new(self.sync.note.clone()).size(12.0).color(col));
            }
        });
        if do_sign_in {
            self.begin_sign_in(ctx);
        }
        if do_disconnect {
            self.sync.note.clear();
            self.spawn_sync(ctx, || {
                crate::sync::disconnect();
                SyncEvent::Disconnected
            });
        }
    }

    fn render_modal(&mut self, ctx: &egui::Context) {
        let Some(modal) = &mut self.modal else {
            return;
        };
        let mut action = ModalAction::None;
        let mut test_request: Option<Vec<String>> = None;

        match modal {
            Modal::Keys { rows, add_text } => {
                let title = format!("{} API keys", provider_label(&self.draft.stt_provider));
                let backdrop = Self::modal_frame(ctx, &title, 460.0, |ui| {
                    if rows.is_empty() {
                        ui.label(RichText::new("No keys yet — paste one below.").color(muted()));
                    }
                    let mut remove: Option<usize> = None;
                    for (i, row) in rows.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(mask(&row.value))
                                    .monospace()
                                    .size(13.0)
                                    .color(text()),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("\u{00d7}").clicked() {
                                        remove = Some(i);
                                    }
                                    match row.verdict {
                                        Verdict::Untested => {
                                            chip(ui, "untested", muted());
                                        }
                                        Verdict::Testing => {
                                            ui.add(egui::Spinner::new().size(13.0));
                                        }
                                        Verdict::Ok => chip(ui, "working", good()),
                                        Verdict::Fail => chip(ui, "failed", bad()),
                                    }
                                },
                            );
                        });
                    }
                    if let Some(i) = remove {
                        rows.remove(i);
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let edit = styled_input(add_text)
                            .hint_text("paste a new key\u{2026}")
                            .desired_width(ui.available_width() - 70.0)
                            .font(egui::TextStyle::Monospace);
                        let resp = ui.add(edit);
                        let submitted =
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if (ui.button("Add").clicked() || submitted) && !add_text.trim().is_empty()
                        {
                            rows.push(KeyRow {
                                value: add_text.trim().to_string(),
                                verdict: Verdict::Untested,
                            });
                            add_text.clear();
                        }
                    });
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if accent_button(ui, "Test all").clicked() {
                            test_request = Some(rows.iter().map(|r| r.value.clone()).collect());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if accent_button(ui, "Done").clicked() {
                                action = ModalAction::Commit;
                            }
                            if ui.button("Cancel").clicked() {
                                action = ModalAction::Cancel;
                            }
                        });
                    });
                });
                if backdrop || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                    action = ModalAction::Cancel;
                }
            }
            Modal::Replacements {
                rows,
                add_from,
                add_to,
                bulk,
                bulk_text,
            } => {
                let backdrop = Self::modal_frame(ctx, "Text replacements", 500.0, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Misheard phrase \u{2192} what to type instead.")
                                .color(muted()),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Toggle between the table and a paste-friendly text editor.
                            let label = if *bulk {
                                "Table view"
                            } else {
                                "Text editor\u{2026}"
                            };
                            if ui.button(label).clicked() {
                                if *bulk {
                                    *rows = text_to_replacements(bulk_text);
                                } else {
                                    *bulk_text = replacements_to_text(rows);
                                }
                                *bulk = !*bulk;
                            }
                        });
                    });
                    ui.add_space(8.0);

                    if *bulk {
                        ui.label(
                            RichText::new("One per line:  misheard => replacement")
                                .size(12.0)
                                .color(muted()),
                        );
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical()
                            .max_height(320.0)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(bulk_text)
                                        .font(egui::TextStyle::Monospace)
                                        .desired_rows(14)
                                        .desired_width(f32::INFINITY)
                                        .hint_text("Chat GPT => ChatGPT\nGithub => GitHub"),
                                );
                            });
                    } else {
                        let mut remove: Option<usize> = None;
                        egui::ScrollArea::vertical()
                            .max_height(280.0)
                            .show(ui, |ui| {
                                for (i, (from, to)) in rows.iter_mut().enumerate() {
                                    ui.horizontal(|ui| {
                                        ui.add(styled_input(from).desired_width(185.0));
                                        ui.label(RichText::new("\u{2192}").color(muted()));
                                        ui.add(styled_input(to).desired_width(185.0));
                                        if ui.button("\u{00d7}").clicked() {
                                            remove = Some(i);
                                        }
                                    });
                                }
                            });
                        if let Some(i) = remove {
                            rows.remove(i);
                        }
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.add(
                                styled_input(add_from)
                                    .hint_text("misheard\u{2026}")
                                    .desired_width(185.0),
                            );
                            ui.label(RichText::new("\u{2192}").color(muted()));
                            ui.add(
                                styled_input(add_to)
                                    .hint_text("replace with\u{2026}")
                                    .desired_width(185.0),
                            );
                            if ui.button("Add").clicked() && !add_from.trim().is_empty() {
                                rows.push((add_from.trim().to_string(), add_to.trim().to_string()));
                                add_from.clear();
                                add_to.clear();
                            }
                        });
                    }

                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if accent_button(ui, "Done").clicked() {
                                action = ModalAction::Commit;
                            }
                            if ui.button("Cancel").clicked() {
                                action = ModalAction::Cancel;
                            }
                        });
                    });
                });
                if backdrop {
                    action = ModalAction::Cancel;
                }
            }
        }

        if let Some(keys) = test_request {
            self.start_key_test(ctx, keys);
        }

        match action {
            ModalAction::Commit => match self.modal.take() {
                Some(Modal::Keys { rows, .. }) => {
                    let id = self.draft.stt_provider.clone();
                    *keys_of(&mut self.draft, &id) = rows
                        .iter()
                        .map(|r| r.value.clone())
                        .filter(|v| !v.is_empty())
                        .collect();
                }
                Some(Modal::Replacements {
                    rows,
                    bulk,
                    bulk_text,
                    ..
                }) => {
                    // If the user left it in text-editor mode, parse that.
                    let final_rows = if bulk {
                        text_to_replacements(&bulk_text)
                    } else {
                        rows
                    };
                    self.draft.text_replacements = final_rows
                        .into_iter()
                        .filter(|(f, _)| !f.trim().is_empty())
                        .collect();
                }
                None => {}
            },
            ModalAction::Cancel => {
                self.modal = None;
            }
            ModalAction::None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_hotkeys_round_trip_through_the_parser() {
        // A bare F-key.
        let f14 = combo_from_event(egui::Key::F14, egui::Modifiers::default()).unwrap();
        assert_eq!(f14, "f14");
        assert!(crate::hotkeys::parse_combo(&f14).is_ok());

        // A modified letter.
        let mods = egui::Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        let combo = combo_from_event(egui::Key::D, mods).unwrap();
        assert_eq!(combo, "ctrl+shift+d");
        assert!(crate::hotkeys::parse_combo(&combo).is_ok());

        // Keys the parser can't use are rejected up front.
        assert!(combo_from_event(egui::Key::F35, egui::Modifiers::default()).is_none());
    }

    #[test]
    fn bulk_replacements_round_trip() {
        let rows = vec![
            ("Chat GPT".to_string(), "ChatGPT".to_string()),
            ("Github".to_string(), "GitHub".to_string()),
        ];
        assert_eq!(text_to_replacements(&replacements_to_text(&rows)), rows);

        // Lenient: `=` separator, blank lines and separator-less lines skipped.
        let parsed = text_to_replacements("a = b\n\n  c=d \nnosep");
        assert_eq!(
            parsed,
            vec![
                ("a".to_string(), "b".to_string()),
                ("c".to_string(), "d".to_string())
            ]
        );
    }
}
