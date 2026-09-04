//! The window's own state: the modal it has open, the sync card's phase, the
//! history cache, and `SettingsApp` itself, plus the per-frame `eframe::App`
//! loop that drives them.

use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};

use eframe::egui::{self, Margin};

use crate::config::Config;
use crate::state::{App, HistoryEntry};
use crate::stats::StatsRange;

// Split out of this file so each surface can be reviewed on its own; the

use super::*;

/// Which hotkey field a "Record" button is currently listening for.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum HotkeyField {
    Toggle,
    Hold,
}

/// The key-manager modal's own scratch state, bundled into one struct so the
/// render function that owns it takes one parameter instead of six.
pub(super) struct KeysModalState {
    pub(super) rows: Vec<KeyRow>,
    pub(super) add_text: String,
    pub(super) bulk: bool,
    pub(super) bulk_text: String,
    pub(super) bulk_note: String,
    pub(super) bulk_error: bool,
}

/// The text-replacements modal's own scratch state.
pub(super) struct ReplacementsModalState {
    pub(super) rows: Vec<(String, String)>,
    pub(super) add_from: String,
    pub(super) add_to: String,
    /// Bulk "text editor" mode: edit all replacements as `from = to` lines
    /// so a big set can be pasted/copied at once.
    pub(super) bulk: bool,
    pub(super) bulk_text: String,
}

pub(super) enum Modal {
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

// ---- Connections settings-sync UI state ------------------------------------

/// Visible state of the opt-in "Sync settings with Connections" control.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum SyncPhase {
    /// No creds on disk — show the opt-in button.
    SignedOut,
    /// Interactive sign-in underway (browser round-trip).
    SigningIn,
    /// Creds present — synced (a background pull/push may still be in flight).
    SignedIn,
}

