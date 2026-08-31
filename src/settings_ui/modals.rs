//! The Settings window's modal dialogs: bulk key and replacement
//! editors, the confirm prompts, and the frame they share.

use super::keys_modal::render_keys_modal;
use super::replacements_modal::render_replacements_modal;
use super::stats_modal::render_stats_modal;
use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ModalAction {
    #[default]
    None,
    Commit,
    CommitAndSave,
    Cancel,
}

/// Everything a modal-render arm can decide to do, bundled into one struct
/// so each `render_*_modal` function below takes one output parameter
/// instead of eight. `action` and `test_request` are read back the same
/// frame; the `do_*` flags are deferred side effects applied once
/// `render_modal`'s match on `self.modal` has released its borrow.
#[derive(Default)]
pub(super) struct ModalOutcome {
    pub(super) action: ModalAction,
    pub(super) test_request: Option<Vec<String>>,
    pub(super) reset_stats: bool,
    pub(super) do_default_reset: bool,
    pub(super) do_close_discard: bool,
    pub(super) do_close_save: bool,
    pub(super) do_reload_from_disk: bool,
    pub(super) do_overwrite: bool,
}

/// The "Default settings" confirm-before-destroy modal.
fn render_default_reset_modal(ctx: &egui::Context, out: &mut ModalOutcome) {
    let backdrop = SettingsApp::modal_frame(ctx, "Reset all settings?", 380.0, |ui| {
        egui::Frame::new()
            .fill(bad().gamma_multiply(0.09))
            .stroke(Stroke::new(1.0, bad().gamma_multiply(0.45)))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(
                        "This resets every setting back to its default \u{2014} \
                         provider, hotkeys, replacements, and profiles. Your API \
                         keys are kept. This cannot be undone.",
                    )
                    .size(11.5)
                    .color(text()),
                );
            });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui
                .button(RichText::new("Reset").font(semibold(11.5)).color(bad()))
                .clicked()
            {
                out.do_default_reset = true;
                out.action = ModalAction::Cancel;
            }
            if ui.button("Cancel").clicked() {
                out.action = ModalAction::Cancel;
            }
        });
    });
    if backdrop || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        out.action = ModalAction::Cancel;
    }
}

/// The "you have unsaved changes" close-confirm modal.
fn render_unsaved_changes_modal(ctx: &egui::Context, out: &mut ModalOutcome) {
    let backdrop = SettingsApp::modal_frame(ctx, "Unsaved changes", 380.0, |ui| {
        ui.label(
            RichText::new(
                "You have unsaved changes. Save them before closing, or discard \
                 them?",
            )
            .size(12.5)
            .color(text()),
        );
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if accent_button(ui, "Save").clicked() {
                out.do_close_save = true;
            }
            if ui.button(RichText::new("Discard").color(bad())).clicked() {
                out.do_close_discard = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    out.action = ModalAction::Cancel;
                }
            });
        });
    });
    // Escape/backdrop = Cancel (keep editing), never Discard — an
    // accidental dismiss must never be the thing that throws away edits.
    if backdrop || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        out.action = ModalAction::Cancel;
    }
}

/// The "settings.json changed on disk" reload-vs-overwrite modal.
fn render_external_change_modal(ctx: &egui::Context, out: &mut ModalOutcome) {
    let backdrop = SettingsApp::modal_frame(ctx, "settings.json changed on disk", 420.0, |ui| {
        ui.label(
            RichText::new(
                "settings.json changed on disk since you opened \u{201c}Edit \
                 settings.json\u{2026}\u{201d} \u{2014} probably your own hand-edit. \
                 Reload it here (discarding the edits you've made in this window), \
                 or overwrite it with what's in this window?",
            )
            .size(12.5)
            .color(text()),
        );
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if accent_button(ui, "Reload").clicked() {
                out.do_reload_from_disk = true;
            }
            if ui.button(RichText::new("Overwrite").color(bad())).clicked() {
                out.do_overwrite = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    out.action = ModalAction::Cancel;
                }
            });
        });
    });
    if backdrop || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        out.action = ModalAction::Cancel;
    }
}

