//! The speech-to-text provider card: choosing a provider, its API keys, and
//! the local model packs that need installing first.

use super::*;

/// The right-aligned controls for one local-model row in
/// [`SettingsApp::local_model_section`] — install / cancel / delete, plus
/// whatever progress text that phase shows. A free function, not a method:
/// it only ever needs to report an error back through `status`, not the rest
/// of `SettingsApp`. Splits on whether the phase is "idle, waiting for a
/// click" or "busy, showing progress", since those two halves share no logic.
fn local_model_status_controls(
    status: &mut String,
    ui: &mut egui::Ui,
    spec: &crate::local_stt::ModelSpec,
    snapshot: &crate::local_stt::InstallSnapshot,
) {
    use crate::local_stt::InstallPhase;
    match &snapshot.phase {
        InstallPhase::Installed | InstallPhase::NotInstalled | InstallPhase::Failed(_) => {
            render_model_action_button(status, ui, spec, &snapshot.phase);
        }
        _ => render_model_progress_controls(status, ui, spec, snapshot),
    }
}

/// The idle half of [`local_model_status_controls`]: a single clickable
/// button — Delete once installed, Install otherwise (covers both
/// "never installed" and "install failed, so let them retry").
fn render_model_action_button(
    status: &mut String,
    ui: &mut egui::Ui,
    spec: &crate::local_stt::ModelSpec,
    phase: &crate::local_stt::InstallPhase,
) {
    if matches!(phase, crate::local_stt::InstallPhase::Installed) {
        if ui
            .button(delete_glyph())
            .on_hover_text(
                "Delete this downloaded model from this PC. You can install it again later.",
            )
            .clicked()
        {
            if let Err(e) = crate::local_stt::start_remove(spec.id) {
                *status = e;
            }
        }
    } else if accent_button(ui, "Install").clicked() {
        if let Err(e) = crate::local_stt::start_install(spec.id) {
            *status = e;
        }
    }
}

/// The busy half of [`local_model_status_controls`]: a spinner plus whatever
/// progress text or Cancel button that phase shows.
fn render_model_progress_controls(
    status: &mut String,
    ui: &mut egui::Ui,
    spec: &crate::local_stt::ModelSpec,
    snapshot: &crate::local_stt::InstallSnapshot,
) {
    use crate::local_stt::InstallPhase;
    match &snapshot.phase {
        InstallPhase::DownloadingRuntime | InstallPhase::DownloadingModel => {
            if ui.button("Cancel").clicked() {
                if let Err(e) = crate::local_stt::cancel_install(spec.id) {
                    *status = e;
                }
            }
            let pct = snapshot
                .downloaded
                .saturating_mul(100)
                .checked_div(snapshot.total)
                .unwrap_or(0);
            ui.label(RichText::new(format!("{pct}%")).size(12.0).color(muted()));
        }
        InstallPhase::InstallingRuntime | InstallPhase::VerifyingDownload => {
            if ui.button("Cancel").clicked() {
                if let Err(e) = crate::local_stt::cancel_install(spec.id) {
                    *status = e;
                }
            }
            let label = if matches!(snapshot.phase, InstallPhase::VerifyingDownload) {
                "verifying\u{2026}"
            } else {
                "installing runtime\u{2026}"
            };
            ui.label(RichText::new(label).size(12.0).color(muted()));
        }
        InstallPhase::Cancelling => {
            ui.label(
                RichText::new("cancelling\u{2026}")
                    .size(12.0)
                    .color(muted()),
            );
        }
        InstallPhase::Removing => {
            ui.label(RichText::new("removing\u{2026}").size(12.0).color(muted()));
        }
        InstallPhase::Installed | InstallPhase::NotInstalled | InstallPhase::Failed(_) => {
            // Handled by `render_model_action_button`; unreachable here.
        }
    }
    ui.add(egui::Spinner::new().size(14.0));
}

