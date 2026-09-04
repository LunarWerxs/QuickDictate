//! The application card: the data folder, the behaviour toggles, the cleanup
//! pass, and the Per-App Profiles editor.

use super::*;

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

    /// The nine application-behavior toggles, split across two columns.
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
            blue_check(
                right,
                &mut self.draft.share_usage_stats,
                "Share anonymous usage stats with LunarWerx",
            )
            .on_hover_text(
                "Once a day, send an anonymized rollup of your lifetime word/audio/\
                 dictation totals and which providers you use -- never any dictated \
                 text, hostname, username, or account info. Helps LunarWerx see which \
                 features actually get used. Off by default; on or off any time.",
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