impl super::SettingsApp {
    /// Open the key manager over one key pool: a provider id,
    /// [`KEYS_TARGET_PROVIDER`] for whichever STT provider is selected, or
    /// [`KEYS_TARGET_POLISH`] for the AI cleanup pass's own keys.
    pub(crate) fn open_keys_modal(&mut self, target: &str) {
        self.keys_target = target.to_string();
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
        self.modal = Some(Modal::Keys(KeysModalState {
            rows,
            add_text: String::new(),
            bulk: false,
            bulk_text: String::new(),
            bulk_note: String::new(),
            bulk_error: false,
        }));
    }
    pub(crate) fn open_replacements_modal(&mut self) {
        let rows = self
            .draft
            .text_replacements
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.modal = Some(Modal::Replacements(ReplacementsModalState {
            rows,
            add_from: String::new(),
            add_to: String::new(),
            bulk: false,
            bulk_text: String::new(),
        }));
    }
    /// Centered modal card (native `egui::Modal` — handles the dim backdrop,
    /// centering, and Escape/backdrop-to-close). Returns true when the user
    /// dismissed it via backdrop/Escape so the caller can treat that as Cancel.
    pub(crate) fn modal_frame(
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
    pub(crate) fn render_modal(&mut self, ctx: &egui::Context) {
        let stats_snapshot = self.app.stats.snapshot();
        let Some(modal) = &mut self.modal else {
            return;
        };
        let mut out = ModalOutcome::default();
        let (mut stats_range, mut stats_reset_confirm) =
            (self.stats_range, self.stats_reset_confirm);

        match modal {
            Modal::Stats => {
                (stats_range, stats_reset_confirm) = render_stats_modal(
                    ctx,
                    &stats_snapshot,
                    stats_range,
                    stats_reset_confirm,
                    &mut out,
                );
            }
            Modal::DefaultReset => render_default_reset_modal(ctx, &mut out),
            Modal::UnsavedChanges => render_unsaved_changes_modal(ctx, &mut out),
            Modal::ExternalChange => render_external_change_modal(ctx, &mut out),
            Modal::Keys(state) => render_keys_modal(
                ctx,
                &self.keys_target,
                &self.draft.stt_provider,
                state,
                &mut out,
            ),
            Modal::Replacements(state) => render_replacements_modal(ctx, state, &mut out),
        }

        self.stats_range = stats_range;
        self.stats_reset_confirm = stats_reset_confirm;
        if out.reset_stats {
            self.app.stats.reset();
            crate::sync::schedule_stats_push(Arc::clone(&self.app));
        }

        if let Some(keys) = out.test_request.take() {
            self.start_key_test(ctx, keys);
        }

        let save_after_commit = matches!(out.action, ModalAction::CommitAndSave);
        self.apply_modal_action(out.action);
        if save_after_commit && !self.save_and_sync(ctx) {
            // Keep the imported keys editable if validation or disk I/O made
            // the requested save fail. The draft already contains them.
            self.open_keys_modal(KEYS_TARGET_PROVIDER);
        }

        // ---- Confirmations that were armed above, applied now that the
        // `self.modal` borrow from the `match modal { .. }` above has ended.
        self.apply_deferred_modal_effects(ctx, &out);
    }

    /// Apply a committed/cancelled modal's action back into the settings
    /// draft. Runs after the `match modal { .. }` above has released its
    /// borrow of `self.modal`.
    fn apply_modal_action(&mut self, action: ModalAction) {
        match action {
            ModalAction::Commit | ModalAction::CommitAndSave => match self.modal.take() {
                Some(Modal::Keys(state)) => {
                    let id = self.keys_target.clone();
                    *keys_of(&mut self.draft, &id) = deduped_key_values(&state.rows);
                }
                Some(Modal::Replacements(state)) => {
                    // If the user left it in text-editor mode, parse that.
                    let final_rows = if state.bulk {
                        text_to_replacements(&state.bulk_text)
                    } else {
                        state.rows
                    };
                    self.draft.text_replacements = final_rows
                        .into_iter()
                        .filter(|(f, _)| !f.trim().is_empty())
                        .collect();
                }
                Some(Modal::Stats)
                | Some(Modal::DefaultReset)
                | Some(Modal::UnsavedChanges)
                | Some(Modal::ExternalChange) => {}
                None => {}
            },
            ModalAction::Cancel => {
                self.modal = None;
                self.stats_reset_confirm = false;
                // Backing out of the settings.json-changed prompt should not
                // leave a stale "resume this save" note behind.
                self.pending_save_kind = None;
            }
            ModalAction::None => {}
        }
    }

    /// The `do_*` deferred effects a modal render can arm: default reset,
    /// closing the window (with or without saving), and the settings.json-
    /// changed prompt's Reload/Overwrite.
    fn apply_deferred_modal_effects(&mut self, ctx: &egui::Context, out: &ModalOutcome) {
        if out.do_default_reset {
            self.reset_to_defaults();
        }
        if out.do_close_discard {
            self.modal = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            OPEN.store(false, Ordering::Release);
        }
        if out.do_close_save {
            if self.external_change_pending() {
                // A hand-edit landed on disk while this dialog was open too;
                // surface that prompt instead of clobbering it. The window
                // stays open — the user can retry Save (or the X) once it's
                // resolved.
                self.pending_save_kind = Some(PendingSaveKind::Plain);
                self.modal = Some(Modal::ExternalChange);
            } else {
                self.modal = None;
                if self.save_and_sync(ctx) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    OPEN.store(false, Ordering::Release);
                }
                // On failure (e.g. a hotkey conflict) `self.status` already
                // carries the reason — matches the ordinary Save button.
            }
        }
        if out.do_reload_from_disk {
            self.reload_settings_from_disk();
        }
        if out.do_overwrite {
            self.modal = None;
            self.editor_opened_at = None;
            match self.pending_save_kind.take() {
                Some(PendingSaveKind::Restart) => self.save_and_restart(ctx),
                _ => {
                    self.save_and_sync(ctx);
                }
            }
        }
    }

    /// `do_reload_from_disk`: discard the in-window draft and reload
    /// settings.json, replaying its own diagnostics through tracing the same
    /// way `main.rs` replays `startup_diags`.
    fn reload_settings_from_disk(&mut self) {
        self.modal = None;
        self.editor_opened_at = None;
        self.pending_save_kind = None;
        let path = Config::settings_path();
        if path.exists() {
            let (cfg, diags) = Config::load_or_create();
            // Same severity-prefix convention `main.rs` replays
            // `startup_diags` with (see there) — kept lightweight here
            // since the Reload outcome is already visible in
            // `self.status` right below.
            for d in &diags {
                if let Some(rest) = d.strip_prefix("WARN: ") {
                    tracing::warn!("{rest}");
                } else if let Some(rest) = d
                    .strip_prefix("ERROR: ")
                    .or_else(|| d.strip_prefix("ALERT: "))
                {
                    tracing::error!("{rest}");
                } else {
                    tracing::info!("{d}");
                }
            }
            self.draft = cfg;
            self.resync_vocabulary_scratch();
            self.status = "Reloaded settings.json from disk.".into();
        } else {
            self.status = "settings.json is missing \u{2014} keeping your edits here.".into();
        }
    }
}
