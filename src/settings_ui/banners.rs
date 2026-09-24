//! The banners pinned above the cards: first-run onboarding, an available
//! update, and the one-time sign-in nudge.

use super::*;

/// The tinted, rounded strip every banner sits in, and the gap below it.
/// `fill` and `stroke` are how strongly `tint` shows in each. One definition,
/// so the banners stacked above the page cannot drift apart in shape.
fn banner_strip(
    ui: &mut egui::Ui,
    tint: Color32,
    fill: f32,
    stroke: f32,
    margin: i8,
    add: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::new()
        .fill(tint.gamma_multiply(fill))
        .stroke(Stroke::new(1.0, tint.gamma_multiply(stroke)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(margin))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(10.0);
}

/// Which control on an [`ask_strip`] was clicked this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StripClick {
    /// The corner ×.
    Close,
    NotNow,
    /// The accent button: take the offer.
    Action,
    /// "Remind me in a month", on the strips that offer it.
    Monthly,
}

/// What an [`ask_strip`] says, and which buttons it offers.
struct AskStrip<'a> {
    tint: Color32,
    headline: &'a str,
    body: &'a str,
    action: &'a str,
    action_hover: Option<&'a str>,
    /// The hover text for "Remind me in a month"; `None` leaves that button
    /// out.
    monthly_hover: Option<&'a str>,
}

/// A small muted button, for the quiet ways out of an ask.
fn quiet_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.button(RichText::new(label).size(12.0).color(muted()))
}

/// The strip the crash-report, sign-in and feedback asks share: headline with
/// a corner ×, the body copy, then the button row. Returns what was clicked
/// rather than acting on it: the click is acted on after the frame closes, so
/// nothing mutates `self` from inside it, and each banner decides for itself
/// what its answers mean to its engine.
fn ask_strip(ui: &mut egui::Ui, strip: &AskStrip) -> Option<StripClick> {
    let mut click = None;
    banner_strip(ui, strip.tint, 0.12, 0.45, 12, |ui| {
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
            if ask_strip_header(ui, strip.headline) {
                click = Some(StripClick::Close);
            }
            ui.add_space(2.0);
            ui.label(RichText::new(strip.body).size(12.0).color(muted()));
            ui.add_space(8.0);
            if let Some(button) = ask_strip_buttons(ui, strip) {
                click = Some(button);
            }
        });
    });
    click
}

/// An ask strip's headline, with the dismiss × in its corner. Returns whether
/// the × was clicked.
fn ask_strip_header(ui: &mut egui::Ui, headline: &str) -> bool {
    ui.horizontal(|ui| {
        ui.label(RichText::new(headline).font(semibold(14.0)).color(text()));
        // The × belongs in the corner, not in the button row. Beside "Not now" it
        // reads as a fourth choice, when it is really the same "no" the whole
        // strip can be closed with — and the two can mean different things to an
        // engine (see `nudge_outcome`).
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.button(RichText::new("\u{00D7}").size(14.0).color(muted()))
                .on_hover_text("Dismiss")
                .clicked()
        })
        .inner
    })
    .inner
}

/// An ask strip's right-aligned button row: the accent action, "Not now",
/// and "Remind me in a month" when the strip offers it.
fn ask_strip_buttons(ui: &mut egui::Ui, strip: &AskStrip) -> Option<StripClick> {
    button_row_right(ui, |ui| {
        let mut click = None;
        let mut action = accent_button(ui, strip.action);
        if let Some(tip) = strip.action_hover {
            action = action.on_hover_text(tip);
        }
        if action.clicked() {
            click = Some(StripClick::Action);
        }
        if quiet_button(ui, "Not now").clicked() {
            click = Some(StripClick::NotNow);
        }
        if strip.monthly_hover.is_some_and(|tip| {
            quiet_button(ui, "Remind me in a month")
                .on_hover_text(tip)
                .clicked()
        }) {
            click = Some(StripClick::Monthly);
        }
        click
    })
}

/// Whether the first-run banner applies: a cloud provider is selected and no
/// provider has a key yet. Local needs no key, so it is never nagged about.
fn needs_onboarding(draft: &Config) -> bool {
    !draft.stt_provider.eq_ignore_ascii_case("local") && draft.providers_with_keys().is_empty()
}

