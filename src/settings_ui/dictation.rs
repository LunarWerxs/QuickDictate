//! The dictation card: hotkey capture fields, language and mode, the listen
//! tail, and the rest of what one press does.

use super::*;

impl super::SettingsApp {
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
}
