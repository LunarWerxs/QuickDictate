//! Testing provider API keys — which list is under test, the parallel probe
//! run, and draining its verdicts.

use crate::settings_ui::*;

impl SettingsApp {
    /// The key list the key manager is currently pointed at: the selected STT
    /// provider normally, or the cleanup pass's own pool when it was opened
    /// from there. See `SettingsApp::keys_target`.
    pub(crate) fn active_keys(&mut self) -> Vec<String> {
        let id = self.keys_target.clone();
        keys_of(&mut self.draft, &id).clone()
    }
    /// Kick off PARALLEL probes for `keys`; verdicts stream back into
    /// `test_rx` and are drained in `update`.
    pub(crate) fn start_key_test(&mut self, ctx: &egui::Context, keys: Vec<String>) {
        if keys.is_empty() || self.test_rx.is_some() {
            return;
        }
        let mut cfg = self.draft.clone();
        cfg.stt_provider = self.draft.stt_provider.clone();
        let (tx, rx) = mpsc::channel();
        self.test_rx = Some(rx);
        self.testing_left = keys.len();
        if let Some(Modal::Keys(state)) = &mut self.modal {
            for r in state.rows.iter_mut() {
                if keys.contains(&r.value) {
                    r.verdict = Verdict::Testing;
                }
            }
        }
        let repaint = ctx.clone();
        let report = Arc::new(move |key, ok| {
            let _ = tx.send((key, ok));
            repaint.request_repaint();
        });
        // The cleanup keys authenticate against `polish_endpoint`, not against
        // the speech provider, so they need their own probe. Same button, same
        // verdict channel, different API.
        if self.keys_target == KEYS_TARGET_POLISH {
            let settings = crate::polish::PolishSettings {
                endpoint: cfg.polish_endpoint.clone(),
                model: cfg.polish_model.clone(),
                keys: keys.clone(),
                deadline: std::time::Duration::from_millis(cfg.polish_deadline_ms),
            };
            crate::polish::spawn_key_test(&self.app, settings, keys, report);
        } else {
            crate::stt::spawn_key_test(&self.app, cfg, keys, report);
        }
    }
    pub(crate) fn drain_verdicts(&mut self) {
        let mut done = Vec::new();
        if let Some(rx) = &self.test_rx {
            while let Ok(v) = rx.try_recv() {
                done.push(v);
            }
        }
        for (key, ok) in done {
            self.testing_left = self.testing_left.saturating_sub(1);
            self.verdicts.retain(|(k, _)| *k != key);
            self.verdicts.push((key.clone(), ok));
            if let Some(Modal::Keys(state)) = &mut self.modal {
                if let Some(r) = state.rows.iter_mut().find(|r| r.value == key) {
                    r.verdict = if ok { Verdict::Ok } else { Verdict::Fail };
                }
            }
        }
        if self.testing_left == 0 {
            self.test_rx = None;
        }
    }
    // Which auto-opened modal (if any) a headless shot should land on, keyed by
    // `QUICKDICTATE_UI_OPEN`. Split out of `screenshot_hook` so the frame-timing
    // logic there isn't buried under this dispatch's own branching.
}