/// The sign-in engine's answer for a click on its strip.
///
/// Same answers, same words, as the web banner every other LunarWerx app
/// shows (`nudge-banner.ts`). "Not now" and the × are the same thing - a
/// dismissal worth one interval - and there is deliberately no permanent
/// opt-out: the engine has no state that could express one. See
/// `nudge_engine.rs`'s header for the decision and what it costs.
fn nudge_outcome(click: StripClick) -> crate::nudge_engine::Outcome {
    use crate::nudge_engine::{Cadence, Outcome};
    match click {
        StripClick::Close => Outcome::Declined,
        StripClick::NotNow => Outcome::Snoozed,
        StripClick::Action => Outcome::Accepted,
        StripClick::Monthly => Outcome::SetCadence(Cadence::Monthly),
    }
}

/// The feedback engine's answer for a click on its strip: only the action
/// counts as sharing; every other way out is the same dismissal.
fn survey_outcome(click: StripClick) -> crate::feedback_survey::Outcome {
    use crate::feedback_survey::Outcome;
    match click {
        StripClick::Action => Outcome::Shared,
        StripClick::Close | StripClick::NotNow | StripClick::Monthly => Outcome::Dismissed,
    }
}

impl super::SettingsApp {
    /// First-run onboarding banner, pinned above the provider card while *no*
    /// provider has any key. QuickDictate is unusable until a key is added, so
    /// when we auto-open Settings at launch (see `main`) this makes the very
    /// first action obvious instead of leaving the user to guess. It reads the
    /// live draft, so it vanishes the instant a key is saved into any provider.
    pub(crate) fn onboarding_banner(&mut self, ui: &mut egui::Ui) {
        if !needs_onboarding(&self.draft) {
            return;
        }
        banner_strip(ui, accent(), 0.16, 0.55, 14, |ui| {
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
    }
    /// A newer release the daily auto-check found but hasn't installed (see
    /// `update::pending_update`) — surfaced here too, not just the tray
    /// tooltip, since Settings is where most people go looking. The button
    /// is the consent: it opens the About window and starts the install
    /// there at once, on the same download → verify → swap → relaunch path
    /// the About pill runs, so there is one install flow and not two. It
    /// used to say "Review…" and only open About, which left the user to find
    /// and click the pill a second time.
    pub(crate) fn update_available_banner(&mut self, ui: &mut egui::Ui) {
        let Some(tag) = crate::update::pending_update() else {
            return;
        };
        banner_strip(ui, good(), 0.14, 0.5, 12, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Update available: v{tag}"))
                        .font(semibold(14.0))
                        .color(text()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if accent_button(ui, "Update")
                        .on_hover_text(
                            "Download and install it now. QuickDictate restarts itself \
                             when it's done and brings this window back.",
                        )
                        .clicked()
                    {
                        crate::about::show_about_and_install(tag.clone());
                    }
                });
            });
        });
    }
    /// A fresh `quickdictate-panic.log` entry the previous run left behind (see
    /// `crash_banner::note_launch`, called once at startup) — offers to open the same redacted
    /// preview `error_report_section`'s "Create an error report..." button builds, or dismiss.
    ///
    /// Strictly opt-in like the rest of error reporting: `crash_banner::note_launch` never even
    /// produces an [`crate::crash_banner::Ask`] unless `error_reporting_enabled` was on at
    /// launch, so this banner cannot appear for anyone who hasn't turned that setting on. Nothing
    /// here reads the log itself or sends anything anywhere — it only decides whether to show a
    /// button that opens the same local, redacted preview the settings page already has.
    pub(crate) fn crash_report_banner(&mut self, ui: &mut egui::Ui) {
        let Some(ask) = crate::crash_banner::pending_ask() else {
            return;
        };
        let click = ask_strip(
            ui,
            &AskStrip {
                tint: bad(),
                headline: ask.headline,
                body: ask.body,
                action: "Open report\u{2026}",
                action_hover: None,
                monthly_hover: None,
            },
        );
        let Some(click) = click else {
            return;
        };
        if click == StripClick::Action {
            // WHY force this true: `error_report_section` early-returns (and so never renders
            // the preview we're about to set) whenever `self.draft.error_reporting_enabled` is
            // false. That draft field is this window's *unsaved* copy, so a user who unchecked
            // "Enable local error reports" earlier in this same Settings session -- without
            // saving -- would otherwise click "Open report..." here and see nothing happen. The
            // banner only ever appears because the setting was on at launch, so restoring it in
            // the draft just re-affirms what the user already turned on; Cancel/closing without
            // Save discards it exactly like any other unsaved draft edit.
            self.draft.error_reporting_enabled = true;
            self.error_report_preview = Some(self.build_error_report_text());
            // The preview renders under "Error reporting" on the Advanced page.
            self.tab = nav::Tab::Advanced;
            self.status.clear();
        }
        // Opening the report and dismissing both answer the offer for this launch.
        crate::crash_banner::dismiss();
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
        let click = ask_strip(
            ui,
            &AskStrip {
                tint: accent(),
                headline: &ask.headline,
                body: &ask.body,
                action: &ask.action_label,
                action_hover: Some(
                    "Opens your browser to sign in, then syncs these settings to \
                     your Connections account.",
                ),
                // The month-long dismissal only exists from the fourth ask on, and the
                // ENGINE decides that, never a count re-derived here.
                monthly_hover: ask.can_snooze_month.then_some(
                    "Hides this for a month. Settings sync stays available on \
                     this page in the meantime.",
                ),
            },
        );
        let Some(click) = click else {
            return;
        };
        crate::nudge::record(nudge_outcome(click));
        self.nudge_ask = None;
        if click == StripClick::Action {
            // Start the app's OWN sign-in rather than sending them to a web page and hoping
            // they come back and find the sync card. The offer is already built; the prompt's
            // only job was to say so. `begin_sign_in` is the exact path the Settings sync
            // button runs, so this app has one sign-in flow, not two that can drift.
            let ctx = ui.ctx().clone();
            self.begin_sign_in(&ctx);
            self.tab = super::nav::Tab::Application;
            self.status = "Finish signing in with Connections in your browser\u{2026}".to_string();
        }
    }

    /// The occasional "how's it going?" feedback ask. Same strip, same restraint as
    /// `sign_in_nudge_banner`: not a modal, closable from the same corner ×, and never asked more
    /// than the engine in `feedback_survey` allows. Unlike the sign-in ask there is no month-long
    /// escape hatch to offer, because there is no ladder here to escape - the whole cadence is
    /// already a quarter apart, see that module's doc for why.
    pub(crate) fn feedback_survey_banner(&mut self, ui: &mut egui::Ui) {
        let Some(ask) = self.feedback_ask.clone() else {
            return;
        };
        let click = ask_strip(
            ui,
            &AskStrip {
                tint: good(),
                headline: ask.headline,
                body: ask.body,
                action: ask.action_label,
                action_hover: Some("Opens a new issue on GitHub, pre-filled"),
                monthly_hover: None,
            },
        );
        let Some(click) = click else {
            return;
        };
        crate::feedback_survey::record(survey_outcome(click));
        self.feedback_ask = None;
        if click == StripClick::Action {
            crate::about::open_url(&ask.url);
            self.status = "Thanks \u{2014} opening GitHub in your browser\u{2026}".to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nudge_engine::{Cadence, Outcome};

    #[test]
    fn onboarding_shows_until_any_cloud_provider_has_a_key() {
        let mut draft = Config::default();
        assert!(needs_onboarding(&draft), "a fresh install has no keys");
        draft.deepgram_keys.push("dg-key".into());
        // Any provider's key will do, not only the selected one's: it can be
        // switched to in the same dropdown.
        assert!(!needs_onboarding(&draft));
    }

    #[test]
    fn onboarding_never_nags_the_keyless_local_provider() {
        let mut draft = Config {
            stt_provider: "local".into(),
            ..Config::default()
        };
        assert!(!needs_onboarding(&draft));
        draft.stt_provider = "Local".into();
        assert!(!needs_onboarding(&draft));
    }

    #[test]
    fn sign_in_clicks_map_to_the_engine_answers() {
        assert_eq!(nudge_outcome(StripClick::Action), Outcome::Accepted);
        assert_eq!(nudge_outcome(StripClick::NotNow), Outcome::Snoozed);
        assert_eq!(nudge_outcome(StripClick::Close), Outcome::Declined);
        assert_eq!(
            nudge_outcome(StripClick::Monthly),
            Outcome::SetCadence(Cadence::Monthly)
        );
    }

    #[test]
    fn only_the_feedback_action_counts_as_shared() {
        use crate::feedback_survey::Outcome as Survey;
        assert_eq!(survey_outcome(StripClick::Action), Survey::Shared);
        for click in [StripClick::Close, StripClick::NotNow, StripClick::Monthly] {
            assert_eq!(survey_outcome(click), Survey::Dismissed, "{click:?}");
        }
    }
}
