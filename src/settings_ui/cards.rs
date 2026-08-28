//! The main Settings cards: onboarding, provider and keys, dictation,
//! and application behavior.

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

/// One editable profile row inside `SettingsApp::active_profiles_section`:
/// the name + match-list header, the language/provider override row, and the
/// optional custom-vocabulary override. A free function, not a method: it
/// only ever touches the one profile and vocab buffer it's given, not the
/// rest of `SettingsApp`.
fn profile_editor_row(
    ui: &mut egui::Ui,
    idx: usize,
    p: &mut crate::config::Profile,
    vocab_buf: &mut String,
) {
    egui::Frame::new()
        .fill(input_bg())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&p.name).font(semibold(13.0)).color(text()));
                ui.label(RichText::new(p.match_.join(", ")).size(11.5).color(muted()));
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Language").on_hover_text(
                    "Recognition language for this app. Leave blank to use the \
                     global language.",
                );
                let mut lang_buf = p.language.clone().unwrap_or_default();
                if ui
                    .add(
                        styled_input(&mut lang_buf)
                            .hint_text("Use global")
                            .desired_width(90.0),
                    )
                    .changed()
                {
                    p.language = (!lang_buf.trim().is_empty()).then_some(lang_buf);
                }
                ui.add_space(8.0);
                ui.label("Provider");
                egui::ComboBox::from_id_salt(("profile_provider", idx))
                    .width(150.0)
                    .selected_text(
                        p.stt_provider
                            .as_deref()
                            .map(provider_label)
                            .unwrap_or("Use global"),
                    )
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(p.stt_provider.is_none(), "Use global")
                            .clicked()
                        {
                            p.stt_provider = None;
                        }
                        for (id, label) in providers() {
                            let selected = p.stt_provider.as_deref() == Some(id);
                            if ui.selectable_label(selected, label).clicked() {
                                p.stt_provider = Some(id.to_string());
                            }
                        }
                    });
            });
            ui.add_space(4.0);
            let mut override_vocab = p.custom_vocabulary.is_some();
            if blue_check(ui, &mut override_vocab, "Override vocabulary for this app")
                .on_hover_text(
                    "Unchecked: use the global custom vocabulary. Checked with \
                     an empty list: no vocabulary biasing at all in this app.",
                )
                .changed()
            {
                p.custom_vocabulary = if override_vocab {
                    Some(parse_vocabulary(vocab_buf))
                } else {
                    None
                };
            }
            if p.custom_vocabulary.is_some() {
                ui.add(
                    egui::TextEdit::multiline(vocab_buf)
                        .desired_width(f32::INFINITY)
                        .desired_rows(2)
                        .margin(Margin::symmetric(6, CTRL_PAD))
                        .hint_text("One term per line"),
                );
            }
        });
}

