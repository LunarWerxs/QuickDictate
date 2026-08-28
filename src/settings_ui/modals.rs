//! The Settings window's modal dialogs: bulk key and replacement
//! editors, the confirm prompts, and the frame they share.

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
struct ModalOutcome {
    action: ModalAction,
    test_request: Option<Vec<String>>,
    reset_stats: bool,
    do_default_reset: bool,
    do_close_discard: bool,
    do_close_save: bool,
    do_reload_from_disk: bool,
    do_overwrite: bool,
}

/// "No dictations yet" placeholder, shown for a range that's had none.
fn render_stats_empty_state(ui: &mut egui::Ui, stats_snapshot: &crate::stats::UsageStats) {
    ui.vertical_centered(|ui| {
        ui.add_space(5.0);
        ui.label(
            RichText::new(if stats_snapshot.total_dictations == 0 {
                "Your first dictation will start the scoreboard."
            } else {
                "No dictations in this range yet."
            })
            .font(semibold(14.0))
            .color(text()),
        );
        ui.label(
            RichText::new(if stats_snapshot.total_dictations == 0 {
                "Words and processed audio time are tracked as numeric totals."
            } else {
                "Recent-range history fills in as you dictate with this version."
            })
            .size(12.0)
            .color(muted()),
        );
        ui.add_space(5.0);
    });
}

/// The two stat-tile rows (words/audio, dictations/pace) plus the
/// "longest dictations" line, shown once the range has at least one.
fn render_stats_tiles(ui: &mut egui::Ui, stats_view: &crate::stats::StatsView, range: StatsRange) {
    let average_words = stats_view.totals.words / stats_view.totals.dictations.max(1);
    let pace = stats_view
        .totals
        .words
        .saturating_mul(60_000)
        .checked_div(stats_view.totals.audio_ms)
        .unwrap_or(0);
    let words_detail = match range {
        StatsRange::Last24Hours => "In the last 24 hours",
        StatsRange::Last7Days => "In the last 7 days",
        StatsRange::AllTime => "All recognized words",
    };

    ui.columns(2, |cols| {
        stat_tile(
            &mut cols[0],
            "WORDS TRANSCRIBED",
            grouped_number(stats_view.totals.words),
            words_detail,
        );
        stat_tile(
            &mut cols[1],
            "AUDIO PROCESSED",
            format_audio_time(stats_view.totals.audio_ms),
            "Trailing silence excluded",
        );
    });
    ui.add_space(7.0);
    ui.columns(2, |cols| {
        stat_tile(
            &mut cols[0],
            "DICTATIONS",
            grouped_number(stats_view.totals.dictations),
            &format!("{average_words} words on average"),
        );
        stat_tile(
            &mut cols[1],
            "SPEAKING PACE",
            format!("{pace} wpm"),
            "Based on processed audio",
        );
    });

    ui.add_space(13.0);
    ui.label(
        RichText::new("LONGEST DICTATIONS")
            .font(semibold(11.5))
            .color(muted()),
    );
    ui.label(
        RichText::new(format!(
            "Most words: {} \u{00b7} Longest audio: {}",
            grouped_number(stats_view.totals.longest_dictation_words),
            format_audio_time(stats_view.totals.longest_dictation_audio_ms)
        ))
        .font(semibold(14.0))
        .color(text()),
    );
}

/// The "BY PROVIDER" breakdown, shown when more than one provider has been
/// used in range — sorted by dictation count, ties broken by provider id.
fn render_stats_provider_breakdown(ui: &mut egui::Ui, stats_view: &crate::stats::StatsView) {
    if stats_view.totals.providers.is_empty() {
        return;
    }
    ui.add_space(13.0);
    ui.label(
        RichText::new("BY PROVIDER")
            .font(semibold(11.5))
            .color(muted()),
    );
    let mut providers = stats_view.totals.providers.iter().collect::<Vec<_>>();
    providers.sort_by(|left, right| {
        right
            .1
            .dictations
            .cmp(&left.1.dictations)
            .then_with(|| left.0.cmp(right.0))
    });
    for (id, totals) in providers {
        ui.horizontal(|ui| {
            ui.label(RichText::new(provider_label(id)).size(12.5).color(text()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} words \u{00b7} {} \u{00b7} {} session{}",
                        grouped_number(totals.words),
                        format_audio_time(totals.audio_ms),
                        grouped_number(totals.dictations),
                        if totals.dictations == 1 { "" } else { "s" }
                    ))
                    .size(11.5)
                    .color(muted()),
                );
            });
        });
    }
}

