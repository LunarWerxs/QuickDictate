//! The Licence page, the Personal-or-Business question, and the licence
//! banners above every page. The decisions all live in `crate::licence`; this
//! only renders them and hands clicks back.

use std::sync::mpsc;

use crate::licence::{self, Plan, Posture, RedeemReport};

use super::banners::{banner_strip, quiet_button};
use super::*;

/// The Licence page's own state.
#[derive(Default)]
pub(super) struct LicenceUi {
    /// What is typed in the key field. Never logged; cleared on success.
    pub(super) key_input: String,
    /// The line under the key field after a redeem.
    pub(super) note: String,
    pub(super) is_error: bool,
    /// The redeem in flight, if any.
    pub(super) rx: Option<mpsc::Receiver<RedeemReport>>,
    /// The switch back to free use is asking "are you sure?".
    pub(super) confirm_free: bool,
}

/// Set by [`super::show_settings_on_licence`]; the frame loop moves to the
/// Licence page when it sees it.
pub(super) static OPEN_LICENCE_PAGE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The tint for a posture's chip and banner: green when licensed, the accent
/// while things are fine, red once a licence is needed.
fn posture_tint(p: Posture) -> Color32 {
    match p {
        Posture::Licensed { .. } | Posture::LicensedRenewing { .. } => good(),
        Posture::Personal | Posture::Evaluation { .. } => accent(),
        Posture::EvaluationEnded { .. }
        | Posture::NoLongerActive { .. }
        | Posture::Locked { .. } => bad(),
    }
}

/// Whether a posture wants the loud strip above every page.
fn wants_loud_banner(p: Posture) -> bool {
    matches!(
        p,
        Posture::EvaluationEnded { .. }
            | Posture::NoLongerActive {
                stops_unix: Some(_)
            }
            | Posture::Locked { .. }
    )
}

impl super::SettingsApp {
    /// Drain a finished redeem. Called every frame from `logic`, so a result
    /// that lands while the window is hidden is still recorded.
    pub(super) fn drain_licence(&mut self) {
        let Some(rx) = &self.licence.rx else {
            return;
        };
        let Ok(report) = rx.try_recv() else {
            return;
        };
        self.licence.rx = None;
        let (line, is_error) = licence::format::report_line(&report);
        self.licence.note = line;
        self.licence.is_error = is_error;
        if matches!(
            report,
            RedeemReport::Licensed { .. } | RedeemReport::AcceptedNoCertificate { .. }
        ) {
            self.licence.key_input.clear();
        }
    }