impl super::SettingsApp {
    /// First-run onboarding banner, pinned above the provider card while *no*
    /// provider has any key. QuickDictate is unusable until a key is added, so
    /// when we auto-open Settings at launch (see `main`) this makes the very
    /// first action obvious instead of leaving the user to guess. It reads the
    /// live draft, so it vanishes the instant a key is saved into any provider.
    pub(crate) fn onboarding_banner(&mut self, ui: &mut egui::Ui) {
        if self.draft.stt_provider.eq_ignore_ascii_case("local")
            || !self.draft.providers_with_keys().is_empty()
        {
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
                    self.open_keys_modal(KEYS_TARGET_PROVIDER);
                }
            });
        ui.add_space(10.0);
    }
    /// A newer release the daily auto-check found but hasn't installed (see
    /// `update::pending_update`) — surfaced here too, not just the tray
    /// tooltip, since Settings is where most people go looking. Installing
    /// itself still only happens from the About window's pill, matching the
    /// click-to-consent model everywhere else in the app.
    pub(crate) fn update_available_banner(&mut self, ui: &mut egui::Ui) {
        let Some(tag) = crate::update::pending_update() else {
            return;
        };
        egui::Frame::new()
            .fill(good().gamma_multiply(0.14))
            .stroke(Stroke::new(1.0, good().gamma_multiply(0.5)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("Update available: v{tag}"))
                            .font(semibold(14.0))
                            .color(text()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if accent_button(ui, "Review\u{2026}").clicked() {
                            crate::about::show_about();
                        }
                    });
                });
            });
        ui.add_space(10.0);
    }
    /// The "you could be signed in" banner.
    ///
    /// Sits with the other two banners — above the page header, outside the scroll area — because
    /// it is true regardless of which page you are on, and because the thing it points at (the
    /// Settings sync card) is the LAST card on the Application page and therefore below the fold
    /// on a default window. That gap is the entire reason this exists: the offer is already in the
    /// app, and almost nobody scrolls far enough to find out.
    ///
    /// Three deliberate restraints, all of which the shared engine enforces and this only renders:
    ///
    ///   * **It is not a modal.** No overlay, no focus steal, no Escape to trap. It is a strip at
    ///     the top of a window the user opened on purpose, and everything behind it stays usable.
    ///   * **"Never" is offered on the first ask**, not withheld until the third. An opt-out you
    ///     have to earn is not an opt-out.
    ///   * **Nothing here asks for money.** The account is free and QuickDictate already signs
    ///     into it; the whole pitch is that it exists.
    pub(crate) fn sign_in_nudge_banner(&mut self, ui: &mut egui::Ui) {
        let Some(ask) = self.nudge_ask.clone() else {
            return;
        };
        // Answer collected inside the closure and acted on after it, so the borrow of `self` that
        // the frame holds is already released when we mutate `nudge_ask` and touch the engine.
        let mut answer: Option<crate::nudge_engine::Outcome> = None;
        let mut connect = false;

        egui::Frame::new()
            .fill(accent().gamma_multiply(0.12))
            .stroke(Stroke::new(1.0, accent().gamma_multiply(0.45)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // Text on its own rows, buttons on a row of their own beneath.
                //
                // The obvious layout — text left, buttons right, one row — does not survive
                // contact: four controls need roughly 300pt, the copy is a full sentence, and
                // egui's horizontal layout does not reserve space for what comes after, so the
                // body simply runs underneath the buttons. Reserving a fixed width for them only
                // moves the failure to whichever window size the guess is wrong at. Stacking is
                // correct at every width, which matters here because this window is resizable and
                // auto-fits its content.
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(&ask.headline)
                                .font(semibold(14.0))
                                .color(text()),
                        );
                        // The × belongs in the corner, not in the button row. Beside "Not now" it
                        // reads as a fourth choice, when it is really the same "no" the whole
                        // strip can be closed with — and the two mean different things to the
                        // engine (see the comment on the buttons below).
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button(RichText::new("\u{00D7}").size(14.0).color(muted()))
                                .on_hover_text("Dismiss")
                                .clicked()
                            {
                                answer = Some(crate::nudge_engine::Outcome::Declined);
                            }
                        });
                    });
                    ui.add_space(2.0);
                    ui.label(RichText::new(&ask.body).size(12.0).color(muted()));
                    ui.add_space(8.0);
                    // The `horizontal` wrapper is load-bearing, not decoration. A bare
                    // `with_layout(right_to_left)` inside a vertical claims ALL the remaining
                    // height, which made this banner swallow the entire settings page and pinned
                    // its buttons to the bottom of the window. `horizontal` constrains it to one
                    // row's height, which is what a button row is.
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if accent_button(ui, &ask.action_label)
                                .on_hover_text(
                                    "Opens your browser to sign in, then syncs these settings to \
                                 your Connections account.",
                                )
                                .clicked()
                            {
                                answer = Some(crate::nudge_engine::Outcome::Accepted);
                                connect = true;
                            }
                            // Same answers, same words, as the web banner every other LunarWerx app
                            // shows (`nudge-banner.ts`). "Not now" and the × are the same thing -
                            // a dismissal worth one interval - and there is deliberately no
                            // permanent opt-out: the engine has no state that could express one.
                            // See `nudge_engine.rs`'s header for the decision and what it costs.
                            if ui
                                .button(RichText::new("Not now").size(12.0).color(muted()))
                                .clicked()
                            {
                                answer = Some(crate::nudge_engine::Outcome::Snoozed);
                            }
                            // The month-long dismissal only exists from the fourth ask on, and the
                            // ENGINE decides that, never a count re-derived here.
                            if ask.can_snooze_month
                                && ui
                                    .button(
                                        RichText::new("Remind me in a month")
                                            .size(12.0)
                                            .color(muted()),
                                    )
                                    .on_hover_text(
                                        "Hides this for a month. Settings sync stays available on \
                                 this page in the meantime.",
                                    )
                                    .clicked()
                            {
                                answer = Some(crate::nudge_engine::Outcome::SetCadence(
                                    crate::nudge_engine::Cadence::Monthly,
                                ));
                            }
                        });
                    });
                });
            });
        ui.add_space(10.0);

        if let Some(outcome) = answer {
            crate::nudge::record(outcome);
            self.nudge_ask = None;
            if connect {
                // Start the app's OWN sign-in rather than sending them to a web page and hoping
                // they come back and find the sync card. The offer is already built; the prompt's
                // only job was to say so. `begin_sign_in` is the exact path the Settings sync
                // button runs, so this app has one sign-in flow, not two that can drift.
                let ctx = ui.ctx().clone();
                self.begin_sign_in(&ctx);
                self.tab = super::nav::Tab::Application;
                self.status =
                    "Finish signing in with Connections in your browser\u{2026}".to_string();
            }
        }
    }

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
    /// A hotkey text field with a small, subtle "record" dot tucked into its
    /// right edge (instead of a separate wide button). Click the dot to arm
    /// capture — the next keypress *or mouse button* fills the field; click
    /// again (or Esc) to cancel. Armed = a solid accent dot; the field greys while listening so
    /// the keypress can't also land in the text well. `width` matches this
    /// field to the control directly above it (Language / Mode) so the 2×2
    /// block reads as two clean columns instead of the hotkey wells jutting
    /// out wider.
    pub(crate) fn hotkey_field_ui(&mut self, ui: &mut egui::Ui, field: HotkeyField, width: f32) {
        let recording = self.recording == Some(field);
        let value = match field {
            HotkeyField::Toggle => &mut self.draft.toggle_hotkey,
            HotkeyField::Hold => &mut self.draft.hold_hotkey,
        };
        // The record dot floats over the field's right edge; padding-right on
        // the well keeps typed text from sliding under it.
        let resp = ui.add_enabled(
            !recording,
            styled_input(value).desired_width(width).margin(Margin {
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
                "Listening — press a key or mouse button (Esc to cancel)"
            } else {
                "Record hotkey"
            });
        if hit.clicked() {
            self.recording = if recording { None } else { Some(field) };
        }
    }
    pub(crate) fn dictation_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
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
                        self.hotkey_field_ui(ui, HotkeyField::Toggle, 130.0);
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
                        self.hotkey_field_ui(ui, HotkeyField::Hold, 120.0);
                        ui.end_row();
                    });
            });

            // Windows only grants a hotkey to the first process that asks for
            // it; if another app got there first, `RegisterHotKey` fails and
            // that failure otherwise only reaches a log file. Surface it here
            // so a combo Windows won't grant is never silently invisible.
            if crate::hotkeys::hotkeys_blocked() {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Another app is holding one of these hotkeys \u{2014} try a different \
                         combination.",
                    )
                    .size(11.5)
                    .color(bad()),
                );
            }

            // No separator here: the timing row sits snug under the 2×2 block
            // above so it reads as one group and the card stays short.
            ui.add_space(4.0);

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
                        ui.label("Keep listening after")
                            .on_hover_text(TIP_LISTEN_TAIL);
                        secs_input(ui, &mut self.draft.listen_tail_ms, 0.3..=3.0, "listen_tail")
                            .on_hover_text(TIP_LISTEN_TAIL);
                        ui.end_row();

                        // Meaningless with the cleanup pass off, so gray it
                        // out rather than letting it read as a live budget.
                        let polish_on = self.draft.polish_enabled;
                        ui.add_enabled_ui(polish_on, |ui| {
                            ui.label("AI cleanup waits").on_hover_text(TIP_POLISH_WAIT);
                        });
                        ui.add_enabled_ui(polish_on, |ui| {
                            secs_input(
                                ui,
                                &mut self.draft.polish_deadline_ms,
                                0.1..=2.0,
                                "polish_deadline",
                            )
                            .on_hover_text(TIP_POLISH_WAIT);
                        });
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
                    if text_replacements_button(ui, repl_count)
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

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(
                RichText::new("Custom vocabulary")
                    .font(semibold(13.0))
                    .color(text()),
            );
            ui.label(
                RichText::new(
                    "Words and phrases sent to the provider to bias recognition toward them: \
                     names, jargon, product names it keeps mishearing. This is \
                     different from text replacements above, which repair the text *after* \
                     recognition. One term per line.",
                )
                .size(11.5)
                .color(muted()),
            );
            ui.add_space(6.0);
            ui.add(
                egui::TextEdit::multiline(&mut self.vocabulary_text)
                    .desired_width(f32::INFINITY)
                    .desired_rows(4)
                    .margin(Margin::symmetric(6, CTRL_PAD))
                    .hint_text("Supabase\nCloudflare\nQuickDictate"),
            );
        });
    }
    /// Where QuickDictate writes its runtime files.
    ///
    /// This exists because the default -- "next to the exe" -- is actively bad
    /// for the very common case of an exe kept on the Desktop: the logs folder,
    /// the stats json, the sync credential blob, and the update cache all land
    /// on the Desktop with it.
    ///
    /// The field edits `draft.data_dir`, so it saves through the same Save
    /// button as everything else and takes effect on the next start (the data
    /// folder is resolved once, at boot -- see [`crate::paths`]). The blurb
    /// under the row says so rather than pretending it is live.
    pub(crate) fn data_folder_section(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);

        // Both of these are cached OnceLock reads, deliberately: this runs on
        // every repaint, and the obvious spelling of "the default folder"
        // (`Config::settings_path().parent()`) stats the filesystem up to eight
        // times per call.
        let default_dir = crate::paths::default_dir();
        let live_dir = crate::paths::data_dir();

        ui.horizontal(|ui| {
            ui.label(RichText::new("Files").size(12.0).color(muted()));
            ui.label(
                RichText::new("\u{2014} folder for the logs, stats, sync, and update files")
                    .size(12.0)
                    .color(muted()),
            );
        });
        ui.add_space(4.0);

        // Clicks are captured here and acted on after the row closes, so no
        // handler borrows `self` while the text edit holds `draft.data_dir`.
        let mut browse = false;
        let mut use_app_data = false;
        let mut use_default = false;
        let mut open_folder = false;

        ui.horizontal(|ui| {
            ui.add(
                styled_input(&mut self.draft.data_dir)
                    .hint_text(default_dir.to_string_lossy().to_string())
                    .desired_width(330.0),
            )
            .on_hover_text(
                "Leave empty to keep everything next to QuickDictate.exe (the default). \
                 %VARIABLES% are expanded, so %LOCALAPPDATA%\\QuickDictate works. \
                 The path must be a full one, starting with a drive letter.\n\n\
                 settings.json itself stays where it is \u{2014} QuickDictate has to find it \
                 before it can read this setting out of it. Don't move that file by hand; \
                 \u{201c}Use AppData\u{201d} is the one place it is also looked for.",
            );
            browse = ui.button("Browse\u{2026}").clicked();
            open_folder = ui
                .button("Open")
                .on_hover_text("Show the folder currently in use in Explorer.")
                .clicked();
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            use_app_data = ui
                .button("Use AppData")
                .on_hover_text(
                    "%LOCALAPPDATA%\\QuickDictate \u{2014} the usual place for an app's own \
                     files, and the one that leaves QuickDictate's own folder empty.",
                )
                .clicked();
            use_default = ui
                .button("Next to the app")
                .on_hover_text("Back to the default: alongside QuickDictate.exe.")
                .clicked();
        });

        // Shape check only -- pure, so it can run every frame. Whether the
        // folder is actually writable is checked when Browse returns and again
        // at startup, which is the only moment it can be acted on.
        let typed = self.draft.data_dir.trim();
        if !typed.is_empty() && crate::paths::expand(typed).is_none() {
            ui.add_space(2.0);
            ui.label(
                RichText::new(
                    "Not a usable path. It needs to be a full path (C:\\\u{2026}) and any \
                     %VARIABLE% in it has to exist \u{2014} QuickDictate will keep using the \
                     current folder until it is.",
                )
                .size(11.0)
                .color(bad()),
            );
        }

        ui.add_space(2.0);
        ui.label(
            RichText::new(format!(
                "In use now: {}{} \u{2014} a change applies after Save and restart, and \
                 QuickDictate moves the existing files across for you.",
                live_dir.display(),
                // Describes LIVE_DIR, so it must be decided by live_dir. Reading
                // the draft here labelled a still-active custom folder "(the
                // default)" the moment the field was cleared and before any save.
                if live_dir == default_dir {
                    " (the default)"
                } else {
                    ""
                }
            ))
            .size(11.0)
            .color(muted()),
        );

        if browse {
            let start = crate::paths::expand(&self.draft.data_dir).unwrap_or(live_dir.clone());
            if let Some(dir) = crate::paths::pick_folder(Some(&start)) {
                match crate::paths::check_writable(&dir) {
                    Ok(()) => {
                        self.draft.data_dir = dir.to_string_lossy().into_owned();
                        // Accept the choice either way, but say so if the folder
                        // is already somebody else's.
                        self.status = crate::paths::folder_caution(&dir).unwrap_or_default();
                    }
                    Err(e) => self.status = format!("Can't use that folder: {e}"),
                }
            }
        }
        if use_app_data {
            match crate::paths::app_data_dir() {
                Some(dir) => {
                    self.draft.data_dir = dir.to_string_lossy().into_owned();
                    // Clear any "can't use that folder" left by an earlier
                    // Browse: it describes a choice that is no longer selected.
                    self.status.clear();
                }
                None => {
                    self.status = "Windows did not report a LOCALAPPDATA folder.".to_string();
                }
            }
        }
        if use_default {
            self.draft.data_dir.clear();
            self.status.clear();
        }
        if open_folder {
            let _ = std::fs::create_dir_all(&live_dir);
            let _ = std::process::Command::new("explorer.exe")
                .arg(&live_dir)
                .spawn();
        }
    }

    pub(crate) fn application_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            // Eight toggles split across two columns. The wordiest options are
            // trimmed to short labels with the detail moved into their hover
            // tooltips. "Enable per-app profiles" lives here too — it used to
            // sit in its own near-empty card.
            self.application_toggles(ui);

            self.data_folder_section(ui);

            // ---- AI cleanup setup ---------------------------------------
            // Only shown once the box above is ticked: with it off this is
            // noise, and with it on the missing key is the single thing most
            // likely to make the feature look broken.
            if self.draft.polish_enabled {
                self.polish_setup_section(ui);
            }

            // "Active profiles" editor — shown only when a power user has
            // actually added `profiles` to settings.json. With none
            // configured, the toggle above is the whole story and we don't
            // waste a row on a "None configured" line.
            self.active_profiles_section(ui);
        });
    }

    /// The eight application-behavior toggles, split across two columns.
    fn application_toggles(&mut self, ui: &mut egui::Ui) {
        ui.columns(2, |cols| {
            let left = &mut cols[0];
            blue_check(
                left,
                &mut self.draft.prewarm_keys,
                "Probe keys at startup (prewarm)",
            )
            .on_hover_text("On launch, warm up your API keys so the first dictation is fast.");
            blue_check(left, &mut self.draft.run_at_startup, "Start with Windows")
                .on_hover_text("Launch QuickDictate automatically when you sign in to Windows.");
            blue_check(left, &mut self.draft.hide_tray_icon, "Hide tray icon").on_hover_text(
                "QuickDictate keeps running in the background with no icon shown. \
                     To get back in, launch QuickDictate again -- it will reopen this \
                     Settings window instead of starting a second copy, and you can \
                     re-enable the icon here.",
            );
            blue_check(
                left,
                &mut self.draft.update_auto_check,
                "Check for updates daily",
            )
            .on_hover_text("Automatically check for a newer QuickDictate release once a day.");
            // Only meaningful once auto-check is on; hidden otherwise
            // rather than shown-but-inert.
            if self.draft.update_auto_check {
                blue_check(
                    left,
                    &mut self.draft.update_auto_install,
                    "Install updates automatically without asking",
                )
                .on_hover_text(
                    "By default a newer release only shows as \u{201c}available\u{201d} \u{2014} \
                     you click to install it (About window). Turn this on to install \
                     automatically as soon as the daily check finds one, with no confirmation.",
                );
            }

            let right = &mut cols[1];
            blue_check(
                right,
                &mut self.draft.enable_logging,
                "Write quickdictate.log",
            )
            .on_hover_text("Write troubleshooting diagnostics in the app's logs folder.");
            // Dependent on the log file existing at all: without
            // `enable_logging` there is nothing for this to write into, so
            // gray it out rather than letting it read as an active privacy
            // choice that does nothing.
            let logging_on = self.draft.enable_logging;
            right.add_enabled_ui(logging_on, |right| {
                blue_check(
                    right,
                    &mut self.draft.log_transcripts,
                    "Log full dictated text",
                )
                .on_hover_text(if logging_on {
                    "Deep debugging only: records the actual text you dictate into \
                     the log file. Leave off for privacy."
                } else {
                    "Turn on \u{201c}Write quickdictate.log\u{201d} first: there is no \
                     log file for this to write into."
                });
            });
            blue_check(
                right,
                &mut self.draft.voice_commands,
                "\u{201c}Scratch that\u{201d} voice command",
            )
            .on_hover_text(
                "Say \u{201c}scratch that\u{201d} to automatically undo your last paste.",
            );
            blue_check(
                right,
                &mut self.draft.polish_enabled,
                "Clean up with AI before pasting",
            )
            .on_hover_text(TIP_POLISH);
            blue_check(
                right,
                &mut self.draft.profiles_enabled,
                "Enable per-app profiles",
            )
            .on_hover_text(
                "Apply per-application overrides for punctuation, spacing, and \
                 replacements based on the app you're typing into.",
            );
        });
    }

    /// AI-cleanup setup: the key-count status line, the "Manage keys…" /
    /// model row, and the free-key hint.
    fn polish_setup_section(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("AI cleanup").size(12.0).color(muted()));
            let keys = self
                .draft
                .polish_keys
                .iter()
                .filter(|k| !k.trim().is_empty())
                .count();
            if keys == 0 {
                ui.label(
                    RichText::new("\u{2014} needs an API key, until then pastes are unchanged")
                        .size(12.0)
                        .color(bad()),
                );
            } else {
                ui.label(
                    RichText::new(format!(
                        "\u{2014} {keys} key{}",
                        if keys == 1 { "" } else { "s" }
                    ))
                    .size(12.0)
                    .color(good()),
                );
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if accent_button(ui, "Manage keys\u{2026}")
                .on_hover_text(TIP_POLISH_KEYS)
                .clicked()
            {
                self.open_keys_modal(KEYS_TARGET_POLISH);
            }
            ui.label("Model").on_hover_text(TIP_POLISH_KEYS);
            ui.add(
                egui::TextEdit::singleline(&mut self.draft.polish_model)
                    .desired_width(200.0)
                    .margin(Margin::symmetric(6, CTRL_PAD)),
            )
            .on_hover_text(TIP_POLISH_KEYS);
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Free key: aistudio.google.com/apikey \u{2014} hover any control here \
                 for which API to enable and which model to pick.",
            )
            .size(11.0)
            .color(muted()),
        );
    }

    /// "Active profiles" editor, a no-op when none are configured (so we
    /// don't waste a row on a "None configured" line). Only Language,
    /// Provider, and vocabulary are editable here; the name, match list, and
    /// text replacements still require settings.json (a full add/remove/
    /// reorder editor is out of scope for this pass).
    fn active_profiles_section(&mut self, ui: &mut egui::Ui) {
        if self.draft.profiles.is_empty() {
            return;
        }
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(RichText::new("Active profiles").size(12.0).color(muted()));
        ui.label(
            RichText::new(
                "Language, provider, and vocabulary can be tuned here. Edit \
                 settings.json to add, remove, rename, or reorder profiles, or to \
                 change their match list or text replacements.",
            )
            .size(11.0)
            .color(muted()),
        );
        ui.add_space(4.0);

        // `draft.profiles` and `profile_vocab_text` are disjoint fields, so
        // both can be borrowed mutably at once; keep them in lockstep
        // defensively in case a hand-edit (via the settings.json-changed
        // prompt's Reload) changed the profile count out from under the
        // scratch buffers.
        if self.profile_vocab_text.len() != self.draft.profiles.len() {
            self.profile_vocab_text
                .resize(self.draft.profiles.len(), String::new());
        }
        let profiles = &mut self.draft.profiles;
        let vocab_bufs = &mut self.profile_vocab_text;
        for (idx, (p, vocab_buf)) in profiles.iter_mut().zip(vocab_bufs.iter_mut()).enumerate() {
            profile_editor_row(ui, idx, p, vocab_buf);
            ui.add_space(4.0);
        }
    }
}