/// The totals section: either the empty state, or the tiles plus the
/// per-provider breakdown.
fn render_stats_totals(
    ui: &mut egui::Ui,
    stats_view: &crate::stats::StatsView,
    stats_snapshot: &crate::stats::UsageStats,
    range: StatsRange,
) {
    if stats_view.totals.dictations == 0 {
        render_stats_empty_state(ui, stats_snapshot);
    } else {
        render_stats_tiles(ui, stats_view, range);
        render_stats_provider_breakdown(ui, stats_view);
    }
}

/// The "Reset all stats?" inline confirmation box, shown while
/// `stats_reset_confirm` is set.
fn render_stats_reset_confirm_box(
    ui: &mut egui::Ui,
    stats_reset_confirm: &mut bool,
    out: &mut ModalOutcome,
) {
    ui.add_space(12.0);
    egui::Frame::new()
        .fill(bad().gamma_multiply(0.09))
        .stroke(Stroke::new(1.0, bad().gamma_multiply(0.45)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Reset all stats? This also resets synced devices.")
                        .size(11.5)
                        .color(text()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(RichText::new("Reset").font(semibold(11.5)).color(bad()))
                        .clicked()
                    {
                        out.reset_stats = true;
                        *stats_reset_confirm = false;
                    }
                    if ui.button("Cancel").clicked() {
                        *stats_reset_confirm = false;
                    }
                });
            });
        });
}

/// The trailing "Reset stats \u{2014} privacy note \u{2014} Done" row.
fn render_stats_footer_row(
    ui: &mut egui::Ui,
    stats_reset_confirm: &mut bool,
    out: &mut ModalOutcome,
) {
    ui.add_space(12.0);
    ui.separator();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if !*stats_reset_confirm
            && ui
                .button(
                    RichText::new("Reset stats")
                        .font(semibold(11.5))
                        .color(bad()),
                )
                .clicked()
        {
            *stats_reset_confirm = true;
        }
        ui.label(
            RichText::new("Numeric totals only \u{2014} transcript text is never stored.")
                .size(10.5)
                .color(muted()),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if accent_button(ui, "Done").clicked() {
                out.action = ModalAction::Cancel;
            }
        });
    });
}

/// The Stats modal: usage totals, the provider breakdown, and the reset
/// confirmation. Doesn't touch `SettingsApp` — everything it needs is
/// already snapshotted by the caller — so it's a free function. Takes and
/// returns `(stats_range, stats_reset_confirm)` by value since both are
/// plain `Copy` UI state mirrored from `SettingsApp`, not deferred actions.
fn render_stats_modal(
    ctx: &egui::Context,
    stats_snapshot: &crate::stats::UsageStats,
    mut stats_range: StatsRange,
    mut stats_reset_confirm: bool,
    out: &mut ModalOutcome,
) -> (StatsRange, bool) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let backdrop = SettingsApp::modal_frame(ctx, "Dictation stats", 500.0, |ui| {
        stats_range_selector(ui, &mut stats_range);
        let stats_view = stats_snapshot.view(stats_range, now);
        let range_detail = match stats_range {
            StatsRange::Last24Hours => "Rolling 24-hour window",
            StatsRange::Last7Days => "Rolling 7-day window",
            StatsRange::AllTime => "Since stats tracking began",
        };
        ui.add_space(4.0);
        ui.label(RichText::new(range_detail).size(11.0).color(muted()));
        ui.add_space(8.0);
        if stats_range == StatsRange::AllTime {
            stats_provider_chart(
                ui,
                &stats_view.totals.providers,
                stats_view.totals.dictations,
            );
        } else {
            stats_chart(ui, &stats_view.chart, &stats_view.chart_caption);
        }
        ui.add_space(10.0);

        render_stats_totals(ui, &stats_view, stats_snapshot, stats_range);

        if stats_reset_confirm {
            render_stats_reset_confirm_box(ui, &mut stats_reset_confirm, out);
        }

        render_stats_footer_row(ui, &mut stats_reset_confirm, out);
    });
    if backdrop || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        out.action = ModalAction::Cancel;
    }
    (stats_range, stats_reset_confirm)
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

