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
//! This file is the hub: the window plumbing ([`show_settings`]), the tooltip
//! copy the cards share, and the bottom bar. Everything else lives in a
//! sibling:
//!
//! - [`app`]: [`SettingsApp`] and the state it holds, plus the per-frame loop.
//! - [`style`]: palette, fonts, glyphs, and the egui style.
//! - [`widgets`]: reusable controls, cards, and the usage-stats charts.
//! - [`logic`]: construction, validation, saving, sync, hotkey capture.
//! - [`cards`]: the provider card, its keys, and the local model packs.
//! - [`banners`]: onboarding, available update, sign-in nudge.
//! - [`dictation`]: hotkey capture, language, mode, the listen tail.
//! - [`application`]: data folder, behaviour toggles, Per-App Profiles.
//! - [`history_sync`]: the transcript-history browser and the sync card.
//! - [`modals`]: the confirm prompts, the shared frame, and modal dispatch.
//! - [`keys_modal`] / [`replacements_modal`] / [`stats_modal`]: one each.
//! - [`keys`]: the provider list and the bulk key editor's parsing.
//! - [`combo`]: egui events to hotkey combo strings, and conflict detection.
//! - [`mod@text`]: pure transforms over the settings the window edits.
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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};

use eframe::egui::containers::menu;
use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};

use crate::config::Config;
use crate::state::App;
use crate::stats::StatsRange;
use crate::theme;

// Split out of this file so each surface can be reviewed on its own; the
// hub keeps the shared state, the window plumbing, and the frame loop.
mod app;
mod application;
mod banners;
mod cards;
mod combo;
mod dictation;
mod history_sync;
mod keys;
mod keys_modal;
mod logic;
mod modals;
mod nav;
mod replacements_modal;
mod stats_modal;
mod style;
mod text;
mod widgets;

#[cfg(test)]
mod tests;

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

use app::*;
use combo::*;
use keys::*;
use text::*;

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
     NOT work here; they are separate APIs.\n\nRecommended model: gemini-flash-lite-latest. \
     Measured at ~0.56 s with the best results of everything tested — about 3x faster than \
     GPT-4.1-mini, and faster than the bigger Gemini models, which think before answering and \
     lose the race for no benefit.";

/// Reveal the dedicated diagnostics directory in Explorer.
fn open_log_folder() {
    let dir = crate::logging::logs_dir();
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
