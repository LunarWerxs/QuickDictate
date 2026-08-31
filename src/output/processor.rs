//! What happened to a paste, and the cached text processor that produced it.
//!
//! The processor is rebuilt only when the config or the foreground app
//! changes, so the common case of repeated pastes into one window is free.

use crate::text::TextProcessor;

/// What actually happened to the text.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PasteOutcome {
    /// It was injected into the focused window.
    Typed,
    /// The focused window is at a higher integrity level, so injected input is
    /// silently dropped by UIPI. The text was left on the clipboard instead,
    /// deliberately NOT restoring the previous contents, so the user can paste
    /// it themselves.
    LeftOnClipboard,
}

/// Per-config-snapshot cache of built [`TextProcessor`]s, keyed by which
/// profile (if any) matched -- `None` is the key for "no profile matched /
/// global settings". Avoids rebuilding the replacement regexes on every
/// single paste even once Per-App Profiles are in use.
pub(super) struct ProcessorCache {
    entries: Vec<(Option<String>, TextProcessor)>,
}

impl ProcessorCache {
    pub(super) fn new(cfg: &crate::config::Config) -> Self {
        // Pre-seed the global (no-match) entry -- the overwhelmingly common
        // case when no profile matches the foreground window.
        Self {
            entries: vec![(None, build_processor(cfg, None))],
        }
    }

    pub(super) fn get_or_build(
        &mut self,
        cfg: &crate::config::Config,
        exe_name: Option<&str>,
    ) -> &TextProcessor {
        let key = cfg.active_profile(exe_name).map(|p| p.name.clone());
        if let Some(idx) = self.entries.iter().position(|(k, _)| *k == key) {
            return &self.entries[idx].1;
        }
        tracing::debug!("output: building TextProcessor for profile {:?}", key);
        self.entries
            .push((key.clone(), build_processor(cfg, exe_name)));
        // Index the slot just pushed rather than `.last().unwrap()`: same
        // element, no panic path, and it cannot go stale if this grows a
        // capacity bound later.
        &self.entries[self.entries.len() - 1].1
    }
}

fn build_processor(cfg: &crate::config::Config, exe_name: Option<&str>) -> TextProcessor {
    let effective = cfg.effective_settings(exe_name);
    TextProcessor::new(
        &effective.text_replacements,
        effective.auto_punct,
        effective.auto_space,
        effective.auto_newline,
    )
}

pub(super) fn preview(s: &str) -> String {
    let trimmed: String = s.chars().take(60).collect();
    if s.chars().count() > 60 {
        format!("{trimmed}...")
    } else {
        trimmed
    }
}
