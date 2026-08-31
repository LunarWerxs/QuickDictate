//! The banners pinned above the cards: first-run onboarding, an available
//! update, and the one-time sign-in nudge.

use super::*;

impl super::SettingsApp {
    /// First-run onboarding banner, pinned above the provider card while *no*
    /// provider has any key. QuickDictate is unusable until a key is added, so
    /// when we auto-open Settings at launch (see `main`) this makes the very
    /// first action obvious instead of leaving the user to guess. It reads the
    /// live draft, so it vanishes the instant a key is saved into any provider.
    pub(crate) fn onboarding_banner(&mut self, ui: &mut egui::Ui) {
        if self.draft.stt_provider.eq_ignore_ascii_case("local")
            || !self.draft.providers_with_keys().is_empty()
        {
            return;
        }
        let acc = accent();
        egui::Frame::new()
            .fill(acc.gamma_multiply(0.16))
            .stroke(Stroke::new(1.0, acc.gamma_multiply(0.55)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("Add an API key to get started")
                        .font(semibold(15.0))
                        .color(text()),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "QuickDictate is bring-your-own-key. Pick a provider below, then \
                         \"Manage keys\u{2026}\" to paste a key from any one of them \
                         (ElevenLabs, Deepgram, OpenAI, AssemblyAI, DashScope, or Google). \
                         Hit Save & Restart when you're done. Free tiers/trials exist for \
                         several providers — signup links are in the README.",
                    )
                    .size(12.5)
                    .color(muted()),
                );
                ui.add_space(8.0);
                if accent_button(ui, "Manage keys\u{2026}").clicked() {
                    self.open_keys_modal(KEYS_TARGET_PROVIDER);
                }
            });
        ui.add_space(10.0);
    }
    /// A newer release the daily auto-check found but hasn't installed (see
    /// `update::pending_update`) — surfaced here too, not just the tray
    /// tooltip, since Settings is where most people go looking. Installing
    /// itself still only happens from the About window's pill, matching the
    /// click-to-consent model everywhere else in the app.
    pub(crate) fn update_available_banner(&mut self, ui: &mut egui::Ui) {
        let Some(tag) = crate::update::pending_update() else {
            return;
        };
        egui::Frame::new()
            .fill(good().gamma_multiply(0.14))
            .stroke(Stroke::new(1.0, good().gamma_multiply(0.5)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("Update available: v{tag}"))
                            .font(semibold(14.0))
                            .color(text()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if accent_button(ui, "Review\u{2026}").clicked() {
                            crate::about::show_about();
                        }
                    });
                });
            });
        ui.add_space(10.0);
    }
    /// The "you could be signed in" banner.
    ///
    /// Sits with the other two banners — above the page header, outside the scroll area — because
    /// it is true regardless of which page you are on, and because the thing it points at (the
    /// Settings sync card) is the LAST card on the Application page and therefore below the fold
    /// on a default window. That gap is the entire reason this exists: the offer is already in the
    /// app, and almost nobody scrolls far enough to find out.
    ///
    /// Three deliberate restraints, all of which the shared engine enforces and this only renders:
    ///
    ///   * **It is not a modal.** No overlay, no focus steal, no Escape to trap. It is a strip at
    ///     the top of a window the user opened on purpose, and everything behind it stays usable.
    ///   * **"Never" is offered on the first ask**, not withheld until the third. An opt-out you
    ///     have to earn is not an opt-out.
    ///   * **Nothing here asks for money.** The account is free and QuickDictate already signs
    ///     into it; the whole pitch is that it exists.
    pub(crate) fn sign_in_nudge_banner(&mut self, ui: &mut egui::Ui) {
        let Some(ask) = self.nudge_ask.clone() else {
            return;
        };
        // Answer collected inside the closure and acted on after it, so the borrow of `self` that
        // the frame holds is already released when we mutate `nudge_ask` and touch the engine.
        let mut answer: Option<crate::nudge_engine::Outcome> = None;
        let mut connect = false;

        egui::Frame::new()
            .fill(accent().gamma_multiply(0.12))
            .stroke(Stroke::new(1.0, accent().gamma_multiply(0.45)))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // Text on its own rows, buttons on a row of their own beneath.
                //
                // The obvious layout — text left, buttons right, one row — does not survive
                // contact: four controls need roughly 300pt, the copy is a full sentence, and
                // egui's horizontal layout does not reserve space for what comes after, so the
                // body simply runs underneath the buttons. Reserving a fixed width for them only
                // moves the failure to whichever window size the guess is wrong at. Stacking is
                // correct at every width, which matters here because this window is resizable and
                // auto-fits its content.
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(&ask.headline)
                                .font(semibold(14.0))
                                .color(text()),
                        );
                        // The × belongs in the corner, not in the button row. Beside "Not now" it
                        // reads as a fourth choice, when it is really the same "no" the whole
                        // strip can be closed with — and the two mean different things to the
                        // engine (see the comment on the buttons below).
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button(RichText::new("\u{00D7}").size(14.0).color(muted()))
                                .on_hover_text("Dismiss")
                                .clicked()
                            {
                                answer = Some(crate::nudge_engine::Outcome::Declined);
                            }
                        });
                    });
                    ui.add_space(2.0);
                    ui.label(RichText::new(&ask.body).size(12.0).color(muted()));
                    ui.add_space(8.0);
                    // The `horizontal` wrapper is load-bearing, not decoration. A bare
                    // `with_layout(right_to_left)` inside a vertical claims ALL the remaining
                    // height, which made this banner swallow the entire settings page and pinned
                    // its buttons to the bottom of the window. `horizontal` constrains it to one
                    // row's height, which is what a button row is.
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if accent_button(ui, &ask.action_label)
                                .on_hover_text(
                                    "Opens your browser to sign in, then syncs these settings to \
                                 your Connections account.",
                                )
                                .clicked()
                            {
                                answer = Some(crate::nudge_engine::Outcome::Accepted);
                                connect = true;
                            }
                            // Same answers, same words, as the web banner every other LunarWerx app
                            // shows (`nudge-banner.ts`). "Not now" and the × are the same thing -
                            // a dismissal worth one interval - and there is deliberately no
                            // permanent opt-out: the engine has no state that could express one.
                            // See `nudge_engine.rs`'s header for the decision and what it costs.
                            if ui
                                .button(RichText::new("Not now").size(12.0).color(muted()))
                                .clicked()
                            {
                                answer = Some(crate::nudge_engine::Outcome::Snoozed);
                            }
                            // The month-long dismissal only exists from the fourth ask on, and the
                            // ENGINE decides that, never a count re-derived here.
                            if ask.can_snooze_month
                                && ui
                                    .button(
                                        RichText::new("Remind me in a month")
                                            .size(12.0)
                                            .color(muted()),
                                    )
                                    .on_hover_text(
                                        "Hides this for a month. Settings sync stays available on \
                                 this page in the meantime.",
                                    )
                                    .clicked()
                            {
                                answer = Some(crate::nudge_engine::Outcome::SetCadence(
                                    crate::nudge_engine::Cadence::Monthly,
                                ));
                            }
                        });
                    });
                });
            });
        ui.add_space(10.0);

        if let Some(outcome) = answer {
            crate::nudge::record(outcome);
            self.nudge_ask = None;
            if connect {
                // Start the app's OWN sign-in rather than sending them to a web page and hoping
                // they come back and find the sync card. The offer is already built; the prompt's
                // only job was to say so. `begin_sign_in` is the exact path the Settings sync
                // button runs, so this app has one sign-in flow, not two that can drift.
                let ctx = ui.ctx().clone();
                self.begin_sign_in(&ctx);
                self.tab = super::nav::Tab::Application;
                self.status =
                    "Finish signing in with Connections in your browser\u{2026}".to_string();
            }
        }
    }
}
