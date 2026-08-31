//! The text-replacements modal: the table, its bulk text editor, and the
//! toggle that converts between them.

use super::modals::{ModalAction, ModalOutcome};
use super::*;

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
pub(super) fn render_replacements_modal(
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
