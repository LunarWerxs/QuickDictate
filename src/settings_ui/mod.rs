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
//! ## Layout of this module
//! This file is the hub: shared state ([`SettingsApp`] and the small types it
//! holds), the window plumbing ([`show_settings`]), and the per-frame loop
//! (`impl eframe::App`). Everything that draws or acts lives in a sibling:
//!
//! - [`style`]: palette, fonts, glyphs, and the egui style.
//! - [`widgets`]: reusable controls, cards, and the usage-stats charts.
//! - [`logic`]: construction, validation, saving, sync, hotkey capture.
//! - [`cards`]: onboarding, provider/keys, dictation, application.
//! - [`history_sync`]: the transcript-history browser and the sync card.
//! - [`modals`]: the bulk editors, confirm prompts, and their shared frame.
//!
//! Submodules see the hub (and each other's shared helpers) through
//! `use super::*;`, so moving a card between files needs no import churn.
//!
//! ## Headless screenshots (UI testing without screen control)
//! Set `QUICKDICTATE_UI_SHOT=<path.png>` and the window captures *itself* via
//! egui's viewport screenshot a few frames after opening, writing the PNG to
//! that path (`QUICKDICTATE_UI_OPEN=keys|keys-bulk|keys-test|replacements|
//! replacements-bulk|stats` first opens a modal).
//! `scripts/ui_shot.ps1` wraps the whole loop.
//!
//! ## Changing the window size or the Save button?
//! Read `docs/SETTINGS_WINDOW.md` first. This window runs at 0.9 zoom (so three
//! coordinate systems are in play) and the Save split button has a
//! border/height gotcha. That doc captures the traps so
//! an edit does not turn into a long debugging session.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};

use eframe::egui::containers::menu;
use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};

use crate::config::Config;
use crate::state::{App, HistoryEntry};
use crate::stats::StatsRange;
use crate::theme;

// Split out of this file so each surface can be reviewed on its own; the
// hub keeps the shared state, the window plumbing, and the frame loop.
mod cards;
mod history_sync;
mod logic;
mod modals;
mod nav;
mod style;
mod widgets;

// Shared look and reusable widgets, used by name throughout the hub and, via
// each sibling's own `use super::*;`, throughout the card modules too. The
// other submodules hold only `impl SettingsApp` blocks, so they have no names
// to re-export here.
pub(crate) use style::*;
pub(crate) use widgets::*;

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

// ---- Palette (egui-side) --------------------------------------------------

/// (id, label) for the provider dropdown. Google only exists in builds with
/// the `google` feature (the published binaries have it).
fn providers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("elevenlabs", "ElevenLabs"),
        ("deepgram", "Deepgram"),
        ("openai", "OpenAI"),
        ("assemblyai", "AssemblyAI"),
        ("dashscope", "DashScope (Alibaba)"),
        ("google", "Google (batch)"),
        ("local", "Local (offline)"),
    ]
}

fn provider_label(id: &str) -> &str {
    if id.eq_ignore_ascii_case("mixed") {
        return "Mixed providers";
    }
    providers()
        .iter()
        .find(|(pid, _)| *pid == id)
        .map(|(_, l)| *l)
        .unwrap_or("Unknown")
}

/// `keys_of` id for the cleanup pass's own pool. It is not an STT provider,
/// but it is edited by the very same key manager, so it rides the same path
/// rather than growing a second, near-identical keys editor. NUL-prefixed so
/// `@`-prefixed so it can never collide with a real provider id.
pub(crate) const KEYS_TARGET_POLISH: &str = "@polish";
/// `keys_of` id meaning "whatever STT provider the draft currently selects",
/// resolved at use time so switching providers with the modal shut cannot
/// leave the target pointing at the old one.
pub(crate) const KEYS_TARGET_PROVIDER: &str = "@provider";

fn keys_of<'a>(cfg: &'a mut Config, id: &str) -> &'a mut Vec<String> {
    if id == KEYS_TARGET_POLISH {
        return &mut cfg.polish_keys;
    }
    let selected;
    let id = if id == KEYS_TARGET_PROVIDER {
        selected = cfg.stt_provider.clone();
        selected.as_str()
    } else {
        id
    };
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
    if chars.len() <= 4 {
        return "\u{2022}".repeat(chars.len());
    }
    if chars.len() <= 12 {
        let head: String = chars[..2].iter().collect();
        let tail: String = chars[chars.len() - 2..].iter().collect();
        return format!("{head}\u{2026}{tail}");
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 6..].iter().collect();
    format!("{head}\u{2026}{tail}")
}

// ---- Hotkey recording ------------------------------------------------------

