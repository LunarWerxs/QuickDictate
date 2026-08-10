//! Reusable widgets for the Settings window: the blue checkbox, the
//! accent buttons, cards, chips, and the usage-stats tiles and charts.

use super::*;

/// SageThumbs-style checkbox: rounded square, brand-blue fill + white check
/// when on, input-well + hairline border when off. The whole row is clickable.
pub(crate) fn blue_check(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    let box_side = 18.0;
    let gap = 8.0;
    let text_galley = ui.painter().layout_no_wrap(
        label.to_string(),
        egui::TextStyle::Body.resolve(ui.style()),
        text(),
    );
    let desired = egui::vec2(box_side + gap + text_galley.size().x, box_side.max(20.0));
    let (rect, mut resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let box_rect = egui::Rect::from_min_size(
            egui::pos2(rect.min.x, rect.center().y - box_side / 2.0),
            egui::vec2(box_side, box_side),
        );
        let hovered = resp.hovered();
        if *on {
            let fill = if resp.is_pointer_button_down_on() {
                accent_press()
            } else if hovered {
                accent_hot()
            } else {
                accent()
            };
            p.rect_filled(box_rect, CornerRadius::same(5), fill);
            // White check mark.
            let s = box_side;
            let a = box_rect.min + egui::vec2(0.24 * s, 0.52 * s);
            let b = box_rect.min + egui::vec2(0.43 * s, 0.72 * s);
            let d = box_rect.min + egui::vec2(0.78 * s, 0.30 * s);
            let stroke = Stroke::new(2.0, Color32::WHITE);
            p.line_segment([a, b], stroke);
            p.line_segment([b, d], stroke);
        } else {
            p.rect_filled(box_rect, CornerRadius::same(5), input_bg());
            p.rect_stroke(
                box_rect,
                CornerRadius::same(5),
                Stroke::new(1.0, if hovered { accent() } else { border() }),
                egui::StrokeKind::Inside,
            );
        }
        p.galley(
            egui::pos2(
                box_rect.max.x + gap,
                rect.center().y - text_galley.size().y / 2.0,
            ),
            text_galley,
            text(),
        );
    }
    resp
}
/// A plain seconds text box bound to a `u64` **millisecond** config field, with
/// a small "s" unit label beside it. The config stores durations in ms, but
/// users think in seconds, so we present seconds (one decimal) and write the
/// rounded ms value back. Returns the text field's response so the caller can
/// attach a hover tooltip. `id_source` must be unique per field.
///
/// This is a normal input box — no click-and-drag (which felt "dumb" for a
/// value you just want to type). Two immediate-mode wrinkles it handles:
///
/// * **Don't fight the typist.** If we rebuilt the text from `ms` every frame,
///   typing "1." would reformat to "1.0" mid-keystroke and the decimal could
///   never be entered. So while the box is focused we keep the user's raw text
///   in egui memory and leave it alone; we only re-sync the display from `ms`
///   (tidying "2" -> "2.0") once focus leaves.
/// * **Only clamp on an actual edit.** A power user can hand-set a value
///   *outside* `range_secs` in settings.json; merely opening Settings must show
///   it as-is, not silently clamp it back into the draft (which would clobber
///   their choice on the next Save). So we parse + clamp + write only on the
///   frames the text actually changes — matching the old control's `Edits`
///   clamping, not `Always`.
pub(crate) fn secs_input(
    ui: &mut egui::Ui,
    ms: &mut u64,
    range_secs: std::ops::RangeInclusive<f32>,
    id_source: &str,
) -> egui::Response {
    let id = ui.make_persistent_id(("secs_input", id_source));
    let editing = ui.memory(|m| m.focused()) == Some(id);
    // The tidy display value; also the seed for a fresh edit.
    let canonical = format!("{:.1}", *ms as f32 / 1000.0);
    // While editing, preserve the user's in-progress text; otherwise mirror ms.
    let mut buf = if editing {
        ui.memory_mut(|m| m.data.get_temp::<String>(id))
            .unwrap_or_else(|| canonical.clone())
    } else {
        canonical.clone()
    };

    let resp = ui
        .horizontal(|ui| {
            let r = ui.add(styled_input(&mut buf).id(id).desired_width(48.0));
            ui.add_space(2.0);
            ui.weak("s");
            r
        })
        .inner;

    // Commit only when the text actually changed this frame (see doc comment):
    // parse the number, clamp into range, and write the rounded ms back.
    if resp.changed() {
        if let Ok(secs) = buf.trim().parse::<f32>() {
            let clamped = secs.clamp(*range_secs.start(), *range_secs.end());
            *ms = (clamped * 1000.0).round() as u64;
        }
    }

    ui.memory_mut(|m| m.data.insert_temp(id, buf));
    resp
}
/// A single-line text field at the shared control height. Combo boxes and
/// buttons are `row_height + 2*button_padding.y` tall; giving the text well the
/// same vertical margin makes every input, dropdown and button line up.
pub(crate) fn styled_input(value: &mut String) -> egui::TextEdit<'_> {
    let pad = CTRL_PAD;
    egui::TextEdit::singleline(value).margin(Margin::symmetric(6, pad))
}
/// Vertical inner padding shared by text wells and buttons (see `styled_input`
/// and `apply_style`'s `button_padding.y`), so their heights match.
pub(crate) const CTRL_PAD: i8 = 6;
/// Shared height for the Save split button's two halves. Set at/above the Save
/// text button's natural height so BOTH halves clamp to exactly this value —
/// otherwise Save renders at its (taller) natural height while the slim chevron
/// half sits shorter, so the pair looks mismatched. Pins them equal.
pub(crate) const SPLIT_BTN_H: f32 = 30.0;
/// Filled brand-blue primary button.
pub(crate) fn accent_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    accent_button_rounded(ui, label, CornerRadius::same(ROUND), egui::Vec2::ZERO)
}
/// Same as [`accent_button`] but with an explicit corner rounding and minimum
/// size — used to build the Save split button, where the Save segment is
/// rounded only on its left corners (square where it meets the dropdown
/// segment) and pinned to [`SPLIT_BTN_H`] so it matches the chevron half.
pub(crate) fn accent_button_rounded(
    ui: &mut egui::Ui,
    label: &str,
    corner_radius: CornerRadius,
    min_size: egui::Vec2,
) -> egui::Response {
    let btn = egui::Button::new(RichText::new(label).color(Color32::WHITE))
        .fill(accent())
        .corner_radius(corner_radius)
        .min_size(min_size)
        .stroke(Stroke::NONE);
    let resp = ui.add(btn);
    if resp.hovered() {
        let r = resp.rect;
        ui.painter()
            .rect_filled(r, corner_radius, Color32::from_white_alpha(10));
    }
    resp
}
/// A `menu_button` tinted brand-blue so it reads as the dropdown half of a
/// split button sitting next to an [`accent_button`]. The menu popup itself
/// renders in the global menu style (it's a separate `Area`), so this local
/// visuals tweak only colors the trigger, not the items. Returns the trigger's
/// `Response` (for `.on_hover_text`).
///
/// `corner_radius` lets the caller square off the edge that abuts the other
/// half of a split button (see the Save/▾ control), so the pair reads as one
/// unified control with a single outer rounding rather than two separate
/// pill-shaped buttons.
pub(crate) fn accent_menu_button<R>(
    ui: &mut egui::Ui,
    label: RichText,
    corner_radius: CornerRadius,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::Response {
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        for state in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            state.fg_stroke = Stroke::new(1.0, Color32::WHITE);
            state.corner_radius = corner_radius;
            // Drop the inherited hairline border so the accent fill reaches the
            // button edges — matching the Save half (which is drawn with
            // Stroke::NONE). With the border in place the chevron's fill was
            // inset ~1px top and bottom, so it read ~2px shorter than Save.
            state.bg_stroke = Stroke::NONE;
        }
        w.inactive.weak_bg_fill = accent();
        w.hovered.weak_bg_fill = accent_hot();
        w.active.weak_bg_fill = accent_press();
        w.open.weak_bg_fill = accent_press();
        // Narrow the chevron half — the global 12px h-padding is far too wide for
        // a single glyph — and pin its height to SPLIT_BTN_H so it matches the
        // Save half (the icon-font chevron's galley is otherwise taller). The
        // slimmer v-padding leaves the tall glyph room to center within that
        // fixed height rather than forcing the button taller.
        ui.spacing_mut().button_padding = egui::vec2(7.0, 3.0);
        let button = egui::Button::new(label)
            .corner_radius(corner_radius)
            .min_size(egui::vec2(0.0, SPLIT_BTN_H));
        menu::MenuButton::from_button(button).ui(ui, add).0
    })
    .inner
}
/// A little status chip (● label) used for key verdicts.
pub(crate) fn chip(ui: &mut egui::Ui, label: &str, color: Color32) {
    let frame = egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(8, 2));
    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.0, color);
            ui.label(RichText::new(label).size(12.0).color(color));
        });
    });
}
/// A small rounded-square count badge — a bordered chip with a number inside,
/// no fill (so it reads as an outline "pill" rather than a status dot). Used
/// next to a button label to show a live count (e.g. text replacements)
/// without spelling the number out in the label text itself.
pub(crate) fn count_badge(ui: &mut egui::Ui, count: usize) {
    egui::Frame::new()
        .stroke(Stroke::new(1.0, muted()))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(count.to_string()).size(11.5).color(muted()));
        });
}
/// The "Text replacements" button: a normal button frame (matching plain
/// `ui.button` styling) with the live rule count rendered as a [`count_badge`]
/// beside the label instead of spelled out in parentheses. The whole frame is
/// one click target — the badge is purely visual, not a nested widget.
pub(crate) fn text_replacements_button(ui: &mut egui::Ui, count: usize) -> egui::Response {
    let frame = egui::Frame::new()
        .fill(ui.visuals().widgets.inactive.bg_fill)
        .stroke(ui.visuals().widgets.inactive.bg_stroke)
        .corner_radius(ui.visuals().widgets.inactive.corner_radius)
        // Shorter than a full control (CTRL_PAD): this button sits among the
        // checkbox rows, so a tighter vertical pad keeps it compact and aligned.
        .inner_margin(Margin::symmetric(12, 3));
    let mut prepared = frame.begin(ui);
    prepared.content_ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new("Text replacements").color(text()));
        count_badge(ui, count);
    });
    // Allocate first so we know hover/press state, then repaint the frame
    // with the matching widget-state colors before it's actually drawn (the
    // background shape is a placeholder until `paint` fills it in).
    let resp = prepared
        .allocate_space(ui)
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let visuals = if resp.is_pointer_button_down_on() {
        &ui.visuals().widgets.active
    } else if resp.hovered() {
        &ui.visuals().widgets.hovered
    } else {
        &ui.visuals().widgets.inactive
    };
    prepared.frame = prepared
        .frame
        .fill(visuals.bg_fill)
        .stroke(visuals.bg_stroke);
    prepared.paint(ui);
    resp
}
/// Card section: surface fill, hairline border, rounded, padded.
pub(crate) fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(surface())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}
pub(crate) fn grouped_number(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}
pub(crate) fn format_audio_time(audio_ms: u64) -> String {
    let total_seconds = audio_ms / 1_000;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}
