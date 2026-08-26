//! Settings-window state: construction, validation, saving, the
//! Connections sync handshake, hotkey capture, and the key-test run.
//! Nothing here draws; the drawing lives in the sibling card modules.

use super::*;

impl super::SettingsApp {
    pub(crate) fn new(app: Arc<App>) -> Self {
        let draft = (*app.config.load_full()).clone();
        // Seed the sync control from any DPAPI-sealed creds already on disk.
        let creds = crate::sync::load_creds();
        let signed_in = creds.is_some();
        let (email, name) = creds.map(|c| (c.email, c.name)).unwrap_or_default();
        let sync = SyncUi {
            phase: if signed_in {
                SyncPhase::SignedIn
            } else {
                SyncPhase::SignedOut
            },
            email,
            name,
            avatar: None,
            note: String::new(),
            is_error: false,
            rx: None,
            resume_kicked: false,
        };
        let mut this = Self {
            app,
            draft,
            modal: None,
            recording: None,
            verdicts: Vec::new(),
            test_rx: None,
            testing_left: 0,
            status: String::new(),
            sync,
            stats_range: StatsRange::AllTime,
            stats_reset_confirm: false,
            vocabulary_text: String::new(),
            profile_vocab_text: Vec::new(),
            history_filter: String::new(),
            history_cache: HistoryCache::default(),
            editor_opened_at: None,
            pending_save_kind: None,
            pending_restart: None,
            shot_path: std::env::var("QUICKDICTATE_UI_SHOT").ok(),
            frames: 0,
            shot_requested: false,
            // `QUICKDICTATE_UI_PAGE=dictation` opens straight to that page, so
            // the headless screenshot hook above can capture any page and not
            // just the one the window happens to open on.
            keys_target: KEYS_TARGET_PROVIDER.to_string(),
            tab: match std::env::var("QUICKDICTATE_UI_PAGE")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str()
            {
                "dictation" => nav::Tab::Dictation,
                "history" => nav::Tab::History,
                _ => nav::Tab::Application,
            },
        };
        this.resync_vocabulary_scratch();
        this
    }
    /// Reset the editable draft and transient UI state so a re-opened (was
    /// hidden, now shown) window looks exactly like a fresh open — the same
    /// state [`SettingsApp::new`] builds — rather than showing whatever was left
    /// on screen when the user last closed it. Deliberately drops any unsaved
    /// edits, which matches the previous behavior (a brand-new window per open).
    pub(crate) fn reseed_for_reopen(&mut self) {
        self.draft = (*self.app.config.load_full()).clone();
        self.modal = None;
        self.recording = None;
        self.verdicts.clear();
        self.test_rx = None;
        self.testing_left = 0;
        self.status.clear();
        self.stats_range = StatsRange::AllTime;
        self.stats_reset_confirm = false;
        self.resync_vocabulary_scratch();
        self.history_filter.clear();
        self.history_cache = HistoryCache::default();
        self.editor_opened_at = None;
        self.pending_save_kind = None;
        self.pending_restart = None;

        // Re-seed the sync control from creds on disk and re-arm the one-shot
        // silent resume-pull so a re-open also refreshes from the cloud.
        let creds = crate::sync::load_creds();
        self.sync.phase = if creds.is_some() {
            SyncPhase::SignedIn
        } else {
            SyncPhase::SignedOut
        };
        let (email, name) = creds.map(|c| (c.email, c.name)).unwrap_or_default();
        self.sync.email = email;
        self.sync.name = name;
        self.sync.avatar = None;
        self.sync.note.clear();
        self.sync.is_error = false;
        self.sync.rx = None;
        self.sync.resume_kicked = false;
    }
    /// Rebuild the vocabulary text-editor scratch buffers (global + one per
    /// profile) from `self.draft`. Called whenever `draft` is replaced
    /// wholesale — window open/reopen, a defaults reset, or a reload-from-disk
    /// — so the multiline editors show what's actually in the draft instead
    /// of stale text left over from before.
    pub(crate) fn resync_vocabulary_scratch(&mut self) {
        self.vocabulary_text = self.draft.custom_vocabulary.join("\n");
        self.profile_vocab_text = self
            .draft
            .profiles
            .iter()
            .map(|p| p.custom_vocabulary.clone().unwrap_or_default().join("\n"))
            .collect();
    }
    /// Materialize the vocabulary scratch buffers into `self.draft`, trimming
    /// blank lines (see `parse_vocabulary`). Called right before every save —
    /// and by `draft_is_dirty`, which needs to know what a save would
    /// actually write — so the multiline editors don't need to stay in sync
    /// with `draft` on every keystroke.
    pub(crate) fn fold_vocabulary_into_draft(&mut self) {
        self.draft.custom_vocabulary = parse_vocabulary(&self.vocabulary_text);
        for (p, buf) in self
            .draft
            .profiles
            .iter_mut()
            .zip(self.profile_vocab_text.iter())
        {
            if p.custom_vocabulary.is_some() {
                p.custom_vocabulary = Some(parse_vocabulary(buf));
            }
        }
    }
    /// Whether `self.draft` (plus whatever the vocabulary editors currently
    /// hold) differs from what's actually saved on disk right now. Backs the
    /// "unsaved changes" close-confirm (`Modal::UnsavedChanges`) so an X-click
    /// after real edits asks first, but a no-op open-then-close (or a window
    /// left open with nothing touched) closes exactly like before, silently.
    pub(crate) fn draft_is_dirty(&self) -> bool {
        let saved = self.app.config.load_full();
        let mut snapshot = self.draft.clone();
        snapshot.custom_vocabulary = parse_vocabulary(&self.vocabulary_text);
        for (p, buf) in snapshot
            .profiles
            .iter_mut()
            .zip(self.profile_vocab_text.iter())
        {
            if p.custom_vocabulary.is_some() {
                p.custom_vocabulary = Some(parse_vocabulary(buf));
            }
        }
        configs_differ(&snapshot, &saved)
    }
    /// Snapshot settings.json's mtime when "Edit settings.json…" is opened, so
    /// a later Save can tell a hand-edit landed on disk in the meantime (see
    /// `external_change_pending`) instead of silently clobbering it with
    /// whatever's in the draft.
    pub(crate) fn note_editor_opened(&mut self) {
        let path = Config::settings_path();
        self.editor_opened_at = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    }
    /// Whether settings.json has changed on disk since `note_editor_opened`
    /// last ran. `false` when no editor session is being tracked (the common
    /// case) or if the file's mtime can't be read.
    pub(crate) fn external_change_pending(&self) -> bool {
        let Some(opened_at) = self.editor_opened_at else {
            return false;
        };
        let path = Config::settings_path();
        std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .is_ok_and(|mtime| mtime > opened_at)
    }
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
                SyncEvent::Connected(Ok(c)) => {
                    self.sync.phase = SyncPhase::SignedIn;
                    self.sync.email = c.email.clone();
                    self.sync.name = c.name.clone();
                    if let Some((w, h, rgba)) = &c.avatar {
                        let img = egui::ColorImage::from_rgba_unmultiplied(
                            [*w as usize, *h as usize],
                            rgba,
                        );
                        self.sync.avatar =
                            Some(ctx.load_texture("cnx-avatar", img, egui::TextureOptions::LINEAR));
                    }
                    self.sync.is_error = false;
                    if let Some(remote) = &c.remote {
                        let config_changed =
                            crate::sync::apply_synced_to_config(&mut self.draft, remote);
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
                        if config_changed || stats_changed {
                            self.sync.note = "Updated from your Connections account.".into();
                        } else {
                            self.sync.note = "Synced \u{2014} already up to date.".into();
                        }
                    } else if c.seeded {
                        self.sync.note =
                            "Synced \u{2014} your settings and stats are now backed up.".into();
                    } else {
                        self.sync.note = "Synced.".into();
                    }
                }
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
    /// The key list the key manager is currently pointed at: the selected STT
    /// provider normally, or the cleanup pass's own pool when it was opened
    /// from there. See `SettingsApp::keys_target`.
    pub(crate) fn active_keys(&mut self) -> Vec<String> {
        let id = self.keys_target.clone();
        keys_of(&mut self.draft, &id).clone()
    }
    /// While a hotkey field is recording, capture the next real keypress **or
    /// mouse button** into it (Escape cancels). Modifier-only presses are
    /// ignored (egui only fires `Key` events for actual keys, carrying
    /// modifiers alongside).
    ///
    /// Mouse buttons arrive as `PointerButton` rather than `Key`, which is why
    /// they were previously uncapturable: this only ever inspected `Key`
    /// events, so clicking the middle or a thumb button while armed did
    /// nothing at all. Left/right click stay unbindable (see
    /// [`combo_from_pointer`]), which also means the very click that arms
    /// recording can never be recorded as the binding.
    pub(crate) fn capture_hotkey(&mut self, ctx: &egui::Context) {
        let Some(field) = self.recording else {
            // Not armed: make sure any lease from a finished recording is
            // dropped, so mouse hotkeys are live again immediately.
            crate::mouse_hook::end_capture_lease();
            return;
        };
        // Armed: hold the global mouse hook passive. Without this, pressing an
        // ALREADY-BOUND mouse button in order to rebind it would fire the
        // hotkey (and get swallowed before egui ever saw it) instead of being
        // recorded. Renewed every frame, and self-expiring if this window goes
        // away mid-record.
        crate::mouse_hook::capture_lease();
        let captured = ctx.input(|i| {
            for ev in &i.events {
                match ev {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } => {
                        if *key == egui::Key::Escape {
                            return Some(None);
                        }
                        if let Some(combo) = combo_from_event(*key, *modifiers) {
                            return Some(Some(combo));
                        }
                    }
                    egui::Event::PointerButton {
                        button,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        if let Some(combo) = combo_from_pointer(*button, *modifiers) {
                            return Some(Some(combo));
                        }
                    }
                    _ => {}
                }
            }
            None
        });
        match captured {
            Some(Some(combo)) => {
                match field {
                    HotkeyField::Toggle => self.draft.toggle_hotkey = combo,
                    HotkeyField::Hold => self.draft.hold_hotkey = combo,
                }
                self.recording = None;
            }
            Some(None) => self.recording = None, // Escape cancelled
            None => ctx.request_repaint(),       // keep listening
        }
    }
    pub(crate) fn validate(&self) -> Result<(), String> {
        crate::hotkeys::parse_combo(&self.draft.toggle_hotkey)
            .map_err(|e| format!("Toggle hotkey: {e}"))?;
        crate::hotkeys::parse_combo(&self.draft.hold_hotkey)
            .map_err(|e| format!("Hold hotkey: {e}"))?;
        if hotkeys_conflict(&self.draft.toggle_hotkey, &self.draft.hold_hotkey) {
            return Err(format!(
                "Toggle hotkey and Hold hotkey are both set to \"{}\" \u{2014} Windows can only \
                 register one of them, so the other would silently never fire. Pick two \
                 different combinations.",
                self.draft.hold_hotkey.trim()
            ));
        }
        Ok(())
    }
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
        if let Some(Modal::Keys { rows, .. }) = &mut self.modal {
            for r in rows.iter_mut() {
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
            if let Some(Modal::Keys { rows, .. }) = &mut self.modal {
                if let Some(r) = rows.iter_mut().find(|r| r.value == key) {
                    r.verdict = if ok { Verdict::Ok } else { Verdict::Fail };
                }
            }
        }
        if self.testing_left == 0 {
            self.test_rx = None;
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
            match mode.as_str() {
                "keys" | "keys-test" => self.open_keys_modal(KEYS_TARGET_PROVIDER),
                // Proves the one editor really does target two different pools.
                "keys-polish" | "keys-polish-test" => self.open_keys_modal(KEYS_TARGET_POLISH),
                "keys-bulk" => {
                    self.open_keys_modal(KEYS_TARGET_PROVIDER);
                    if let Some(Modal::Keys { bulk, .. }) = &mut self.modal {
                        *bulk = true;
                    }
                }
                "replacements" => self.open_replacements_modal(),
                "replacements-bulk" => {
                    self.open_replacements_modal();
                    if let Some(Modal::Replacements {
                        rows,
                        bulk,
                        bulk_text,
                        ..
                    }) = &mut self.modal
                    {
                        *bulk_text = replacements_to_text(rows);
                        *bulk = true;
                    }
                }
                "stats" => self.modal = Some(Modal::Stats),
                _ => {}
            }
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