/// `(key, name)` pairs backing [`egui_key_name`]. A plain table, not a
/// `match`, because every arm here is the same trivial "spell it lowercase"
/// rule — the table IS the mapping, with no per-key logic to dispatch on.
const EGUI_KEY_NAMES: &[(egui::Key, &str)] = &[
    (egui::Key::A, "a"),
    (egui::Key::B, "b"),
    (egui::Key::C, "c"),
    (egui::Key::D, "d"),
    (egui::Key::E, "e"),
    (egui::Key::F, "f"),
    (egui::Key::G, "g"),
    (egui::Key::H, "h"),
    (egui::Key::I, "i"),
    (egui::Key::J, "j"),
    (egui::Key::K, "k"),
    (egui::Key::L, "l"),
    (egui::Key::M, "m"),
    (egui::Key::N, "n"),
    (egui::Key::O, "o"),
    (egui::Key::P, "p"),
    (egui::Key::Q, "q"),
    (egui::Key::R, "r"),
    (egui::Key::S, "s"),
    (egui::Key::T, "t"),
    (egui::Key::U, "u"),
    (egui::Key::V, "v"),
    (egui::Key::W, "w"),
    (egui::Key::X, "x"),
    (egui::Key::Y, "y"),
    (egui::Key::Z, "z"),
    (egui::Key::Num0, "0"),
    (egui::Key::Num1, "1"),
    (egui::Key::Num2, "2"),
    (egui::Key::Num3, "3"),
    (egui::Key::Num4, "4"),
    (egui::Key::Num5, "5"),
    (egui::Key::Num6, "6"),
    (egui::Key::Num7, "7"),
    (egui::Key::Num8, "8"),
    (egui::Key::Num9, "9"),
    (egui::Key::F1, "f1"),
    (egui::Key::F2, "f2"),
    (egui::Key::F3, "f3"),
    (egui::Key::F4, "f4"),
    (egui::Key::F5, "f5"),
    (egui::Key::F6, "f6"),
    (egui::Key::F7, "f7"),
    (egui::Key::F8, "f8"),
    (egui::Key::F9, "f9"),
    (egui::Key::F10, "f10"),
    (egui::Key::F11, "f11"),
    (egui::Key::F12, "f12"),
    (egui::Key::F13, "f13"),
    (egui::Key::F14, "f14"),
    (egui::Key::F15, "f15"),
    (egui::Key::F16, "f16"),
    (egui::Key::F17, "f17"),
    (egui::Key::F18, "f18"),
    (egui::Key::F19, "f19"),
    (egui::Key::F20, "f20"),
    (egui::Key::F21, "f21"),
    (egui::Key::F22, "f22"),
    (egui::Key::F23, "f23"),
    (egui::Key::F24, "f24"),
    (egui::Key::Space, "space"),
    (egui::Key::Enter, "enter"),
    (egui::Key::Tab, "tab"),
    (egui::Key::Backspace, "backspace"),
    (egui::Key::Delete, "delete"),
    (egui::Key::Insert, "insert"),
    (egui::Key::Home, "home"),
    (egui::Key::End, "end"),
    (egui::Key::PageUp, "pageup"),
    (egui::Key::PageDown, "pagedown"),
    (egui::Key::ArrowUp, "up"),
    (egui::Key::ArrowDown, "down"),
    (egui::Key::ArrowLeft, "left"),
    (egui::Key::ArrowRight, "right"),
];

/// Map an egui key to QuickDictate's hotkey name (matching `hotkeys::vk_for`);
/// `None` for keys the parser doesn't support (F25+, symbols, keypad).
fn egui_key_name(key: egui::Key) -> Option<&'static str> {
    EGUI_KEY_NAMES
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, name)| *name)
}

/// Prefix a key/button name with whichever modifiers were held, in the fixed
/// order `parse_combo` reads back.
fn with_modifiers(name: &str, mods: egui::Modifiers) -> String {
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
    parts.join("+")
}

/// Build a combo string ("ctrl+shift+f14") from a captured key + modifiers.
fn combo_from_event(key: egui::Key, mods: egui::Modifiers) -> Option<String> {
    Some(with_modifiers(egui_key_name(key)?, mods))
}

/// Build a combo string ("mouse4", "ctrl+mouse3") from a captured mouse
/// button + modifiers, or `None` for a button we refuse to bind.
///
/// Primary and Secondary are deliberately unbindable: binding a mouse button
/// also *suppresses* it system-wide (see [`crate::mouse_hook`]), and someone
/// who loses left-click can no longer click their way back here to undo it.
/// `hotkeys::parse_combo` rejects them for the same reason, so a hand-edited
/// `settings.json` can't sneak one in either. Useful side effect: the Primary
/// click that arms recording is never itself recordable.
fn combo_from_pointer(button: egui::PointerButton, mods: egui::Modifiers) -> Option<String> {
    let name = match button {
        egui::PointerButton::Middle => "mouse3",
        egui::PointerButton::Extra1 => "mouse4",
        egui::PointerButton::Extra2 => "mouse5",
        egui::PointerButton::Primary | egui::PointerButton::Secondary => return None,
    };
    Some(with_modifiers(name, mods))
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
            let (f, t) = line.split_once("=>").or_else(|| line.split_once('='))?;
            let f = f.trim().to_string();
            (!f.is_empty()).then(|| (f, t.trim().to_string()))
        })
        .collect()
}

