//! The usage-stats modal: the tiles, the per-provider breakdown, the chart
//! caption, and the reset-everything confirm inside it.

use super::modals::{ModalAction, ModalOutcome};
use super::*;

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
pub(super) fn render_stats_modal(
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
