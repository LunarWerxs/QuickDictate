//! The application card: the everyday behaviour toggles and the AI-cleanup
//! setup. The switches that are set once and then forgotten -- diagnostics,
//! the data folder, per-app profiles -- live on the Advanced page
//! (`advanced.rs`) so this page stays short.

use super::*;

impl super::SettingsApp {
    pub(crate) fn application_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            // Titled: this card shares the Application page with the provider
            // card above it and the sync card below, so it needs a heading to
            // stand apart from them.
            section_title(ui, "\u{E8AB}", "Behavior");
            self.application_toggles(ui);

            // ---- AI cleanup setup ---------------------------------------
            // Only shown once the box above is ticked: with it off this is
            // noise, and with it on the missing key is the single thing most
            // likely to make the feature look broken.
            if self.draft.polish_enabled {
                self.polish_setup_section(ui);
            }
        });
    }

    /// The everyday behaviour toggles, split across two columns. Short labels
    /// with the detail in their hover tooltips.
    fn application_toggles(&mut self, ui: &mut egui::Ui) {
        ui.columns(2, |cols| {
            let left = &mut cols[0];
            blue_check(left, &mut self.draft.run_at_startup, "Start with Windows")
                .on_hover_text("Launch QuickDictate automatically when you sign in to Windows.");
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
        });
    }

    /// AI-cleanup setup: the key-count status line, the "Manage keys…" /
    /// model row, and the free-key hint.
    fn polish_setup_section(&mut self, ui: &mut egui::Ui) {
        subsection_start(ui);
        ui.horizontal(|ui| {
            ui.label(subsection_title("AI cleanup"));
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
}