/// Whether two hotkey combo strings parse to the identical (modifiers, vk)
/// pair — the condition `SettingsApp::validate` rejects, since Windows can
/// only register one of two identical `RegisterHotKey` calls and the loser
/// just silently never fires. An unparsable combo is "not a conflict" here —
/// `validate` already surfaces that parse error on its own before this ever
/// runs. Comparing the *parsed* form (not the raw strings) is what makes this
/// case-insensitive and order-independent, matching `parse_combo`'s own
/// normalisation (e.g. "Ctrl+Shift+D" and "shift+ctrl+d" both parse to the
/// same pair).
fn hotkeys_conflict(a: &str, b: &str) -> bool {
    matches!(
        (crate::hotkeys::parse_combo(a), crate::hotkeys::parse_combo(b)),
        (Ok(x), Ok(y)) if x == y
    )
}

/// Structural equality for two [`Config`]s via their JSON shape. `Config`
/// doesn't derive `PartialEq` (its key lists and nested `Profile`s make a
/// hand-written impl a maintenance trap that silently goes stale as fields
/// are added), so this compares serialized form instead — correct by
/// construction, at the cost of a serialize round-trip. Backs the "unsaved
/// changes" close-confirm (see `SettingsApp::draft_is_dirty`).
fn configs_differ(a: &Config, b: &Config) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(a), Ok(b)) => a != b,
        // Serialization failing is not expected; fail toward "differs" so an
        // unsaved edit is never silently discarded on the strength of a
        // could-not-compare.
        _ => true,
    }
}

/// Parse the multi-line custom-vocabulary editor into the list of terms sent
/// to the provider: one non-blank line per term, trimmed. Only run at save
/// time (see `SettingsApp::fold_vocabulary_into_draft`) — the raw text stays
/// untouched while you're still typing, so a blank line mid-edit is never
/// silently swallowed out from under you.
fn parse_vocabulary(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Case-insensitive substring match for the History section's filter box. An
/// empty (or whitespace-only) filter matches everything.
fn history_matches(filter: &str, text: &str) -> bool {
    let filter = filter.trim();
    filter.is_empty() || text.to_lowercase().contains(&filter.to_lowercase())
}

/// Whether `history_card`'s cached, pre-filtered rows (see [`HistoryCache`])
/// need to be rebuilt: true when the underlying history changed
/// (`TranscriptHistory::version()` bumps on every push/pop) or the filter
/// text itself changed since the cache was last built. Pulled out as a pure
/// function so the invalidation rule is testable without a live history.
fn history_cache_stale(
    cached_version: u64,
    current_version: u64,
    cached_filter: &str,
    current_filter: &str,
) -> bool {
    cached_version != current_version || cached_filter != current_filter
}

/// Shorten a history entry for its one-line list row; the full text stays
/// reachable via the row's hover tooltip. Truncates on a `char` boundary
/// (never splits a multi-byte character) and appends an ellipsis when cut.
fn truncate_preview(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}\u{2026}")
    } else {
        head
    }
}

// ---- Custom widgets -------------------------------------------------------

// Hover-tooltip copy shared between a grid label and its control (and, for the
// hotkeys, the `hotkey_field_ui` helper) so both surfaces explain the same item.
const TIP_LANGUAGE: &str = "BCP-47 language tag for transcription, e.g. en-US, es-ES, or fr-FR.";
const TIP_MODE: &str = "toggle: tap the hotkey to start, tap again to stop.  \
     hold: dictate only while the hotkey is held down.";
const TIP_TOGGLE_HOTKEY: &str = "Tap this key to start dictating; tap again to stop. \
     Click the dot in the field to record a new one \u{2014} a key, or a mouse button \
     (middle, or a thumb button: mouse3 / mouse4 / mouse5). A bound mouse button stops \
     reaching other apps; left and right click can't be bound.";
const TIP_HOLD_HOTKEY: &str = "Hold this key to dictate; release to stop. \
     Click the dot in the field to record a new one \u{2014} a key, or a mouse button \
     (middle, or a thumb button: mouse3 / mouse4 / mouse5). A bound mouse button stops \
     reaching other apps; left and right click can't be bound.";
const TIP_REPASTE: &str = "Hold your toggle hotkey this long to re-paste your most recent \
     dictation. Takes effect after a restart.";
const TIP_LISTEN_TAIL: &str = "After you stop talking, QuickDictate keeps listening this long \
     before finalizing — raise it if trailing words get cut off, lower it for a snappier finish. \
     Applies to your next dictation.";
const TIP_POLISH: &str = "Before pasting, have an AI repair the sentence breaks a pause made \
     the recognizer invent, plus obviously misheard words. It never rewords you: it can only \
     return small exact-match edits, and anything that rewrites more than a quarter of what you \
     said is thrown away. While you are still talking it runs in the background on what you have \
     said so far, so it usually costs nothing at all.";
const TIP_POLISH_WAIT: &str = "The longest a paste will ever wait for that cleanup. If it is \
     not ready in time your text is pasted unpolished — it can never make dictation slower than \
     this.";
/// The setup instructions, kept in one place because they are the whole
/// answer to "I ticked the box and nothing happened".
const TIP_POLISH_KEYS: &str = "Get a free key at aistudio.google.com/apikey, then paste it here \
     (one per line — several keys from different Google projects are rotated, which multiplies \
     your rate limit).\n\nThe key's project needs the \"Generative Language API\" enabled, which \
     is on by default for keys created in AI Studio. A Google key made for Speech-to-Text will \
     NOT work here; they are separate APIs.\n\nRecommended model: gemini-3.5-flash-lite. \
     Measured at ~0.56 s with the best results of everything tested — about 3x faster than \
     GPT-4.1-mini, and faster than the bigger Gemini models, which think before answering and \
     lose the race for no benefit.";

