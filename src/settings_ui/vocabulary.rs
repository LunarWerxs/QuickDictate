//! The Vocabulary page: the biasing terms sent to the provider with every
//! session. It used to be a four-row box at the bottom of the Dictation
//! page; a list of names and jargon is exactly the thing that outgrows four
//! rows, so it has a page of its own and the editor takes all of it.

use super::*;

/// Everything on the page that is not the editor: the card padding, the
/// blurb, the count line, and their spacing. The editor gets the rest.
const VOCABULARY_CHROME_H: f32 = 130.0;

impl super::SettingsApp {
    pub(crate) fn vocabulary_card(&mut self, ui: &mut egui::Ui) {
        let page_height = ui.available_height();
        card(ui, |ui| {
            blurb(
                ui,
                "Words and phrases the provider is told to expect, so names, jargon, and \
                 product names come out right the first time. One term per line. Text \
                 replacements (Dictation page) are the other tool: they repair the text \
                 after recognition. A per-app profile (Advanced page) can override this \
                 list for one app.",
            );
            ui.add_space(6.0);
            let count = match parse_vocabulary(&self.vocabulary_text).len() {
                0 => "No terms yet.".to_string(),
                1 => "1 term \u{2014} sent with your next dictation after Save.".to_string(),
                n => format!("{n} terms \u{2014} sent with your next dictation after Save."),
            };
            blurb(ui, &count);
            ui.add_space(4.0);
            // Fill the page. `desired_rows` is a minimum -- the editor still
            // grows past it with the text, and the page scrolls -- so a long
            // list is never squeezed and a short one still gets a roomy box.
            let row_h = ui.text_style_height(&egui::TextStyle::Body).max(1.0);
            let rows = ((page_height - VOCABULARY_CHROME_H) / row_h)
                .floor()
                .clamp(8.0, 60.0) as usize;
            ui.add(
                egui::TextEdit::multiline(&mut self.vocabulary_text)
                    .desired_width(f32::INFINITY)
                    .desired_rows(rows)
                    .margin(Margin::symmetric(6, CTRL_PAD))
                    .hint_text("Supabase\nCloudflare\nQuickDictate"),
            );
        });
    }
}
