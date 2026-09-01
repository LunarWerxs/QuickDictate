//! The Connections sync handshake: spawning a worker, draining its events,
//! and reflecting a successful sign-in into the UI and the local config.

use crate::settings_ui::*;

impl SettingsApp {
    /// Spawn a sync worker; its `SyncEvent` result is drained in `update`.
    /// Only one runs at a time (`self.sync.rx`), which serializes the
    /// sign-in / resume / push / disconnect operations.
    pub(crate) fn spawn_sync(
        &mut self,
        ctx: &egui::Context,
        job: impl FnOnce() -> SyncEvent + Send + 'static,
    ) -> bool {
        // One operation at a time: never overwrite a live receiver (that would
        // silently drop the running job's result).
        if self.sync.rx.is_some() {
            return false;
        }
        let (tx, rx) = mpsc::channel();
        self.sync.rx = Some(rx);
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("qd-sync".into())
            .spawn(move || {
                let ev = job();
                let _ = tx.send(ev);
                ctx.request_repaint();
            })
            .ok();
        true
    }
    /// Drain finished sync work and reflect it into the UI + local config.
    pub(crate) fn drain_sync(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if let Some(rx) = &self.sync.rx {
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
        }
        if events.is_empty() {
            return;
        }
        self.sync.rx = None; // one operation per receiver
        for e in events {
            match e {
                SyncEvent::Connected(Ok(c)) => self.apply_connected(ctx, c),
                SyncEvent::Connected(Err(e)) => {
                    // If creds still decrypt we're really signed in; a failed
                    // resume/pull is non-fatal (local settings keep working).
                    self.sync.phase = if crate::sync::is_signed_in() {
                        SyncPhase::SignedIn
                    } else {
                        SyncPhase::SignedOut
                    };
                    self.sync.is_error = true;
                    self.sync.note = format!("Sync problem: {e}");
                }
                SyncEvent::Disconnected => {
                    self.sync.phase = SyncPhase::SignedOut;
                    self.sync.email.clear();
                    self.sync.name.clear();
                    self.sync.avatar = None;
                    self.sync.is_error = false;
                    self.sync.note = "Disconnected. Settings stay on this device.".into();
                }
                SyncEvent::Pushed(Ok(())) => {
                    self.status = "Saved and synced to your Connections account.".into();
                }
                SyncEvent::Pushed(Err(error)) => {
                    self.status = format!("Saved locally, but cloud sync failed: {error}");
                }
            }
        }
    }
    /// Reflect a successful sign-in/resume into the UI + local config. Split
    /// out of `drain_sync` because this one arm (avatar load, pull-vs-seed
    /// note, and the vocabulary-scratch resync) carried most of that match's
    /// nesting on its own.
    fn apply_connected(&mut self, ctx: &egui::Context, c: crate::sync::Connected) {
        self.sync.phase = SyncPhase::SignedIn;
        self.sync.email = c.email.clone();
        self.sync.name = c.name.clone();
        if let Some((w, h, rgba)) = &c.avatar {
            let img = egui::ColorImage::from_rgba_unmultiplied([*w as usize, *h as usize], rgba);
            self.sync.avatar =
                Some(ctx.load_texture("cnx-avatar", img, egui::TextureOptions::LINEAR));
        }
        self.sync.is_error = false;
        // However they got here — the banner, the sync card, or a machine that
        // already had credentials — the sign-in campaign is finished. Retire it so it
        // can never be asked again, including if they later sign out.
        crate::nudge::mark_signed_in();
        self.nudge_ask = None;
        let Some(remote) = &c.remote else {
            self.sync.note = if c.seeded {
                "Synced \u{2014} your settings and stats are now backed up.".into()
            } else {
                "Synced.".into()
            };
            return;
        };
        let config_changed = crate::sync::apply_synced_to_config(&mut self.draft, remote);
        let stats_changed = crate::sync::synced_stats(remote)
            .is_some_and(|stats| self.app.stats.apply_synced(stats));
        if config_changed {
            // The pull mutated `draft.custom_vocabulary` /
            // `draft.profiles[..].custom_vocabulary` directly, bypassing the
            // scratch buffers the vocabulary editors actually render (see
            // `resync_vocabulary_scratch`'s doc comment). Without this, the
            // buffers still hold the pre-pull text: closing untouched shows a
            // false "unsaved changes" prompt (`draft_is_dirty` folds the stale
            // buffer back over `draft` to compare), and clicking Save would
            // fold that stale text back over the just-pulled vocabulary,
            // reverting the cloud value right back.
            self.resync_vocabulary_scratch();
            // Persist + hot-store so the pulled prefs take effect.
            let path = Config::settings_path();
            let _ = self.draft.save(&path);
            self.app.config.store(Arc::new(self.draft.clone()));
        }
        self.sync.note = if config_changed || stats_changed {
            "Updated from your Connections account.".into()
        } else {
            "Synced \u{2014} already up to date.".into()
        };
    }
    /// Start an interactive sign-in (opens the system browser).
    pub(crate) fn begin_sign_in(&mut self, ctx: &egui::Context) {
        if self.sync.rx.is_some() {
            return;
        }
        let snapshot = crate::sync::snapshot_to_synced(&self.draft, &self.app.stats.snapshot());
        self.sync.phase = SyncPhase::SigningIn;
        self.sync.note.clear();
        self.sync.is_error = false;
        self.spawn_sync(ctx, move || {
            SyncEvent::Connected(
                crate::sync::connect_and_reconcile(snapshot).map_err(|e| e.to_string()),
            )
        });
    }
}
