//! The left nav rail and the page header, modeled on SageThumbs 2K's v3
//! settings layout: a rail of categories on the left, and a content pane that
//! shows exactly ONE of them at a time.
//!
//! Before this, every card stacked into one scrolling column, which auto-fit
//! the window to roughly 1160 logical points tall. That is taller than a lot
//! of laptop screens and made a small change (flip one checkbox) feel like
//! navigating a document. One category at a time keeps the window around half
//! that height and puts a name on each group of settings.

use super::*;

/// Nav-rail width in egui points. Matches SageThumbs' 188 design px closely
/// enough to feel like the same family without copying its exact metrics.
pub(crate) const NAV_W: f32 = 176.0;
/// Height of one nav row. Tall enough for a comfortable click target.
pub(crate) const NAV_ITEM_H: f32 = 34.0;

/// One page of settings. The order here is the order in the rail, and it runs
/// roughly in the order a new user needs them: pick a provider, tune how
/// dictation behaves, then the app-level and optional extras.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) enum Tab {
    Provider,
    Dictation,
    Application,
    History,
    Sync,
}

pub(crate) const TABS: [Tab; 5] = [
    Tab::Provider,
    Tab::Dictation,
    Tab::Application,
    Tab::History,
    Tab::Sync,
];

impl Tab {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Tab::Provider => "Provider & keys",
            Tab::Dictation => "Dictation",
            Tab::Application => "Application",
            Tab::History => "History",
            Tab::Sync => "Settings sync",
        }
    }

    /// One line under the page title saying what this page is for, so a
    /// category is never just a bare noun.
    pub(crate) fn blurb(self) -> &'static str {
        match self {
            Tab::Provider => "Choose a speech service and manage your API keys.",
            Tab::Dictation => "Hotkeys, timing, and how recognized text is typed.",
            Tab::Application => "Startup, sound, logging, and per-app profiles.",
            Tab::History => "Browse, copy, and re-paste recent transcriptions.",
            Tab::Sync => "Optionally carry your preferences between machines.",
        }
    }

    /// Segoe MDL2 / Fluent glyph for the rail and the page header. Only drawn
    /// when `icons_available()`; without the font these would be tofu, so the
    /// rail falls back to text alone.
    fn glyph(self) -> &'static str {
        match self {
            Tab::Provider => "\u{E8D7}",    // key
            Tab::Dictation => "\u{E720}",   // microphone
            Tab::Application => "\u{E713}", // settings gear
            Tab::History => "\u{E81C}",     // history
            Tab::Sync => "\u{E895}",        // sync
        }
    }
}

impl super::SettingsApp {
    /// Draw the nav rail. Returns nothing; clicking a row sets `self.tab`.
    pub(crate) fn nav_rail(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        for tab in TABS {
            self.nav_item(ui, tab);
        }
    }

    fn nav_item(&mut self, ui: &mut egui::Ui, tab: Tab) {
        let active = self.tab == tab;
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), NAV_ITEM_H),
            egui::Sense::click(),
        );
        if resp.clicked() {
            self.tab = tab;
        }

        let p = ui.painter();
        // Active row: an accent-tinted rounded fill plus a short accent bar on
        // the leading edge. Hover gets the fill only, so the bar always means
        // "this is the page you are on".
        if active || resp.hovered() {
            let tint = if active {
                accent().gamma_multiply(0.22)
            } else {
                surface()
            };
            p.rect_filled(
                rect.shrink2(egui::vec2(4.0, 2.0)),
                CornerRadius::same(6),
                tint,
            );
        }
        if active {
            let bar = egui::Rect::from_min_size(
                egui::pos2(rect.left(), rect.top() + 8.0),
                egui::vec2(3.0, rect.height() - 16.0),
            );
            p.rect_filled(bar, CornerRadius::same(2), accent());
        }

        let fg = if active { accent() } else { text() };
        let mut x = rect.left() + 14.0;
        if icons_available() {
            p.text(
                egui::pos2(x, rect.center().y),
                egui::Align2::LEFT_CENTER,
                tab.glyph(),
                icon_font(15.0),
                fg,
            );
            x += 26.0;
        }
        p.text(
            egui::pos2(x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            tab.label(),
            if active {
                semibold(13.0)
            } else {
                egui::FontId::proportional(13.0)
            },
            if active { text() } else { muted() },
        );
    }

    /// The content pane's page header: the active category's title and blurb,
    /// so the pane is self-describing without the rail.
    pub(crate) fn page_header(&mut self, ui: &mut egui::Ui) {
        let tab = self.tab;
        ui.horizontal(|ui| {
            if icons_available() {
                ui.label(
                    RichText::new(tab.glyph())
                        .font(icon_font(18.0))
                        .color(accent()),
                );
                ui.add_space(4.0);
            }
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(tab.label())
                        .font(semibold(15.0))
                        .color(text()),
                );
                ui.label(RichText::new(tab.blurb()).size(11.5).color(muted()));
            });
        });
        ui.add_space(10.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tab_is_in_the_rail_exactly_once() {
        for tab in TABS {
            assert_eq!(TABS.iter().filter(|t| **t == tab).count(), 1);
        }
        assert_eq!(TABS.len(), 5);
    }

    #[test]
    fn every_tab_has_a_label_and_a_blurb() {
        for tab in TABS {
            assert!(!tab.label().is_empty());
            assert!(!tab.blurb().is_empty(), "{:?} needs a blurb", tab);
            assert!(
                tab.blurb().ends_with('.'),
                "{:?} blurb reads as a sentence",
                tab
            );
            assert!(!tab.glyph().is_empty());
        }
    }

    #[test]
    fn tab_labels_are_distinct() {
        let mut seen: Vec<&str> = TABS.iter().map(|t| t.label()).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "two tabs share a label");
    }
}