pub(crate) fn stat_tile(ui: &mut egui::Ui, label: &str, value: String, detail: &str) {
    egui::Frame::new()
        .fill(input_bg())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(11))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(label).size(11.5).color(muted()));
            ui.label(RichText::new(value).font(semibold(22.0)).color(text()));
            ui.label(RichText::new(detail).size(10.5).color(muted()));
        });
}
pub(crate) fn stats_range_selector(ui: &mut egui::Ui, selected: &mut StatsRange) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        for (range, label) in [
            (StatsRange::Last24Hours, "Last 24 hours"),
            (StatsRange::Last7Days, "Last 7 days"),
            (StatsRange::AllTime, "All time"),
        ] {
            let active = *selected == range;
            let button =
                egui::Button::new(RichText::new(label).font(semibold(11.5)).color(if active {
                    Color32::WHITE
                } else {
                    muted()
                }))
                .fill(if active { accent() } else { input_bg() })
                .stroke(Stroke::new(1.0, if active { accent() } else { border() }))
                .corner_radius(CornerRadius::same(7));
            if ui.add_sized([104.0, 28.0], button).clicked() {
                *selected = range;
            }
        }
    });
}
pub(crate) fn stats_chart(ui: &mut egui::Ui, points: &[u64], caption: &str) {
    egui::Frame::new()
        .fill(input_bg())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(11, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("ACTIVITY")
                        .font(semibold(10.5))
                        .color(muted()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(caption).size(10.5).color(muted()));
                });
            });
            ui.add_space(4.0);
            let chart_width = ui.available_width();
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(chart_width, 54.0), egui::Sense::hover());
            let painter = ui.painter();
            let max = points.iter().copied().max().unwrap_or(0);
            let count = points.len().max(1) as f32;
            let gap = if points.len() > 24 { 2.0 } else { 3.0 };
            let bar_width = ((rect.width() - gap * (count - 1.0)) / count).max(1.0);
            let baseline = rect.bottom();
            painter.line_segment(
                [
                    egui::pos2(rect.left(), baseline),
                    egui::pos2(rect.right(), baseline),
                ],
                Stroke::new(1.0, border()),
            );
            for (index, value) in points.iter().enumerate() {
                let fraction = if max == 0 {
                    0.0
                } else {
                    *value as f32 / max as f32
                };
                let height = if *value == 0 {
                    2.0
                } else {
                    (fraction * (rect.height() - 4.0)).max(5.0)
                };
                let left = rect.left() + index as f32 * (bar_width + gap);
                let bar = egui::Rect::from_min_max(
                    egui::pos2(left, baseline - height),
                    egui::pos2((left + bar_width).min(rect.right()), baseline),
                );
                painter.rect_filled(
                    bar,
                    CornerRadius::same(2),
                    if *value == 0 {
                        border().gamma_multiply(0.65)
                    } else {
                        accent().gamma_multiply(0.88)
                    },
                );
            }
        });
}
pub(crate) fn stats_provider_chart(
    ui: &mut egui::Ui,
    providers: &std::collections::BTreeMap<String, crate::stats::ProviderStats>,
    total_dictations: u64,
) {
    let mut bars = providers
        .iter()
        .map(|(id, totals)| (provider_label(id).to_string(), totals.dictations))
        .collect::<Vec<_>>();
    bars.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    bars.truncate(6);
    if bars.is_empty() && total_dictations > 0 {
        bars.push(("All".into(), total_dictations));
    }

    egui::Frame::new()
        .fill(input_bg())
        .stroke(Stroke::new(1.0, border()))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(11, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("MIX").font(semibold(10.5)).color(muted()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new("All-time dictations by provider")
                            .size(10.5)
                            .color(muted()),
                    );
                });
            });
            ui.add_space(4.0);
            let (rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 62.0), egui::Sense::hover());
            let painter = ui.painter();
            let baseline = rect.bottom() - 15.0;
            painter.line_segment(
                [
                    egui::pos2(rect.left(), baseline),
                    egui::pos2(rect.right(), baseline),
                ],
                Stroke::new(1.0, border()),
            );
            let max = bars.iter().map(|(_, value)| *value).max().unwrap_or(0);
            let slot_width = rect.width() / bars.len().max(1) as f32;
            let bar_width = (slot_width * 0.38).clamp(18.0, 42.0);
            for (index, (label, value)) in bars.iter().enumerate() {
                let center = rect.left() + slot_width * (index as f32 + 0.5);
                let fraction = if max == 0 {
                    0.0
                } else {
                    *value as f32 / max as f32
                };
                let height = (fraction * 38.0).max(4.0);
                let bar = egui::Rect::from_min_max(
                    egui::pos2(center - bar_width / 2.0, baseline - height),
                    egui::pos2(center + bar_width / 2.0, baseline),
                );
                painter.rect_filled(bar, CornerRadius::same(3), accent().gamma_multiply(0.88));
                painter.text(
                    egui::pos2(center, bar.top() - 2.0),
                    egui::Align2::CENTER_BOTTOM,
                    grouped_number(*value),
                    semibold(9.0),
                    text(),
                );
                painter.text(
                    egui::pos2(center, baseline + 3.0),
                    egui::Align2::CENTER_TOP,
                    label,
                    semibold(8.5),
                    muted(),
                );
            }
        });
}
/// A section header: a small accent-blue icon glyph followed by the title.
/// `icon` is a Segoe icon-font codepoint (see `apply_fonts`); it's skipped
/// silently on machines where the icon font failed to load.
pub(crate) fn section_title(ui: &mut egui::Ui, icon: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if icons_available() {
            ui.label(RichText::new(icon).font(icon_font(16.0)).color(accent()));
        }
        ui.label(RichText::new(title).font(semibold(15.0)).color(text()));
    });
    ui.add_space(2.0);
}