// ---- State ----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyMergeSummary {
    added: usize,
    duplicates: usize,
}

/// Merge one-key-per-line text into the manager without ever echoing a secret.
/// Validation is atomic: an invalid line leaves the existing rows untouched.
fn merge_key_lines(rows: &mut Vec<KeyRow>, text: &str) -> Result<KeyMergeSummary, Vec<usize>> {
    let mut candidates = Vec::new();
    let mut invalid_lines = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let key = line.trim();
        if key.is_empty() {
            continue;
        }
        if key.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            invalid_lines.push(index + 1);
        } else {
            candidates.push(key.to_string());
        }
    }
    if !invalid_lines.is_empty() {
        return Err(invalid_lines);
    }

    let mut seen: HashSet<String> = rows.iter().map(|row| row.value.clone()).collect();
    let mut added = 0usize;
    let mut duplicates = 0usize;
    for key in candidates {
        if seen.insert(key.clone()) {
            rows.push(KeyRow {
                value: key,
                verdict: Verdict::Untested,
            });
            added += 1;
        } else {
            duplicates += 1;
        }
    }
    Ok(KeyMergeSummary { added, duplicates })
}

fn deduped_key_values(rows: &[KeyRow]) -> Vec<String> {
    let mut seen = HashSet::new();
    rows.iter()
        .filter_map(|row| {
            let value = row.value.trim();
            (!value.is_empty() && seen.insert(value.to_string())).then(|| value.to_string())
        })
        .collect()
}

/// Which hotkey field a "Record" button is currently listening for.
#[derive(Clone, Copy, PartialEq)]
enum HotkeyField {
    Toggle,
    Hold,
}

/// The key-manager modal's own scratch state, bundled into one struct so the
/// render function that owns it takes one parameter instead of six.
struct KeysModalState {
    rows: Vec<KeyRow>,
    add_text: String,
    bulk: bool,
    bulk_text: String,
    bulk_note: String,
    bulk_error: bool,
}

/// The text-replacements modal's own scratch state.
struct ReplacementsModalState {
    rows: Vec<(String, String)>,
    add_from: String,
    add_to: String,
    /// Bulk "text editor" mode: edit all replacements as `from = to` lines
    /// so a big set can be pasted/copied at once.
    bulk: bool,
    bulk_text: String,
}

enum Modal {
    Keys(KeysModalState),
    Replacements(ReplacementsModalState),
    Stats,
    /// Confirm-before-destroy for the overflow menu's "Default settings"
    /// (see `SettingsApp::reset_to_defaults`). A plain menu item can't host a
    /// two-step confirm in place — clicking anything in an egui menu closes
    /// it — so the confirm lives here instead, styled like the Stats modal's
    /// own "Reset stats" confirmation.
    DefaultReset,
    /// Shown when the window is asked to close (X / Alt-F4) while `draft`
    /// has edits that were never saved (see `SettingsApp::draft_is_dirty`).
    UnsavedChanges,
    /// Shown before a Save (or Save & Restart) would overwrite settings.json
    /// with a hand-edit still sitting on disk (see
    /// `SettingsApp::external_change_pending`). `SettingsApp::pending_save_kind`
    /// remembers what to actually do once the user picks Overwrite.
    ExternalChange,
}

