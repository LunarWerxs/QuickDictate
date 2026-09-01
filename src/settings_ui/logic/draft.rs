//! Building the settings window's editable draft, keeping it in step with
//! disk, and deciding whether it holds unsaved work.

use crate::settings_ui::*;

impl SettingsApp {
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
            nudge_ask: None,
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
        // `nudge_ask` is deliberately NOT cleared here. Closing the window is not an answer, and
        // the ask is already stamped in the engine's persisted state either way — so clearing it
        // would hide a prompt the user still owes an answer to while spending it anyway. Left up,
        // a re-open shows the same banner and the user's click still counts.

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
}