/// The scrollable key list: one row per saved key, its mask, its test
/// verdict, and a remove button.
fn render_key_rows(ui: &mut egui::Ui, state: &mut KeysModalState) {
    if state.rows.is_empty() {
        ui.label(RichText::new("No keys yet — paste one below.").color(muted()));
    }
    let mut remove: Option<usize> = None;
    egui::ScrollArea::vertical()
        .id_salt("api_key_rows")
        .max_height(220.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for (i, row) in state.rows.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(mask(&row.value))
                            .monospace()
                            .size(13.0)
                            .color(text()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
                    });
                });
            }
        });
    if let Some(i) = remove {
        state.rows.remove(i);
    }
}

/// The single-key paste field plus its Add / Bulk add buttons.
fn render_key_add_row(ui: &mut egui::Ui, state: &mut KeysModalState) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let edit = styled_input(&mut state.add_text)
            .hint_text("paste a new key\u{2026}")
            .desired_width((ui.available_width() - 150.0).max(120.0))
            .font(egui::TextStyle::Monospace);
        let resp = ui.add(edit);
        let submitted = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let add = ui.button("Add").clicked() || submitted;
        if add && !state.add_text.trim().is_empty() {
            match merge_key_lines(&mut state.rows, &state.add_text) {
                Ok(summary) => {
                    state.bulk_error = false;
                    state.bulk_note = if summary.added == 0 {
                        "That key is already in the list.".into()
                    } else {
                        "Key added.".into()
                    };
                    state.add_text.clear();
                }
                Err(_) => {
                    state.bulk_error = true;
                    state.bulk_note = "A key cannot contain spaces or control characters.".into();
                }
            }
        }
        if ui.button("Bulk add").clicked() {
            state.bulk = !state.bulk;
            state.bulk_note.clear();
            state.bulk_error = false;
        }
    });
}

/// The "paste many keys at once" text-editor frame, shown while
/// `state.bulk` is set.
fn render_key_bulk_editor(ui: &mut egui::Ui, state: &mut KeysModalState, out: &mut ModalOutcome) {
    if !state.bulk {
        return;
    }
    ui.add_space(10.0);
    egui::Frame::new()
        .fill(input_bg())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.label(
                RichText::new("One API key per line")
                    .font(semibold(13.0))
                    .color(text()),
            );
            ui.label(
                RichText::new("Blank lines and keys already in the list are skipped.")
                    .size(11.5)
                    .color(muted()),
            );
            ui.add_space(6.0);
            ui.add(
                egui::TextEdit::multiline(&mut state.bulk_text)
                    .desired_width(f32::INFINITY)
                    .desired_rows(8)
                    .margin(Margin::symmetric(6, CTRL_PAD))
                    .font(egui::TextStyle::Monospace)
                    .hint_text("sk_example_key_1\nsk_example_key_2\nsk_example_key_3"),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    state.bulk = false;
                    state.bulk_text.clear();
                    state.bulk_note.clear();
                }
                if accent_button(ui, "Save").clicked() {
                    match merge_key_lines(&mut state.rows, &state.bulk_text) {
                        Ok(summary) => {
                            state.bulk_error = false;
                            state.bulk_note = format!(
                                "{} added \u{00b7} {} duplicate{} skipped",
                                summary.added,
                                summary.duplicates,
                                if summary.duplicates == 1 { "" } else { "s" }
                            );
                            state.bulk_text.clear();
                            state.bulk = false;
                            out.action = ModalAction::CommitAndSave;
                        }
                        Err(lines) => {
                            state.bulk_error = true;
                            state.bulk_note = format!(
                                "Nothing imported \u{2014} whitespace/control characters on line{} {}.",
                                if lines.len() == 1 { "" } else { "s" },
                                lines
                                    .iter()
                                    .map(usize::to_string)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
                        }
                    }
                }
            });
        });
}

/// The trailing Test all / Done / Cancel row, hidden while the bulk editor
/// (above) is open.
fn render_key_actions_row(ui: &mut egui::Ui, state: &KeysModalState, out: &mut ModalOutcome) {
    if state.bulk {
        return;
    }
    ui.add_space(12.0);
    ui.separator();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if accent_button(ui, "Test all").clicked() {
            out.test_request = Some(state.rows.iter().map(|r| r.value.clone()).collect());
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if accent_button(ui, "Done").clicked() {
                out.action = ModalAction::Commit;
            }
            if ui.button("Cancel").clicked() {
                out.action = ModalAction::Cancel;
            }
        });
    });
}

