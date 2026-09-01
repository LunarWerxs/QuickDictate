//! Saving: local write, the background push to Connections, reset to
//! defaults, and the deferred save-and-restart.

use crate::settings_ui::*;

impl SettingsApp {
    pub(crate) fn save(&mut self) -> bool {
        self.fold_vocabulary_into_draft();
        if let Err(e) = self.validate() {
            self.status = format!("Not saved — {e}");
            return false;
        }
        let previous = self.app.config.load_full();
        let leaving_local = previous.stt_provider.eq_ignore_ascii_case("local")
            && !self.draft.stt_provider.eq_ignore_ascii_case("local");
        let path = Config::settings_path();
        match self.draft.save(&path) {
            Ok(()) => {
                // Hot-store so per-session settings (paste policy, provider,
                // keys, replacements) apply immediately; hotkeys and logging
                // initialization still need a restart.
                self.app.config.store(Arc::new(self.draft.clone()));
                // The running capture re-resolves its device every couple of
                // seconds, so a microphone change takes effect on its own
                // rather than waiting for a restart.
                crate::audio::set_preferred_input(&self.draft.input_device);
                if leaving_local {
                    crate::local_stt::request_unload();
                } else if self.draft.stt_provider.eq_ignore_ascii_case("local") {
                    crate::local_stt::request_prewarm(&self.draft.local_model);
                }
                crate::autostart::reconcile(self.draft.run_at_startup);
                self.status = "Saved. Hotkey and logging changes apply after restart.".into();
                tracing::info!("settings saved via UI to {}", path.display());
                true
            }
            Err(e) => {
                self.status = format!("Save failed: {e}");
                false
            }
        }
    }
    /// Plain "Save" (bottom bar / dialogs): save locally — fast, so this stays
    /// synchronous and callers can rely on the file being written when it
    /// returns — then, if signed in, push to Connections on a *background*
    /// thread so a slow or dead network never freezes the window (the old
    /// version blocked the egui event-loop thread on `recv_timeout`, which
    /// froze the whole Settings window for up to 6 seconds). The push result
    /// lands later via `SyncEvent::Pushed`, drained by `drain_sync` into
    /// `self.status`. Returns whether the *local* save succeeded.
    pub(crate) fn save_and_sync(&mut self, ctx: &egui::Context) -> bool {
        if !self.save() {
            return false;
        }
        if self.sync.phase == SyncPhase::SignedIn {
            let snapshot = crate::sync::snapshot_to_synced(&self.draft, &self.app.stats.snapshot());
            let spawned = self.spawn_sync(ctx, move || {
                SyncEvent::Pushed(
                    crate::sync::push_now(snapshot)
                        .map(|_| ())
                        .map_err(|e| e.to_string()),
                )
            });
            self.status = if spawned {
                "Saved. Syncing to your Connections account\u{2026}".into()
            } else {
                // Another sync operation (sign-in/resume/disconnect) is
                // already running; the local save already landed, so say so
                // rather than silently dropping this push.
                "Saved locally \u{2014} a sync operation is already in progress.".into()
            };
        }

        // The moment. Someone who has just tuned their setup is exactly who benefits from those
        // settings following them to another machine — and the offer to do that is already in
        // this app, three cards down where it is rarely seen. The engine decides whether this
        // particular save is one to speak up on (almost always: no), and it will not consider at
        // all while signed in, so this cannot fire at someone who already took the offer.
        //
        // Guarded on there being no ask already on screen: a user who saves twice without
        // answering should not stack two banners, and `consider` advances the ladder every time
        // it returns something.
        if self.nudge_ask.is_none() {
            self.nudge_ask = crate::nudge::consider("settings-changed");
        }
        true
    }
    /// "Default settings" (⋯ overflow menu): reset every editable preference
    /// back to [`Config::default`] and persist immediately, refreshing the UI
    /// on the spot.
    ///
    /// Deliberately carries a few fields *forward* rather than blanking them,
    /// because they aren't really "settings" a user means to reset:
    ///   * API keys (`*_keys` / `local_keys`) — QuickDictate is
    ///     bring-your-own-key; wiping these would break dictation entirely
    ///     and force re-onboarding, which isn't what "reset to defaults" implies.
    ///   * `install_id` — a machine identity for update checks, not a
    ///     preference (see `Config::install_id`'s doc comment).
    ///   * `window_width/height/x/y` — machine-local window geometry, same
    ///     category `sync.rs` already excludes from portable settings.
    pub(crate) fn reset_to_defaults(&mut self) {
        let keep = &self.draft;
        self.draft = Config {
            elevenlabs_keys: keep.elevenlabs_keys.clone(),
            deepgram_keys: keep.deepgram_keys.clone(),
            openai_keys: keep.openai_keys.clone(),
            assemblyai_keys: keep.assemblyai_keys.clone(),
            dashscope_keys: keep.dashscope_keys.clone(),
            google_keys: keep.google_keys.clone(),
            local_keys: keep.local_keys.clone(),
            install_id: keep.install_id.clone(),
            window_width: keep.window_width,
            window_height: keep.window_height,
            window_x: keep.window_x,
            window_y: keep.window_y,
            ..Config::default()
        };
        self.recording = None;
        self.resync_vocabulary_scratch();
        if self.save() {
            self.status = "Settings reset to defaults.".into();
        }
    }
    /// "Save and restart" (bottom bar): save locally, then — if signed in —
    /// best-effort push to Connections on a background thread exactly like
    /// `save_and_sync` (see `SyncEvent::Pushed`), so the window never blocks
    /// the frame. The actual relaunch is deferred to `poll_pending_restart`,
    /// which fires once that push lands (or a short deadline passes) — a dead
    /// network delays the restart by at most a few seconds instead of hanging
    /// the window (the old version blocked here with `recv_timeout`).
    pub(crate) fn save_and_restart(&mut self, ctx: &egui::Context) {
        if self.pending_restart.is_some() {
            return; // already mid-restart; ignore a repeat click
        }
        if !self.save() {
            return;
        }
        if crate::sync::is_signed_in() {
            let snapshot = crate::sync::snapshot_to_synced(&self.draft, &self.app.stats.snapshot());
            let spawned = self.spawn_sync(ctx, move || {
                SyncEvent::Pushed(
                    crate::sync::push_now(snapshot)
                        .map(|_| ())
                        .map_err(|e| e.to_string()),
                )
            });
            if spawned {
                self.status = "Syncing before restart\u{2026}".into();
                let timeout = std::time::Duration::from_secs(6);
                self.pending_restart = Some(PendingRestart {
                    deadline: std::time::Instant::now() + timeout,
                });
                // egui frames are event-driven: with no mouse/keyboard input, nothing
                // would otherwise re-render `ui()` and `poll_pending_restart` would
                // never get to notice the deadline passed. Force a frame right at the
                // deadline so the restart still fires even if the user walks away.
                ctx.request_repaint_after(timeout);
                return; // do_relaunch runs from poll_pending_restart once it lands
            }
            // Another sync operation was already in flight; don't hold the
            // restart hostage waiting for it (best-effort, same as before).
        }
        self.do_relaunch();
    }
    /// Poll a pending "Save and restart" once per frame (see
    /// `save_and_restart`): once the background push has been drained by
    /// `drain_sync` (`sync.rx` back to `None`, whether it succeeded or
    /// failed) or the deadline passes, actually relaunch. Called from `ui`
    /// right after `drain_sync` so it sees a push that just landed this frame.
    pub(crate) fn poll_pending_restart(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending_restart.as_ref() else {
            return;
        };
        let deadline = pending.deadline; // copy out; releases the borrow before the mutation below
        let now = std::time::Instant::now();
        if self.sync.rx.is_none() || now >= deadline {
            self.pending_restart = None;
            self.do_relaunch();
        } else {
            // Still waiting on the push with no guarantee another input event
            // triggers a frame before the deadline; keep re-arming a repaint for
            // the remaining time so the deadline itself is what wakes us up.
            ctx.request_repaint_after(deadline - now);
        }
    }
    /// Flush stats, spawn a fresh `--relaunch` process, and hand shutdown off
    /// to it. Split out of `save_and_restart` so its immediate path (not
    /// signed in, or no sync op could be started) and its deferred path
    /// (`poll_pending_restart`) share the same relaunch logic.
    pub(crate) fn do_relaunch(&mut self) {
        self.app.stats.flush();
        let relaunch = std::env::current_exe()
            .map_err(|e| format!("Could not locate QuickDictate: {e}"))
            .and_then(|exe| {
                // `--relaunch` marks this as a deliberate hand-off so the new
                // process takes over the single-instance mutex and reopens
                // Settings after startup (see `single_instance_guard` and
                // `should_open_settings_on_start`).
                std::process::Command::new(exe)
                    .arg("--relaunch")
                    .spawn()
                    .map_err(|e| format!("Could not restart QuickDictate: {e}"))
            });
        if let Err(e) = relaunch {
            self.status = e;
            return;
        }
        self.app.shutdown.store(true, Ordering::Release);
    }
}
