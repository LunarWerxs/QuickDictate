//! The headless `QUICKDICTATE_UI_SHOT` capture hook used by UI tests.

use crate::settings_ui::*;

impl SettingsApp {
    fn apply_shot_mode(&mut self, mode: &str) {
        match mode {
            "keys" | "keys-test" => self.open_keys_modal(KEYS_TARGET_PROVIDER),
            // Proves the one editor really does target two different pools.
            "keys-polish" | "keys-polish-test" => self.open_keys_modal(KEYS_TARGET_POLISH),
            "keys-bulk" => {
                self.open_keys_modal(KEYS_TARGET_PROVIDER);
                if let Some(Modal::Keys(state)) = &mut self.modal {
                    state.bulk = true;
                }
            }
            "replacements" => self.open_replacements_modal(),
            "replacements-bulk" => {
                self.open_replacements_modal();
                if let Some(Modal::Replacements(state)) = &mut self.modal {
                    state.bulk_text = replacements_to_text(&state.rows);
                    state.bulk = true;
                }
            }
            "stats" => self.modal = Some(Modal::Stats),
            // The sign-in banner, which is otherwise unshootable: it fires on a real save,
            // and only for someone a week in with several sessions behind them, so nothing a
            // headless capture does on a fresh profile would ever produce it.
            //
            // This asks the REAL engine, exactly as `save_and_sync` does. It does not force a
            // banner into existence — with a fresh state file the gate is shut and this
            // returns `None`, and the capture then honestly shows no banner. What makes the
            // shot possible is seeding `quickdictate-nudge.json` with the history of a
            // long-time user; the calendar is the only thing the harness fakes.
            "nudge" => self.nudge_ask = crate::nudge::consider("settings-changed"),
            _ => {}
        }
    }
    pub(crate) fn screenshot_hook(&mut self, ctx: &egui::Context) {
        let Some(path) = self.shot_path.clone() else {
            return;
        };
        self.frames += 1;
        let mode = std::env::var("QUICKDICTATE_UI_OPEN").unwrap_or_default();
        // Which nav page to capture. Without this every shot would show the
        // page the rail opens on, so a change to any other page could not be
        // verified headlessly at all.
        if let Ok(want) = std::env::var("QUICKDICTATE_UI_TAB") {
            if let Some(tab) = nav::TABS.iter().find(|t| {
                t.label()
                    .to_ascii_lowercase()
                    .starts_with(&want.to_ascii_lowercase())
            }) {
                self.tab = *tab;
            }
        }
        // Let fonts/layout settle, optionally auto-open a modal for the shot.
        if self.frames == 5 {
            self.apply_shot_mode(&mode);
        }
        // keys-test: also press "Test all" and shoot once the (parallel)
        // verdicts are in — a headless end-to-end test of the probe pipeline.
        if mode.ends_with("-test") && self.frames == 20 {
            let keys = self.active_keys();
            self.start_key_test(ctx, keys);
        }
        let ready = if mode.ends_with("-test") {
            self.frames > 25 && self.test_rx.is_none() && !self.verdicts.is_empty()
        } else {
            self.frames == 14
        };
        if ready && !self.shot_requested {
            self.shot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = image {
            let (w, h) = (img.size[0] as u32, img.size[1] as u32);
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|p| p.to_array()).collect();
            if let Some(buf) = image::RgbaImage::from_raw(w, h, bytes) {
                // Write atomically (tmp + rename) so a watcher never observes a
                // half-written / zero-byte file mid-encode. Force PNG — the
                // ".tmp" extension would otherwise defeat format-from-extension.
                let tmp = format!("{path}.tmp");
                match buf.save_with_format(&tmp, image::ImageFormat::Png) {
                    Ok(()) => {
                        let _ = std::fs::rename(&tmp, &path);
                        tracing::info!("settings ui screenshot -> {path}");
                    }
                    Err(e) => tracing::error!("screenshot save failed: {e}"),
                }
            }
        }
        ctx.request_repaint(); // keep frames flowing while the hook is armed
    }
}
