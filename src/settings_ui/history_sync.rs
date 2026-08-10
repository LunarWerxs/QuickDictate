//! The transcript-history browser and the Connections settings-sync
//! card.

use super::*;

impl super::SettingsApp {
    /// Recent-transcriptions browser: search/filter, per-entry Copy and Paste
    /// again. `app.history` is in-memory only for this session (see
    /// `TranscriptHistory`) -- this is a bigger window onto the same list the
    /// tray's "Recent transcriptions" submenu already shows. Button clicks are
    /// captured into locals and acted on after the card closure, matching the
    /// rest of this file's pattern for keeping `&mut self` calls unnested.
    pub(crate) fn history_card(&mut self, ui: &mut egui::Ui) {
        let mut do_copy: Option<usize> = None;
        let mut do_replay: Option<usize> = None;
        card(ui, |ui| {
            ui.label(
                RichText::new(
                    "Your recent dictations for this session (not saved to disk). Copy one back \
                     to the clipboard, or paste it again into whatever's currently focused.",
                )
                .size(11.5)
                .color(muted()),
            );
            ui.add_space(6.0);
            ui.add(
                styled_input(&mut self.history_filter)
                    .hint_text("Filter\u{2026}")
                    .desired_width(220.0),
            );
            ui.add_space(6.0);

            // Re-lock and re-filter only when the history or the filter text
            // actually moved since the last frame (see `HistoryCache`) —
            // `history_card` renders every frame, and cloning up to
            // `HISTORY_CAP` full transcript strings on every one of them for
            // an unchanging list is pure waste.
            let current_version = self.app.history.lock().version();
            if history_cache_stale(
                self.history_cache.version,
                current_version,
                &self.history_cache.filter,
                &self.history_filter,
            ) {
                let entries = self.app.history.lock().snapshot();
                self.history_cache.history_empty = entries.is_empty();
                let filter = self.history_filter.clone();
                self.history_cache.rows = entries
                    .into_iter()
                    .enumerate()
                    .filter(|(_, e)| history_matches(&filter, &e.text))
                    .collect();
                self.history_cache.version = current_version;
                self.history_cache.filter = filter;
            }

            if self.history_cache.history_empty {
                ui.label(
                    RichText::new("No dictations yet this session.")
                        .size(12.0)
                        .color(muted()),
                );
                return;
            }
            if self.history_cache.rows.is_empty() {
                ui.label(RichText::new("No matches.").size(12.0).color(muted()));
                return;
            }
            egui::ScrollArea::vertical()
                .id_salt("history_rows")
                .max_height(220.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (idx, entry) in &self.history_cache.rows {
                        ui.horizontal(|ui| {
                            let preview = truncate_preview(&entry.text.replace('\n', " "), 70);
                            ui.label(RichText::new(preview).color(text()))
                                .on_hover_text(entry.text.clone());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("Paste again").clicked() {
                                        do_replay = Some(*idx);
                                    }
                                    if ui.small_button("Copy").clicked() {
                                        do_copy = Some(*idx);
                                    }
                                },
                            );
                        });
                        ui.add_space(2.0);
                    }
                });
        });
        if let Some(idx) = do_copy {
            if let Some(entry) = self.app.history.lock().get(idx) {
                match crate::output::copy_to_clipboard(&entry.text) {
                    Ok(()) => self.status = "Copied to clipboard.".into(),
                    Err(e) => self.status = format!("Copy failed: {e}"),
                }
            }
        }
        if let Some(idx) = do_replay {
            // Same mechanism as the tray's "Recent transcriptions" submenu
            // and the `paste_history:N` dev-trigger hook: hand the index to
            // the replay channel and let the output loop (see `output.rs`)
            // do the actual paste.
            let _ = self.app.replay_tx.try_send(Some(idx));
        }
    }
    /// Opt-in "Sync settings with Connections" control (spec §6.8): four states
    /// (signed out / signing in / signed in / error) plus a one-line privacy
    /// note. Button clicks are captured into locals and acted on after the card
    /// closure to keep `&mut self` calls out of nested borrows.
    pub(crate) fn sync_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut do_sign_in = false;
        let mut do_disconnect = false;
        card(ui, |ui| {
            let working = self.sync.rx.is_some();
            match self.sync.phase {
                SyncPhase::SignedOut => {
                    if accent_button(ui, "Sync settings")
                        .on_hover_text(
                            "Sign in with a free Connections account to back up your preferences \
                             and numeric stats \u{2014} hotkeys, providers, text replacements \
                             (never API keys or transcript text) \
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
                                RichText::new(format!("\u{00b7} {}", self.sync.name))
                                    .color(muted()),
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
}