    fn start_redeem(&mut self, ctx: &egui::Context) {
        if self.licence.rx.is_some() {
            return;
        }
        let raw = self.licence.key_input.clone();
        let (tx, rx) = mpsc::channel();
        self.licence.rx = Some(rx);
        self.licence.note.clear();
        self.licence.is_error = false;
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("qd-licence-redeem".into())
            .spawn(move || {
                let _ = tx.send(licence::redeem_entered_key(&raw));
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.licence.rx = None;
            self.licence.note = "Couldn't start the redeem. Try again.".into();
            self.licence.is_error = true;
        }
    }

    /// The Licence page.
    pub(crate) fn licence_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let snap = licence::snapshot();
        let (headline, detail) = licence::format::status_lines(&snap);
        let mut do_redeem = false;
        card(ui, |ui| {
            section_title(ui, "\u{E8D7}", "Licence");
            ui.horizontal(|ui| chip(ui, &headline, posture_tint(snap.posture)));
            ui.add_space(4.0);
            ui.label(RichText::new(detail).size(12.5).color(text()));
            ui.add_space(12.0);

            let licensed = matches!(
                snap.posture,
                Posture::Licensed { .. } | Posture::LicensedRenewing { .. }
            );
            ui.label(
                RichText::new(if licensed {
                    "Enter another key"
                } else {
                    "Licence key"
                })
                .font(semibold(13.0))
                .color(text()),
            );
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let working = self.licence.rx.is_some();
                let field = ui.add_enabled(
                    !working,
                    styled_input(&mut self.licence.key_input)
                        .hint_text("esk_XXXXX-XXXXX-XXXXX-XXXXX")
                        .desired_width(290.0),
                );
                let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let can = !working && !self.licence.key_input.trim().is_empty();
                let clicked = ui
                    .add_enabled(can, egui::Button::new("Redeem"))
                    .on_hover_text("Check this key with Connections and license this copy.")
                    .clicked();
                if can && (clicked || entered) {
                    do_redeem = true;
                }
                if working {
                    ui.add(egui::Spinner::new().size(14.0));
                }
            });
            if !self.licence.note.is_empty() {
                ui.add_space(4.0);
                let col = if self.licence.is_error { bad() } else { good() };
                ui.label(
                    RichText::new(self.licence.note.clone())
                        .size(12.0)
                        .color(col),
                );
            }

            ui.add_space(14.0);
            ui.label(
                RichText::new("Buy a licence for business use")
                    .font(semibold(13.0))
                    .color(text()),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                for plan in [Plan::Perpetual, Plan::Monthly] {
                    let p = licence::product_for(plan);
                    let label = match plan {
                        Plan::Perpetual => "Buy perpetual",
                        Plan::Monthly => "Buy monthly",
                    };
                    if ui
                        .button(format!("{label} \u{00B7} {}", p.price))
                        .on_hover_text(
                            "Opens the checkout in your browser. The key arrives by email.",
                        )
                        .clicked()
                    {
                        licence::open_buy(plan);
                    }
                }
                if ui
                    .link(RichText::new("Manage your licence").size(12.5))
                    .on_hover_text(
                        "Cancel or renew a licence you hold. To cancel or move one to another \
                         PC by email instead, write to lunawerx@gmail.com.",
                    )
                    .clicked()
                {
                    licence::open_manage();
                }
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Personal and nonprofit use is free, always. A licence covers one \
                     installation; paste the key from your purchase email above.",
                )
                .size(11.5)
                .color(muted()),
            );
            self.free_use_switch(ui, snap.posture);
        });
        if do_redeem {
            self.start_redeem(ctx);
        }
    }

    /// The way back from Business for a copy with no licence: a link, then a
    /// plain "are you sure?" naming what free use means, so it stays the same
    /// self-declaration as the first-run answer.
    fn free_use_switch(&mut self, ui: &mut egui::Ui, posture: Posture) {
        let business_unlicensed = matches!(
            posture,
            Posture::Evaluation { .. }
                | Posture::EvaluationEnded { .. }
                | Posture::Locked { .. }
                | Posture::NoLongerActive {
                    stops_unix: Some(_)
                }
        );
        if !business_unlicensed {
            self.licence.confirm_free = false;
            return;
        }
        ui.add_space(8.0);
        if !self.licence.confirm_free {
            if ui
                .link(RichText::new("Not a for-profit business? Switch to free use").size(12.0))
                .clicked()
            {
                self.licence.confirm_free = true;
            }
            return;
        }
        ui.label(
            RichText::new(
                "Switch to free use only if no for-profit business or paid work uses \
                 this copy. Personal use, and use by charities, schools, public \
                 research, public safety, health and environmental organisations \
                 and government, is free.",
            )
            .size(12.0)
            .color(text()),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if accent_button(ui, "Switch to free use").clicked() {
                licence::switch_to_free_use();
                self.licence.confirm_free = false;
            }
            if ui.button("Cancel").clicked() {
                self.licence.confirm_free = false;
            }
        });
    }

    /// The Personal-or-Business question, asked once. Its corner \u{00D7} is
    /// the same answer as closing the window: Personal.
    pub(crate) fn licence_question_banner(&mut self, ui: &mut egui::Ui) {
        if !licence::question_pending() {
            return;
        }
        let mut answer = None;
        banner_strip(ui, accent(), 0.16, 0.55, 14, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Using QuickDictate for a for-profit business?")
                        .font(semibold(15.0))
                        .color(text()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(RichText::new("\u{00D7}").size(14.0).color(muted()))
                        .on_hover_text("Personal or nonprofit")
                        .clicked()
                    {
                        answer = Some(licence::Mode::Personal);
                    }
                });
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "For-profit business use is free for 10 days, then needs a licence. \
                     Personal use, and use by charities, schools, public research, public \
                     safety, health and environmental organisations and government, is \
                     free, always.",
                )
                .size(12.5)
                .color(muted()),
            );
            ui.add_space(8.0);
            button_row_right(ui, |ui| {
                if ui.button("For-profit business or paid work").clicked() {
                    answer = Some(licence::Mode::Business);
                }
                if accent_button(ui, "Personal or nonprofit").clicked() {
                    answer = Some(licence::Mode::Personal);
                }
            });
        });
        if let Some(mode) = answer {
            licence::answer_question(mode);
            if mode == licence::Mode::Business {
                self.tab = nav::Tab::Licence;
            }
        }
    }

    /// The window has really closed (see `hide_window`) with the question
    /// still unanswered: that is Personal, and the question is not asked again.
    pub(super) fn close_licence_question(&mut self) {
        if licence::question_pending() {
            licence::answer_question(licence::Mode::Personal);
        }
    }

    /// The strip above every page for a Business copy: one quiet line during
    /// the evaluation, a clear notice once a licence is needed. Not shown on
    /// the Licence page itself, which already says it.
    pub(crate) fn licence_banner(&mut self, ui: &mut egui::Ui) {
        if self.tab == nav::Tab::Licence {
            return;
        }
        let snap = licence::snapshot();
        let mut go = false;
        if let Posture::Evaluation { ends_unix } = snap.posture {
            let days = licence::posture::days_until(snap.now_unix, ends_unix);
            banner_strip(ui, muted(), 0.08, 0.3, 8, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "Evaluation: {days} {} left",
                            if days == 1 { "day" } else { "days" }
                        ))
                        .size(12.5)
                        .color(muted()),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        go = quiet_button(ui, "Licence").clicked();
                    });
                });
            });
        } else if wants_loud_banner(snap.posture) {
            let (headline, body) =
                licence::format::notice_text(snap.posture, snap.key_last4.as_deref());
            banner_strip(ui, posture_tint(snap.posture), 0.12, 0.45, 12, |ui| {
                ui.label(RichText::new(headline).font(semibold(14.0)).color(text()));
                ui.add_space(2.0);
                ui.label(RichText::new(body).size(12.0).color(muted()));
                ui.add_space(8.0);
                button_row_right(ui, |ui| {
                    if ui.button("Enter key").clicked() {
                        go = true;
                    }
                    if accent_button(ui, "Buy a licence").clicked() {
                        licence::open_buy(Plan::Perpetual);
                    }
                });
            });
        }
        if go {
            self.tab = nav::Tab::Licence;
        }
    }
}