/// The API-key manager modal: the key list with per-row test verdicts, the
/// add/bulk-add controls, and the Test all / Done / Cancel row. Doesn't need
/// the rest of `SettingsApp` — just which pool it's editing and the current
/// provider label — so it takes those two directly instead of `&self`.
fn render_keys_modal(
    ctx: &egui::Context,
    keys_target: &str,
    provider: &str,
    state: &mut KeysModalState,
    out: &mut ModalOutcome,
) {
    let polish_pool = keys_target == KEYS_TARGET_POLISH;
    let title = if polish_pool {
        "AI cleanup API keys".to_string()
    } else {
        format!("{} API keys", provider_label(provider))
    };
    let backdrop = SettingsApp::modal_frame(ctx, &title, 460.0, |ui| {
        render_key_rows(ui, state);
        render_key_add_row(ui, state);
        render_key_bulk_editor(ui, state, out);
        if !state.bulk_note.is_empty() {
            ui.add_space(5.0);
            ui.label(
                RichText::new(state.bulk_note.clone())
                    .size(11.5)
                    .color(if state.bulk_error { bad() } else { muted() }),
            );
        }
        render_key_actions_row(ui, state, out);
    });
    if backdrop || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        if state.bulk {
            state.bulk = false;
            state.bulk_text.clear();
            state.bulk_note.clear();
        } else {
            out.action = ModalAction::Cancel;
        }
    }
}

/// The header row: the hint label plus the table/text-editor toggle button,
/// which also converts `state`'s rows to/from the bulk text on flip.
fn render_replacements_toggle(ui: &mut egui::Ui, state: &mut ReplacementsModalState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Misheard phrase \u{2192} what to type instead.").color(muted()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = if state.bulk {
                "Table view"
            } else {
                "Text editor\u{2026}"
            };
            if ui.button(label).clicked() {
                if state.bulk {
                    state.rows = text_to_replacements(&state.bulk_text);
                } else {
                    state.bulk_text = replacements_to_text(&state.rows);
                }
                state.bulk = !state.bulk;
            }
        });
    });
}

/// The paste-friendly `misheard => replacement` text editor, shown while
/// `state.bulk` is set.
fn render_replacements_bulk_editor(ui: &mut egui::Ui, state: &mut ReplacementsModalState) {
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
                egui::TextEdit::multiline(&mut state.bulk_text)
                    .font(egui::TextStyle::Monospace)
                    .desired_rows(14)
                    .desired_width(f32::INFINITY)
                    .hint_text("Chat GPT => ChatGPT\nGithub => GitHub"),
            );
        });
}

/// The row-by-row table: one `[from | to | remove]` line per replacement,
/// plus the trailing "add a new one" row.
fn render_replacements_table(ui: &mut egui::Ui, state: &mut ReplacementsModalState) {
    let mut remove: Option<usize> = None;
    egui::ScrollArea::vertical()
        .max_height(280.0)
        .show(ui, |ui| {
            for (i, (from, to)) in state.rows.iter_mut().enumerate() {
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
        state.rows.remove(i);
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add(
            styled_input(&mut state.add_from)
                .hint_text("misheard\u{2026}")
                .desired_width(185.0),
        );
        ui.label(RichText::new("\u{2192}").color(muted()));
        ui.add(
            styled_input(&mut state.add_to)
                .hint_text("replace with\u{2026}")
                .desired_width(185.0),
        );
        if ui.button("Add").clicked() && !state.add_from.trim().is_empty() {
            state.rows.push((
                state.add_from.trim().to_string(),
                state.add_to.trim().to_string(),
            ));
            state.add_from.clear();
            state.add_to.clear();
        }
    });
}

/// The trailing Done / Cancel row.
fn render_replacements_actions_row(ui: &mut egui::Ui, out: &mut ModalOutcome) {
    ui.add_space(12.0);
    ui.separator();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if accent_button(ui, "Done").clicked() {
                out.action = ModalAction::Commit;
            }
            if ui.button("Cancel").clicked() {
                out.action = ModalAction::Cancel;
            }
        });
    });
}

/// The text-replacements modal: the table/text-editor toggle, the row
/// editor, and the Done / Cancel row.
fn render_replacements_modal(
    ctx: &egui::Context,
    state: &mut ReplacementsModalState,
    out: &mut ModalOutcome,
) {
    let backdrop = SettingsApp::modal_frame(ctx, "Text replacements", 500.0, |ui| {
        render_replacements_toggle(ui, state);
        ui.add_space(8.0);
        if state.bulk {
            render_replacements_bulk_editor(ui, state);
        } else {
            render_replacements_table(ui, state);
        }
        render_replacements_actions_row(ui, out);
    });
    if backdrop {
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
