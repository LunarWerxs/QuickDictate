//! The transcript-history browser: filter, tick any number of rows and copy
//! them together, or copy / paste again one row from its icon buttons.

use crate::state::HistoryEntry;

use super::*;

/// Everything on the page that is not the list: the card padding, the blurb,
/// the toolbar, and their spacing. The list gets the rest of the page.
const HISTORY_CHROME_H: f32 = 140.0;
/// The list never shrinks below this, however small the window is dragged.
const HISTORY_LIST_MIN_H: f32 = 220.0;
/// Characters of a row's one-line preview; the full text is in the hover.
const PREVIEW_CHARS: usize = 80;

/// What one row's controls asked for this frame. Collected by
/// [`history_row`] and acted on by the caller, so the row itself never needs
/// `&mut SettingsApp`.
#[derive(Default)]
struct RowAction {
    /// Tick or untick this row (the box, or the text itself).
    toggle: bool,
    copy: bool,
    replay: bool,
}

/// One row of the list: the tick box, the preview (clickable, as the row's
/// own label), and the icon buttons. A free function: it needs the entry and
/// whether it is ticked, nothing else on `SettingsApp`.
fn history_row(ui: &mut egui::Ui, entry: &HistoryEntry, is_selected: bool) -> RowAction {
    let mut action = RowAction::default();
    let fill = if is_selected {
        accent().gamma_multiply(0.16)
    } else {
        Color32::TRANSPARENT
    };
    egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(6, 3))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let mut on = is_selected;
                let box_clicked = blue_check_box(ui, &mut on).changed();
                let text_clicked = history_row_label(ui, entry);
                action.toggle = box_clicked || text_clicked;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    history_row_buttons(ui, &mut action);
                });
            });
        });
    action
}

/// A row's one-line preview, with the full text on hover. The row is its own
/// label: clicking the text ticks it, the same as the box. Returns whether it
/// was clicked.
fn history_row_label(ui: &mut egui::Ui, entry: &HistoryEntry) -> bool {
    let preview = truncate_preview(&entry.text.replace('\n', " "), PREVIEW_CHARS);
    ui.add(egui::Label::new(RichText::new(preview).color(text())).sense(egui::Sense::click()))
        .on_hover_text(entry.text.clone())
        .clicked()
}

/// A row's paste-again and copy icons, right-aligned (paste outermost).
fn history_row_buttons(ui: &mut egui::Ui, action: &mut RowAction) {
    if icon_button(ui, paste_glyph())
        .on_hover_text("Paste again into whatever is focused")
        .clicked()
    {
        action.replay = true;
    }
    if icon_button(ui, copy_glyph())
        .on_hover_text("Copy to the clipboard")
        .clicked()
    {
        action.copy = true;
    }
}

/// The line under the card title, which has to say whether the history the
/// user is looking at outlives the session.
fn history_blurb(persisted: bool) -> &'static str {
    if persisted {
        "Your last 50 dictations, kept on this PC so they survive a restart or \
         an update (never synced or sent anywhere). Tick any number and copy \
         them together, or use a row's buttons to copy just that one or paste \
         it again into whatever's currently focused."
    } else {
        "Your recent dictations for this session only (history saving is off \
         on the Advanced page). Tick any number and copy them together, or use \
         a row's buttons to copy just that one or paste it again into \
         whatever's currently focused."
    }
}

impl super::SettingsApp {
    /// Recent-transcriptions browser: a bigger window onto the same list the
    /// tray's "Recent transcriptions" submenu shows (`app.history`, kept on
    /// disk between runs unless `persist_history` is off -- see
    /// `history_store`). Button clicks are captured into locals and acted on
    /// after the card closure, matching the rest of this module's pattern
    /// for keeping `&mut self` calls unnested.
    pub(crate) fn history_card(&mut self, ui: &mut egui::Ui) {
        // The list takes every point of the page the toolbar leaves it: a
        // history is more useful the more of it is on screen, and the page
        // has nothing else to show.
        let page_height = ui.available_height();
        let (copy_selected, do_copy, do_replay) = self.history_card_body(ui, page_height);
        if let Some(idx) = do_copy {
            self.copy_history_entry(idx);
        }
        if copy_selected {
            self.copy_selected_history();
        }
        if let Some(idx) = do_replay {
            // Same mechanism as the tray's "Recent transcriptions" submenu
            // and the `paste_history:N` dev-trigger hook: hand the index to
            // the replay channel and let the output loop (see `output.rs`)
            // do the actual paste.
            let _ = self.app.replay_tx.try_send(Some(idx));
        }
    }

    /// The card itself: blurb, toolbar, then the list (or why there is none).
    /// Returns whether "Copy selected" was clicked and the row indices whose
    /// copy / paste-again icons were.
    fn history_card_body(
        &mut self,
        ui: &mut egui::Ui,
        page_height: f32,
    ) -> (bool, Option<usize>, Option<usize>) {
        // Reads the SAVED setting, not the draft: the blurb describes what
        // the app is doing now, and an unsaved tick is not that yet.
        let persisted = self.app.config.load().persist_history;
        let mut copy_selected = false;
        let mut do_copy: Option<usize> = None;
        let mut do_replay: Option<usize> = None;
        card(ui, |ui| {
            blurb(ui, history_blurb(persisted));
            ui.add_space(6.0);

            self.rebuild_history_cache_if_stale();
            copy_selected = self.history_toolbar(ui);

            let Some(notice) = self.history_notice() else {
                (do_copy, do_replay) = self.history_rows(ui, page_height);
                return;
            };
            ui.label(RichText::new(notice).size(12.0).color(muted()));
        });
        (copy_selected, do_copy, do_replay)
    }

