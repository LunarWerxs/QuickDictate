//! The API-key manager modal: the row list, the add row, the bulk paste
//! editor, and the actions that commit them.

use super::modals::{ModalAction, ModalOutcome};
use super::*;

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
pub(super) fn render_keys_modal(
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