/// Reveal the dedicated diagnostics directory in Explorer.
fn open_log_folder() {
    let dir = crate::logs_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::process::Command::new("explorer.exe").arg(&dir).spawn();
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
                    // A fixed, comfortable size. This used to open tall and then
                    // auto-fit to the full stacked-card height every frame,
                    // which reached roughly 1160 points (taller than plenty of
                    // laptop screens) and, worse, fought the user: dragging the
                    // edge changed the content's wrap height, which re-sent
                    // InnerSize, which snapped the window back, so a resize
                    // oscillated. One page at a time fits in this box, and
                    // anything taller scrolls inside the pane.
                    .with_inner_size([760.0, 600.0])
                    .with_min_inner_size([620.0, 420.0])
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
    /// Disconnect finished (remote doc deleted + local creds dropped).
    Disconnected,
    /// A plain background push (Save, or the best-effort push before Save &
    /// Restart) finished. Unlike `Connected`, this never touches
    /// `sync.phase`/`email`/`avatar` — it's just "did the write land" —
    /// so `drain_sync` reports it through `self.status` instead.
    Pushed(Result<(), String>),
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

/// Cached, pre-filtered snapshot backing `history_card`, so a frame that
/// changes neither the history nor the filter text doesn't re-lock
/// `app.history` and re-run `history_matches` over every entry from scratch.
/// Rebuilt exactly when [`history_cache_stale`] says `version` or `filter`
/// moved since the last build.
#[derive(Default)]
struct HistoryCache {
    /// `TranscriptHistory::version()` as of the last rebuild.
    version: u64,
    /// `history_filter` as of the last rebuild.
    filter: String,
    /// Whether the *unfiltered* history was empty as of the last rebuild —
    /// cached separately from `rows` so the "no dictations yet" vs. "no
    /// matches" messages in `history_card` stay distinguishable even though
    /// only the filtered rows are kept around.
    history_empty: bool,
    /// `(original index into the live history, cloned entry)` for every entry
    /// matching `filter`, newest first — the original index is what "Copy" /
    /// "Paste again" need to look the entry back up in `app.history`.
    rows: Vec<(usize, HistoryEntry)>,
}

/// A "Save and restart" that saved locally and kicked off a best-effort sync
/// push, waiting for that push to land (or time out) before actually
/// relaunching. See `SettingsApp::save_and_restart` / `poll_pending_restart`.
struct PendingRestart {
    /// Give the push at most this long — a dead network must never hold the
    /// restart hostage indefinitely.
    deadline: std::time::Instant,
}

/// Which action a pre-save "settings.json changed on disk" prompt
/// (`Modal::ExternalChange`) should resume once the user picks Overwrite.
#[derive(Clone, Copy)]
enum PendingSaveKind {
    Plain,
    Restart,
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
    stats_range: StatsRange,
    stats_reset_confirm: bool,
    /// Scratch buffer for the global custom-vocabulary multiline editor —
    /// mirrors `draft.custom_vocabulary` as raw text (one term per line) so
    /// blank lines can exist mid-edit without being swallowed; only parsed
    /// back into `draft` on Save (see `parse_vocabulary`, `save`).
    vocabulary_text: String,
    /// Same idea as `vocabulary_text`, one scratch buffer per entry of
    /// `draft.profiles` (same order). Kept in lockstep with `draft.profiles`
    /// by `resync_vocabulary_scratch`.
    profile_vocab_text: Vec<String>,
    /// Case-insensitive substring filter for the History section.
    history_filter: String,
    /// Cached, pre-filtered rows for `history_card`; see [`HistoryCache`].
    history_cache: HistoryCache,
    /// settings.json's mtime when "Edit settings.json…" was last opened, so a
    /// later Save can tell a hand-edit landed on disk in the meantime. `None`
    /// when no editor session is being tracked (the common case).
    editor_opened_at: Option<std::time::SystemTime>,
    /// Set when `Modal::ExternalChange` is showing, so its Overwrite button
    /// knows whether to resume a plain Save or a Save & Restart.
    pending_save_kind: Option<PendingSaveKind>,
    /// Set by `save_and_restart` while its background sync push is in
    /// flight; polled by `poll_pending_restart`.
    pending_restart: Option<PendingRestart>,
    // -- headless screenshot hook (QUICKDICTATE_UI_SHOT) --
    shot_path: Option<String>,
    frames: u32,
    shot_requested: bool,
    /// Last window inner height (logical pts) we requested via the auto-fit in
    /// `ui`. The window is sized to its content each frame so it can never
    /// scroll and is never taller than needed; this cache gates the resize so we
    /// only issue a viewport command when the content height actually changes
    /// (winit applies `InnerSize` a frame late, so resending every frame would
    /// Which page the nav rail is showing. Kept across a hide/reveal so
    /// reopening Settings lands where you left off.
    tab: nav::Tab,
    /// Which key pool the key manager edits: a provider id,
    /// [`KEYS_TARGET_PROVIDER`], or [`KEYS_TARGET_POLISH`]. Set by
    /// [`SettingsApp::open_keys_modal`] and read by `active_keys` and the
    /// modal's commit, so one editor serves both pools.
    keys_target: String,
    /// The "you could be signed in" ask, when one is currently on screen.
    ///
    /// Held here rather than re-asked per frame on purpose: [`crate::nudge::consider`] MUTATES —
    /// it stamps the ask and advances the ladder — so calling it from a paint function would burn
    /// the user's three lifetime asks in three frames. It is called once, at the moment (a save),
    /// and what it returns lives here until the user answers it.
    nudge_ask: Option<crate::nudge_engine::Ask>,
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

        // Intercept the window close (X button / Alt-F4): cancel the actual OS
        // close (we manage "closing" ourselves as hide-and-reveal-later; see
        // OPEN's doc comment) and either hide right away, or — if the draft
        // has edits that were never saved — ask first instead of silently
        // throwing them away (see `Modal::UnsavedChanges`).
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if self.draft_is_dirty() {
                self.modal = Some(Modal::UnsavedChanges);
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                OPEN.store(false, Ordering::Release);
            }
        }
    }

    // egui 0.35: the framework hands us a root `Ui` (no panel) instead of the
    // old `update(ctx, frame)`. We wrap it in a CentralPanel for the bg + margin.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_verdicts();
        self.drain_sync(&ctx);
        self.poll_pending_restart(&ctx);
        self.capture_hotkey(&ctx);
        self.screenshot_hook(&ctx);

        // On the first frame, if we opened already signed in, silently resume
        // and pull so this machine picks up settings changed on another device.
        if !self.sync.resume_kicked {
            self.sync.resume_kicked = true;
            if crate::sync::is_signed_in() {
                let snapshot =
                    crate::sync::snapshot_to_synced(&self.draft, &self.app.stats.snapshot());
                self.spawn_sync(&ctx, move || {
                    SyncEvent::Connected(
                        crate::sync::resume_and_pull(snapshot).map_err(|e| e.to_string()),
                    )
                });
            }
        }

        let testing = self.test_rx.is_some();

        // ---- Bottom action bar (pinned; removes the old empty bottom gap) ---
        // About at the far left, Save / Save & Restart at the far right. Split
        // out as `bottom_action_bar` purely to keep `ui`'s cognitive load
        // down; the returned flags are acted on below with a clean &mut self.
        let (do_about, do_save, do_save_restart) = self.bottom_action_bar(ui);

        // ---- Scrollable settings body ---------------------------------------
        // ---- Nav rail --------------------------------------------------------
        // Added above the CentralPanel so it occupies the area left of the
        // content and above the bottom bar. Not resizable: it is a fixed rail,
        // not a splitter, and a draggable edge here would be one more thing
        // that can fight the user's window resize.
        egui::Panel::left("nav_rail")
            .exact_size(nav::NAV_W)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(surface())
                    .inner_margin(Margin::symmetric(6, 0)),
            )
            .show(ui, |ui| self.nav_rail(ui));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(bg()).inner_margin(Margin {
                left: 16,
                right: 16,
                top: 16,
                bottom: 4,
            }))
            .show(ui, |ui| {
                // Banners sit ABOVE the pane header and outside the scroll
                // area: "you have no API key" and "an update is waiting" are
                // true regardless of which page you are on, so they must not
                // be something you can navigate away from.
                self.onboarding_banner(ui);
                self.update_available_banner(ui);
                self.sign_in_nudge_banner(ui);
                self.page_header(ui);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Exactly one page. Application carries the app-level
                        // toggles plus the two things that are set once and
                        // rarely revisited (which provider/keys to use, and
                        // whether settings sync is on), so the rail stays
                        // short. Per-app profiles live inside the Application
                        // card; check-for-updates / log / settings.json are in
                        // the ⋯ overflow menu below.
                        match self.tab {
                            nav::Tab::Application => {
                                self.application_card(ui);
                                ui.add_space(10.0);
                                self.provider_card(ui, &ctx, testing);
                                ui.add_space(10.0);
                                self.sync_card(ui, &ctx);
                            }
                            nav::Tab::Dictation => self.dictation_card(ui),
                            nav::Tab::History => self.history_card(ui),
                        }
                        ui.add_space(12.0);
                    });
            });

        // Act on pinned-bar clicks with a clean &mut self.
        if do_about {
            crate::about::show_about();
        }
        // A hand-edit via "Edit settings.json…" may have landed on disk since
        // it was opened; ask Reload/Overwrite first rather than silently
        // clobbering it (see `external_change_pending`, `Modal::ExternalChange`).
        if do_save_restart {
            if self.external_change_pending() {
                self.pending_save_kind = Some(PendingSaveKind::Restart);
                self.modal = Some(Modal::ExternalChange);
            } else {
                self.save_and_restart(&ctx);
            }
        }
        if do_save {
            if self.external_change_pending() {
                self.pending_save_kind = Some(PendingSaveKind::Plain);
                self.modal = Some(Modal::ExternalChange);
            } else {
                self.save_and_sync(&ctx);
            }
        }

        self.render_modal(&ctx);
    }
}