/// Results streamed back from a sync worker thread, drained each frame.
pub(super) enum SyncEvent {
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
pub(super) struct SyncUi {
    pub(super) phase: SyncPhase,
    pub(super) email: String,
    /// Display name from /oauth/userinfo, shown next to the status note. Empty for creds saved
    /// before we fetched it (backfilled on the next silent resume) → the UI then just omits it.
    pub(super) name: String,
    /// Avatar texture (uploaded on the UI thread from decoded bytes a sync worker returns). `None`
    /// until a resume/sign-in resolves the profile picture, or if there is none.
    pub(super) avatar: Option<egui::TextureHandle>,
    /// One-line status/error under the control.
    pub(super) note: String,
    pub(super) is_error: bool,
    /// Receiver for the currently in-flight worker (if any).
    pub(super) rx: Option<mpsc::Receiver<SyncEvent>>,
    /// Fire the silent resume-pull exactly once, on the first frame.
    pub(super) resume_kicked: bool,
}

/// Cached, pre-filtered snapshot backing `history_card`, so a frame that
/// changes neither the history nor the filter text doesn't re-lock
/// `app.history` and re-run `history_matches` over every entry from scratch.
/// Rebuilt exactly when [`history_cache_stale`] says `version` or `filter`
/// moved since the last build.
#[derive(Default)]
pub(super) struct HistoryCache {
    /// `TranscriptHistory::version()` as of the last rebuild.
    pub(super) version: u64,
    /// `history_filter` as of the last rebuild.
    pub(super) filter: String,
    /// Whether the *unfiltered* history was empty as of the last rebuild —
    /// cached separately from `rows` so the "no dictations yet" vs. "no
    /// matches" messages in `history_card` stay distinguishable even though
    /// only the filtered rows are kept around.
    pub(super) history_empty: bool,
    /// `(original index into the live history, cloned entry)` for every entry
    /// matching `filter`, newest first — the original index is what "Copy" /
    /// "Paste again" need to look the entry back up in `app.history`.
    pub(super) rows: Vec<(usize, HistoryEntry)>,
}

/// A "Save and restart" that saved locally and kicked off a best-effort sync
/// push, waiting for that push to land (or time out) before actually
/// relaunching. See `SettingsApp::save_and_restart` / `poll_pending_restart`.
pub(super) struct PendingRestart {
    /// Give the push at most this long — a dead network must never hold the
    /// restart hostage indefinitely.
    pub(super) deadline: std::time::Instant,
}

/// Which action a pre-save "settings.json changed on disk" prompt
/// (`Modal::ExternalChange`) should resume once the user picks Overwrite.
#[derive(Clone, Copy)]
pub(super) enum PendingSaveKind {
    Plain,
    Restart,
}

pub(super) struct SettingsApp {
    pub(super) app: Arc<App>,
    pub(super) draft: Config,
    pub(super) modal: Option<Modal>,
    /// Which hotkey field (if any) is currently recording a keypress.
    pub(super) recording: Option<HotkeyField>,
    /// Latest per-key verdicts for the active provider (fed by parallel tests).
    pub(super) verdicts: Vec<(String, bool)>,
    pub(super) test_rx: Option<mpsc::Receiver<(String, bool)>>,
    pub(super) testing_left: usize,
    pub(super) status: String,
    /// The currently-open error-report preview text (editable), or `None`
    /// when no report is being reviewed. Building the report never writes
    /// anything to disk; only "Save to file..." does. See
    /// `application::error_report_section`.
    pub(super) error_report_preview: Option<String>,
    /// Connections settings-sync control state.
    pub(super) sync: SyncUi,
    pub(super) stats_range: StatsRange,
    pub(super) stats_reset_confirm: bool,
    /// Scratch buffer for the global custom-vocabulary multiline editor —
    /// mirrors `draft.custom_vocabulary` as raw text (one term per line) so
    /// blank lines can exist mid-edit without being swallowed; only parsed
    /// back into `draft` on Save (see `parse_vocabulary`, `save`).
    pub(super) vocabulary_text: String,
    /// Same idea as `vocabulary_text`, one scratch buffer per entry of
    /// `draft.profiles` (same order). Kept in lockstep with `draft.profiles`
    /// by `resync_vocabulary_scratch`.
    pub(super) profile_vocab_text: Vec<String>,
    /// Case-insensitive substring filter for the History section.
    pub(super) history_filter: String,
    /// Cached, pre-filtered rows for `history_card`; see [`HistoryCache`].
    pub(super) history_cache: HistoryCache,
    /// settings.json's mtime when "Edit settings.json…" was last opened, so a
    /// later Save can tell a hand-edit landed on disk in the meantime. `None`
    /// when no editor session is being tracked (the common case).
    pub(super) editor_opened_at: Option<std::time::SystemTime>,
    /// Set when `Modal::ExternalChange` is showing, so its Overwrite button
    /// knows whether to resume a plain Save or a Save & Restart.
    pub(super) pending_save_kind: Option<PendingSaveKind>,
    /// Set by `save_and_restart` while its background sync push is in
    /// flight; polled by `poll_pending_restart`.
    pub(super) pending_restart: Option<PendingRestart>,
    // -- headless screenshot hook (QUICKDICTATE_UI_SHOT) --
    pub(super) shot_path: Option<String>,
    pub(super) frames: u32,
    pub(super) shot_requested: bool,
    /// Last window inner height (logical pts) we requested via the auto-fit in
    /// `ui`. The window is sized to its content each frame so it can never
    /// scroll and is never taller than needed; this cache gates the resize so we
    /// only issue a viewport command when the content height actually changes
    /// (winit applies `InnerSize` a frame late, so resending every frame would
    /// Which page the nav rail is showing. Kept across a hide/reveal so
    /// reopening Settings lands where you left off.
    pub(super) tab: nav::Tab,
    /// Which key pool the key manager edits: a provider id,
    /// [`KEYS_TARGET_PROVIDER`], or [`KEYS_TARGET_POLISH`]. Set by
    /// [`SettingsApp::open_keys_modal`] and read by `active_keys` and the
    /// modal's commit, so one editor serves both pools.
    pub(super) keys_target: String,
    /// The "you could be signed in" ask, when one is currently on screen.
    ///
    /// Held here rather than re-asked per frame on purpose: [`crate::nudge::consider`] MUTATES —
    /// it stamps the ask and advances the ladder — so calling it from a paint function would burn
    /// the user's three lifetime asks in three frames. It is called once, at the moment (a save),
    /// and what it returns lives here until the user answers it.
    pub(super) nudge_ask: Option<crate::nudge_engine::Ask>,
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