impl super::SettingsApp {
    pub(crate) fn provider_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, testing: bool) {
        card(ui, |ui| {
            section_title(ui, "\u{E720}", "Speech-to-text provider");
            // Dropdown + key actions all on one row (saves a whole row).
            self.provider_select_row(ui, ctx, testing);

            ui.add_space(6.0);
            blue_check(
                ui,
                &mut self.draft.protect_keys_at_rest,
                "Encrypt API keys in settings.json (this PC and Windows account only)",
            )
            .on_hover_text(
                "Seals your API keys with Windows DPAPI so settings.json only decrypts on this \
                 exact Windows account and machine. If you copy this portable folder to another \
                 PC or another Windows user, the sealed keys will NOT work there \u{2014} you'll \
                 need to paste them in again.",
            );

            if self.draft.stt_provider == "local" {
                self.local_model_section(ui, ctx);
            }

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
            self.key_test_status_row(ui, testing);
        });
    }

    /// The provider dropdown plus its "Manage keys…" / "Test all keys"
    /// actions, hidden for the local (offline) provider since it has no API
    /// keys.
    fn provider_select_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, testing: bool) {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("provider")
                .width(200.0)
                .selected_text(provider_label(&self.draft.stt_provider))
                .show_ui(ui, |ui| {
                    for (id, label) in providers() {
                        if ui
                            .selectable_value(&mut self.draft.stt_provider, id.to_string(), label)
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
            if self.draft.stt_provider != "local" {
                if accent_button(ui, "Manage keys\u{2026}")
                    .on_hover_text("Add, remove, or paste API keys for the selected provider.")
                    .clicked()
                {
                    self.open_keys_modal(KEYS_TARGET_PROVIDER);
                }
                if ui
                    .add_enabled(!testing, egui::Button::new("Test all keys"))
                    .on_hover_text("Check every saved key for this provider against its live API.")
                    .clicked()
                {
                    let keys = self.active_keys();
                    self.start_key_test(ctx, keys);
                }
            }
        });
    }

    /// The Local-provider block: the offline explainer, the active-model
    /// picker, and one row per installable model with its install/cancel/
    /// delete controls.
    fn local_model_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Runs fully offline after installation. Models are stored in Local AppData, \
                 not in QuickDictate or this repository. The selected model stays warmed in \
                 memory while Local is active; switching providers releases it.",
            )
            .size(12.0)
            .color(muted()),
        );
        ui.add_space(7.0);
        ui.horizontal(|ui| {
            ui.label("Active model");
            egui::ComboBox::from_id_salt("local_model")
                .width(260.0)
                .selected_text(
                    crate::local_stt::model(&self.draft.local_model)
                        .map(|m| m.label)
                        .unwrap_or("Unknown"),
                )
                .show_ui(ui, |ui| {
                    for spec in crate::local_stt::MODELS {
                        ui.selectable_value(
                            &mut self.draft.local_model,
                            spec.id.to_string(),
                            spec.label,
                        )
                        .on_hover_text(spec.detail);
                    }
                });
        });
        ui.add_space(7.0);
        for spec in crate::local_stt::MODELS {
            let snapshot = crate::local_stt::install_snapshot(spec.id);
            if snapshot.busy() {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            ui.horizontal(|ui| {
                let selected = self.draft.local_model == spec.id;
                if selected {
                    chip(ui, "selected", accent());
                } else if ui.small_button("Use").clicked() {
                    self.draft.local_model = spec.id.to_string();
                }
                ui.vertical(|ui| {
                    ui.label(RichText::new(spec.label).color(text()));
                    ui.label(RichText::new(spec.detail).size(11.5).color(muted()));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    local_model_status_controls(&mut self.status, ui, &spec, &snapshot);
                });
            });
            if let crate::local_stt::InstallPhase::Failed(message) = &snapshot.phase {
                ui.label(
                    RichText::new(format!("Install problem: {message}"))
                        .size(11.5)
                        .color(bad()),
                );
            }
            ui.add_space(4.0);
        }
    }

    /// The trailing "N working / N failing / testing…" chip row, shown once
    /// there's something to report.
    fn key_test_status_row(&self, ui: &mut egui::Ui, testing: bool) {
        let ok_count = self.verdicts.iter().filter(|(_, ok)| *ok).count();
        let fail_count = self.verdicts.iter().filter(|(_, ok)| !*ok).count();
        if ok_count == 0 && fail_count == 0 && !testing {
            return;
        }
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
}
