//! The Connections settings-sync card.

use super::*;

impl super::SettingsApp {
    /// Opt-in "Sync settings with Connections" control (spec §6.8): four states
    /// (signed out / signing in / signed in / error) plus a one-line privacy
    /// note. Button clicks are captured into locals and acted on after the card
    /// closure to keep `&mut self` calls out of nested borrows.
    pub(crate) fn sync_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut do_sign_in = false;
        let mut do_disconnect = false;
        card(ui, |ui| {
            // Titled: sync shares the Application page with two other cards,
            // so it needs a heading to stand apart from them.
            section_title(ui, "\u{E895}", "Settings sync");
            let working = self.sync.rx.is_some();
            match self.sync.phase {
                SyncPhase::SignedOut => {
                    do_sign_in = render_sync_signed_out(ui, working);
                }
                SyncPhase::SigningIn => render_sync_signing_in(ui),
                SyncPhase::SignedIn => {
                    do_disconnect = self.render_sync_signed_in(ui, working);
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

    /// The `SignedIn` arm of `sync_card`'s status match: the "Synced" chip,
    /// inline status note, avatar + name, and the Stop syncing button.
    /// Returns whether the user clicked Stop syncing.
    fn render_sync_signed_in(&self, ui: &mut egui::Ui, working: bool) -> bool {
        let mut do_disconnect = false;
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
                ui.label(RichText::new(format!("\u{00b7} {}", self.sync.name)).color(muted()));
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
        do_disconnect
    }
}

/// The `SignedOut` arm of `sync_card`'s status match. Returns whether the
/// user clicked "Sync settings".
fn render_sync_signed_out(ui: &mut egui::Ui, working: bool) -> bool {
    accent_button(ui, "Sync settings")
        .on_hover_text(
            "Sign in with a free Connections account to back up your preferences \
             and numeric stats \u{2014} hotkeys, providers, text replacements \
             (never API keys or transcript text) \
             \u{2014} and sync them to every device you dictate on.",
        )
        .clicked()
        && !working
}

/// The `SigningIn` arm of `sync_card`'s status match.
fn render_sync_signing_in(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.add(egui::Spinner::new().size(14.0));
        ui.label(
            RichText::new("Waiting for sign-in \u{2014} finish in your browser\u{2026}")
                .color(muted()),
        );
    });
}