impl SettingsApp {
    /// The pinned bottom bar: About / Stats / overflow menu on the left,
    /// the Save split-button on the right. Returns which of the three
    /// buttons were clicked this frame; `ui()` acts on them afterwards with
    /// a clean `&mut self` rather than through a nested closure borrow.
    fn bottom_action_bar(&mut self, ui: &mut egui::Ui) -> (bool, bool, bool) {
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
                    if ui
                        .button("Stats")
                        .on_hover_text("View lifetime dictation words, time, and provider totals.")
                        .clicked()
                    {
                        self.modal = Some(Modal::Stats);
                    }
                    // Overflow menu (⋯): the less-used utilities that used to be a
                    // loose button row at the bottom of the settings body. Its
                    // body is `overflow_menu`, split out to keep this function's
                    // cognitive load down.
                    ui.menu_button(overflow_glyph(), |ui| self.overflow_menu(ui))
                        .response
                        .on_hover_text("More: check for updates, open logs, edit settings.json");

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (s, sr) = self.save_split_button(ui);
                        do_save = s;
                        do_save_restart = sr;
                    });
                });
            });
        (do_about, do_save, do_save_restart)
    }

    /// The ⋯ overflow menu's contents: check for updates, open log folder,
    /// edit settings.json, reset to defaults. Split out of
    /// `bottom_action_bar` purely to keep its cognitive load down.
    fn overflow_menu(&mut self, ui: &mut egui::Ui) {
        ui.set_min_width(170.0);
        if ui.button("Check for updates").clicked() {
            // The About window runs the check and shows the result.
            crate::about::show_about();
        }
        if ui.button("Open log folder").clicked() {
            open_log_folder();
        }
        if ui.button("Edit settings.json").clicked() {
            let path = Config::settings_path();
            self.note_editor_opened();
            let _ = std::process::Command::new("notepad.exe").arg(&path).spawn();
        }
        ui.separator();
        if ui
            .button("Default settings")
            .on_hover_text("Reset every setting back to its default. Your API keys are kept.")
            .clicked()
        {
            // A menu closes on any click (egui's default
            // `PopupCloseBehavior::CloseOnClick`), so there's no room for a
            // two-step confirm in place here -- open a small confirmation
            // modal instead, styled like the Stats modal's own "Reset
            // stats" confirm.
            self.modal = Some(Modal::DefaultReset);
        }
    }

    /// The right-aligned Save split-button: [ Save |▾ ], plus the save
    /// status label. Returns (do_save, do_save_restart). Split out of
    /// `bottom_action_bar` purely to keep its cognitive load down.
    fn save_split_button(&mut self, ui: &mut egui::Ui) -> (bool, bool) {
        let mut do_save = false;
        let mut do_save_restart = false;
        // Zero spacing + complementary corner rounding so Save and its
        // dropdown paint as one unified split button: [ Save |▾ ] with a
        // single shared outer rounding and a square seam where the two
        // segments meet. The arrow half reveals "Save and restart".
        ui.spacing_mut().item_spacing.x = 0.0;
        let arrow_round = CornerRadius {
            nw: 0,
            ne: ROUND,
            sw: 0,
            se: ROUND,
        };
        accent_menu_button(ui, chevron_down_glyph(), arrow_round, |ui| {
            ui.set_min_width(150.0);
            if ui.button("Save and restart").clicked() {
                do_save_restart = true;
            }
        })
        .on_hover_text("More save options");
        let save_round = CornerRadius {
            nw: ROUND,
            ne: 0,
            sw: ROUND,
            se: 0,
        };
        if accent_button_rounded(ui, "Save", save_round, egui::vec2(0.0, SPLIT_BTN_H)).clicked() {
            do_save = true;
        }
        // Save status fills the gap between the menu and Save. Restore
        // normal spacing here since the split button above needed 0.
        if !self.status.is_empty() {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.add_space(6.0);
            ui.label(RichText::new(self.status.clone()).color(muted()));
        }
        (do_save, do_save_restart)
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
    fn recorded_mouse_buttons_round_trip_through_the_parser() {
        // The bug this feature fixes: a mouse button pressed while a field was
        // recording produced nothing, because only `Key` events were read.
        // Every capturable button must now yield a combo the engine accepts.
        for (button, expected) in [
            (egui::PointerButton::Middle, "mouse3"),
            (egui::PointerButton::Extra1, "mouse4"),
            (egui::PointerButton::Extra2, "mouse5"),
        ] {
            let combo = combo_from_pointer(button, egui::Modifiers::default())
                .unwrap_or_else(|| panic!("{button:?} must be capturable"));
            assert_eq!(combo, expected);
            assert!(
                crate::hotkeys::parse_combo(&combo).is_ok(),
                "the capture UI and the hotkey engine must agree on {combo}"
            );
        }

        // Modifiers ride along exactly as they do for keys.
        let mods = egui::Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        let combo = combo_from_pointer(egui::PointerButton::Extra1, mods).unwrap();
        assert_eq!(combo, "ctrl+shift+mouse4");
        assert!(crate::hotkeys::parse_combo(&combo).is_ok());
    }

    #[test]
    fn left_and_right_click_are_never_captured() {
        // Two jobs at once: they are unsafe to bind (a bound button is a
        // suppressed button), and refusing Primary here is what stops the very
        // click that ARMS recording from being recorded as the binding.
        for button in [egui::PointerButton::Primary, egui::PointerButton::Secondary] {
            assert!(
                combo_from_pointer(button, egui::Modifiers::default()).is_none(),
                "{button:?} must not be capturable"
            );
            let mods = egui::Modifiers {
                ctrl: true,
                ..Default::default()
            };
            assert!(
                combo_from_pointer(button, mods).is_none(),
                "{button:?} must not become capturable just because a modifier is held"
            );
        }
    }

    #[test]
    fn a_mouse_bound_toggle_and_hold_are_a_reported_conflict() {
        // Two identical mouse bindings are as unusable as two identical
        // keyboard ones, and must be caught by the same check.
        assert!(hotkeys_conflict("mouse4", "mouse4"));
        assert!(hotkeys_conflict("mouse4", "x1")); // same button, different spelling
        assert!(!hotkeys_conflict("mouse4", "mouse5"));
        assert!(!hotkeys_conflict("mouse4", "f14"));
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

    #[test]
    fn bulk_keys_trim_skip_blanks_and_stably_dedupe() {
        let mut rows = vec![KeyRow {
            value: "existing-key".into(),
            verdict: Verdict::Ok,
        }];
        let summary = merge_key_lines(
            &mut rows,
            " new-key-a \r\n\r\nexisting-key\nnew-key-b\nnew-key-a\n",
        )
        .unwrap();
        assert_eq!(
            summary,
            KeyMergeSummary {
                added: 2,
                duplicates: 2,
            }
        );
        assert_eq!(
            deduped_key_values(&rows),
            vec!["existing-key", "new-key-a", "new-key-b"]
        );
        assert_eq!(rows[0].verdict, Verdict::Ok);
        assert_eq!(rows[1].verdict, Verdict::Untested);
    }

    #[test]
    fn bulk_key_validation_is_atomic_and_keys_are_case_sensitive() {
        let mut rows = vec![KeyRow {
            value: "KeyABC".into(),
            verdict: Verdict::Untested,
        }];
        let before = deduped_key_values(&rows);
        assert_eq!(
            merge_key_lines(&mut rows, "valid-key\nnot a key\nalso-valid"),
            Err(vec![2])
        );
        assert_eq!(deduped_key_values(&rows), before);

        let summary = merge_key_lines(&mut rows, "keyabc\nKeyABC").unwrap();
        assert_eq!(summary.added, 1);
        assert_eq!(summary.duplicates, 1);
        assert_eq!(deduped_key_values(&rows), vec!["KeyABC", "keyabc"]);
    }

    #[test]
    fn key_mask_never_displays_the_whole_secret() {
        assert_eq!(mask("abcd"), "\u{2022}\u{2022}\u{2022}\u{2022}");
        assert_eq!(mask("abcdef"), "ab\u{2026}ef");
        assert_eq!(mask("abcdefghijklmnop"), "abcdef\u{2026}klmnop");
    }

    #[test]
    fn stats_numbers_are_human_readable() {
        assert_eq!(grouped_number(0), "0");
        assert_eq!(grouped_number(1_234_567), "1,234,567");
        assert_eq!(format_audio_time(42_900), "42s");
        assert_eq!(format_audio_time(125_000), "2m 5s");
        assert_eq!(format_audio_time(7_500_000), "2h 5m");
    }

    // ---- Hotkey-conflict validation (change 4) -----------------------------

    #[test]
    fn hotkeys_conflict_flags_identical_combos_case_insensitively() {
        assert!(hotkeys_conflict("ctrl+shift+d", "Ctrl+Shift+D"));
        // Order of modifiers shouldn't matter either -- both normalize to the
        // same (modifiers, vk) pair.
        assert!(hotkeys_conflict("ctrl+shift+d", "shift+ctrl+d"));
        assert!(!hotkeys_conflict("f13", "f14"));
    }

    #[test]
    fn hotkeys_conflict_ignores_unparsable_combos() {
        // `validate` already surfaces the parse error for these on its own;
        // this helper only ever runs once both sides are known-good.
        assert!(!hotkeys_conflict("not a key", "f13"));
        assert!(!hotkeys_conflict("f13", "also not a key"));
    }

    // ---- Unsaved-changes dirty check (change 3) ----------------------------

    #[test]
    fn configs_differ_detects_a_changed_field_and_is_false_for_equal_configs() {
        let a = Config::default();
        let b = Config::default();
        assert!(!configs_differ(&a, &b));

        let mut c = Config::default();
        c.auto_punct = !c.auto_punct;
        assert!(configs_differ(&a, &c));
    }

    // ---- Custom vocabulary editor parsing (change 6) -----------------------

    #[test]
    fn parse_vocabulary_trims_and_drops_blank_lines() {
        let parsed = parse_vocabulary("  Supabase  \n\n\tCloudflare\n   \nGitHub\n");
        assert_eq!(
            parsed,
            vec![
                "Supabase".to_string(),
                "Cloudflare".to_string(),
                "GitHub".to_string(),
            ]
        );
        assert!(parse_vocabulary("\n   \n\t\n").is_empty());
    }

    // ---- History search filter (change 7) ----------------------------------

    #[test]
    fn history_matches_is_case_insensitive_and_empty_filter_matches_everything() {
        assert!(history_matches("", "anything at all"));
        assert!(history_matches("   ", "anything at all"));
        assert!(history_matches("hello", "Well, HELLO there"));
        assert!(history_matches("  Hello  ", "hello world")); // filter itself is trimmed
        assert!(!history_matches("zzz", "Well, HELLO there"));
    }

    #[test]
    fn truncate_preview_keeps_short_text_and_ellipsizes_long_text() {
        assert_eq!(truncate_preview("short", 10), "short");
        assert_eq!(truncate_preview("exactly ten", 11), "exactly ten");
        assert_eq!(truncate_preview("this is too long", 7), "this is\u{2026}");
    }

    // ---- History card cache invalidation (adversarial-review fix 3) -------

    #[test]
    fn history_cache_stale_is_false_when_neither_version_nor_filter_moved() {
        assert!(!history_cache_stale(3, 3, "hello", "hello"));
        assert!(!history_cache_stale(0, 0, "", ""));
    }

    #[test]
    fn history_cache_stale_detects_a_version_bump_with_the_filter_unchanged() {
        assert!(history_cache_stale(3, 4, "hello", "hello"));
    }

    #[test]
    fn history_cache_stale_detects_a_filter_edit_with_the_version_unchanged() {
        assert!(history_cache_stale(3, 3, "hello", "hell"));
    }

    #[test]
    fn history_cache_stale_detects_both_moving_at_once() {
        assert!(history_cache_stale(3, 5, "hello", "world"));
    }
}