    /// The scroll area of rows. Returns the copy / paste-again requests the
    /// rows raised this frame. Only called when the list has something to
    /// show (see [`Self::history_notice`]).
    fn history_rows(
        &mut self,
        ui: &mut egui::Ui,
        page_height: f32,
    ) -> (Option<usize>, Option<usize>) {
        let list_h = (page_height - HISTORY_CHROME_H).max(HISTORY_LIST_MIN_H);
        let rows = &self.history_cache.rows;
        let selected = &mut self.history_selected;
        let mut do_copy: Option<usize> = None;
        let mut do_replay: Option<usize> = None;
        egui::ScrollArea::vertical()
            .id_salt("history_rows")
            .max_height(list_h)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (idx, entry) in rows {
                    let action = history_row(ui, entry, selected.contains(&entry.id));
                    if action.toggle && !selected.remove(&entry.id) {
                        selected.insert(entry.id);
                    }
                    if action.copy {
                        do_copy = Some(*idx);
                    }
                    if action.replay {
                        do_replay = Some(*idx);
                    }
                }
            });
        (do_copy, do_replay)
    }

    /// What to show in place of the list, if anything: nothing matching the
    /// filter, or nothing recorded at all.
    fn history_notice(&self) -> Option<&'static str> {
        if self.history_cache.history_empty {
            return Some("No dictations yet.");
        }
        self.history_cache.rows.is_empty().then_some("No matches.")
    }

    /// Copy one dictation, picked by its row's icon button.
    fn copy_history_entry(&mut self, idx: usize) {
        let text = self.app.history.lock().get(idx).map(|e| e.text.clone());
        let Some(text) = text else {
            return;
        };
        match crate::output::copy_to_clipboard(&text) {
            Ok(()) => self.status = "Copied to clipboard.".into(),
            Err(e) => self.status = format!("Copy failed: {e}"),
        }
    }

    /// The filter box on the left; on the right, "Copy selected (N)" and the
    /// Select all / Clear helpers. Returns whether Copy selected was clicked;
    /// the two helpers act on `history_selected` directly.
    fn history_toolbar(&mut self, ui: &mut egui::Ui) -> bool {
        let selected_count = self.history_selected.len();
        let visible_ids: Vec<u64> = self.history_cache.rows.iter().map(|(_, e)| e.id).collect();
        let mut copy_selected = false;
        ui.horizontal(|ui| {
            ui.add(
                styled_input(&mut self.history_filter)
                    .hint_text("Filter\u{2026}")
                    .desired_width(200.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(selected_count > 0, |ui| {
                    if accent_button(ui, &format!("Copy selected ({selected_count})"))
                        .on_hover_text(
                            "Copy every ticked dictation to the clipboard, oldest first, \
                             one paragraph each.",
                        )
                        .clicked()
                    {
                        copy_selected = true;
                    }
                });
                if selected_count > 0
                    && ui
                        .small_button("Clear")
                        .on_hover_text("Untick everything.")
                        .clicked()
                {
                    self.history_selected.clear();
                }
                if !visible_ids.is_empty()
                    && ui
                        .small_button("Select all")
                        .on_hover_text("Tick every dictation the filter is showing.")
                        .clicked()
                {
                    self.history_selected.extend(visible_ids.iter().copied());
                }
            });
        });
        ui.add_space(6.0);
        copy_selected
    }

    /// Re-lock and re-filter only when the history or the filter text
    /// actually moved since the last frame (see `HistoryCache`) --
    /// `history_card` renders every frame, and cloning up to `HISTORY_CAP`
    /// full transcript strings on every one of them for an unchanging list is
    /// pure waste. A rebuild also drops ticks on entries that have since
    /// fallen off the end of the history.
    fn rebuild_history_cache_if_stale(&mut self) {
        let current_version = self.app.history.lock().version();
        if !history_cache_stale(
            self.history_cache.version,
            current_version,
            &self.history_cache.filter,
            &self.history_filter,
        ) {
            return;
        }
        let entries = self.app.history.lock().snapshot();
        self.history_cache.history_empty = entries.is_empty();
        self.history_selected
            .retain(|id| entries.iter().any(|e| e.id == *id));
        let filter = self.history_filter.clone();
        self.history_cache.rows = entries
            .into_iter()
            .enumerate()
            .filter(|(_, e)| history_matches(&filter, &e.text))
            .collect();
        self.history_cache.version = current_version;
        self.history_cache.filter = filter;
    }

    /// Copy every ticked dictation to the clipboard as one text: oldest
    /// first, so it reads as a transcript, with a blank line between entries
    /// so each stays its own paragraph when pasted -- the same shape the
    /// tray's "Copy all" produces. Ticks hidden by the current filter still
    /// count: they are what the user chose, and the button's number said so.
    fn copy_selected_history(&mut self) {
        let snapshot = self.app.history.lock().snapshot();
        let picked: Vec<String> = snapshot
            .into_iter()
            .rev()
            .filter(|e| self.history_selected.contains(&e.id))
            .map(|e| e.text)
            .collect();
        let n = picked.len();
        if n == 0 {
            return;
        }
        match crate::output::copy_to_clipboard(&picked.join("\n\n")) {
            Ok(()) => {
                self.status = format!(
                    "Copied {n} dictation{} to the clipboard.",
                    if n == 1 { "" } else { "s" }
                );
            }
            Err(e) => self.status = format!("Copy failed: {e}"),
        }
    }
}
