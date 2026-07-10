use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use tokio::runtime::Handle as TokioHandle;

use crate::audio::AudioSource;
use crate::config::Config;

/// Cap on how many past transcriptions [`TranscriptHistory`] keeps. Old
/// enough entries just fall off the back -- there's no persistence, so this
/// only bounds in-memory growth across a long-running session.
const HISTORY_CAP: usize = 50;

/// One past dictation result, kept so it can be re-pasted or browsed from the
/// tray. `text` is the fully processed string (replacements/punctuation/etc.
/// already applied) -- exactly what was last sent to the focused window.
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub text: String,
    pub when: SystemTime,
}

/// Rolling, newest-first log of recent transcriptions. This is the
/// generalized replacement for the old single `last_transcription` slot:
/// `front()` / index `0` is always "the most recent paste", so the existing
/// replay-last behavior falls out of this for free.
#[derive(Default)]
pub struct TranscriptHistory {
    entries: VecDeque<HistoryEntry>,
    /// Bumped on every `push`. Cheap way for the tray's poll loop to notice
    /// "history changed since I last rebuilt the submenu" without hashing or
    /// cloning the whole list every frame.
    version: u64,
}

impl TranscriptHistory {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            version: 0,
        }
    }

    /// Record a newly-pasted transcription at the front, evicting the oldest
    /// entry once we're over [`HISTORY_CAP`].
    pub fn push(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.entries.push_front(HistoryEntry {
            text,
            when: SystemTime::now(),
        });
        self.entries.truncate(HISTORY_CAP);
        self.version = self.version.wrapping_add(1);
    }

    /// Current version counter; changes every time `push` adds an entry.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The most recent entry, if any (index `0`) -- what "replay last paste"
    /// has always meant.
    pub fn most_recent(&self) -> Option<HistoryEntry> {
        self.entries.front().cloned()
    }

    /// Removes and returns the most recent entry, if any. Used by the
    /// "scratch that" voice command after it has successfully backspaced
    /// that entry's text out of the target app -- without this, a second
    /// "scratch that" in a row would see the same (already-undone) entry as
    /// still "most recent" and try to backspace it again.
    pub fn pop_most_recent(&mut self) -> Option<HistoryEntry> {
        let popped = self.entries.pop_front();
        if popped.is_some() {
            self.version = self.version.wrapping_add(1);
        }
        popped
    }

    /// Entry at `index` (0 = most recent), if it exists.
    pub fn get(&self, index: usize) -> Option<HistoryEntry> {
        self.entries.get(index).cloned()
    }

    /// Snapshot of all entries, newest first. Used to render the tray's
    /// "Recent transcriptions" submenu.
    pub fn snapshot(&self) -> Vec<HistoryEntry> {
        self.entries.iter().cloned().collect()
    }
}

#[repr(u8)]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Status {
    /// No active dictation. Pip hidden.
    Idle = 0,
    /// User pressed the hotkey; mic / WS are spinning up. Pip shown in amber.
    /// This is the state we transition to *immediately* on press so the user
    /// gets visual feedback within one UI frame.
    Starting = 1,
    /// Audio is actually being streamed to ElevenLabs. Pip shown in green.
    Listening = 2,
    /// The most recent session failed before audio could stream. Pip shown in red.
    Error = 3,
}

impl Status {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Status::Starting,
            2 => Status::Listening,
            3 => Status::Error,
            _ => Status::Idle,
        }
    }
}

pub struct App {
    pub config: ArcSwap<Config>,
    status: AtomicU8,
    pub shutdown: AtomicBool,
    pub session_epoch: parking_lot::Mutex<u64>,
    pub rt: TokioHandle,
    pub status_tx: crossbeam_channel::Sender<Status>,
    #[allow(dead_code)]
    pub status_rx: crossbeam_channel::Receiver<Status>,
    pub transcript_tx: crossbeam_channel::Sender<String>,
    pub transcript_rx: crossbeam_channel::Receiver<String>,
    /// `None` = replay the most recent transcription (the original
    /// behavior); `Some(i)` = replay history entry `i` (0 = most recent),
    /// which is how the tray's "Recent transcriptions" submenu re-pastes an
    /// older entry.
    pub replay_tx: crossbeam_channel::Sender<Option<usize>>,
    pub replay_rx: crossbeam_channel::Receiver<Option<usize>>,
    pub current_key: Mutex<Option<String>>,
    /// Rolling log of recent transcriptions (newest first), the generalized
    /// replacement for the old single "last transcription" slot.
    pub history: Mutex<TranscriptHistory>,
    /// Live word count of the currently-active session (committed chunks +
    /// the latest partial). The UI overlay reads this on each redraw so the
    /// pip can show how many words have been heard so far. Reset to 0 when
    /// a new session starts.
    pub word_count: AtomicU32,
    /// Global pre-warmed audio source. Sessions subscribe to get an
    /// already-running audio feed instead of opening WASAPI per session.
    pub audio: Arc<AudioSource>,
}

impl App {
    pub fn new(config: Config, rt: TokioHandle, audio: Arc<AudioSource>) -> Arc<Self> {
        let (status_tx, status_rx) = crossbeam_channel::unbounded();
        let (transcript_tx, transcript_rx) = crossbeam_channel::bounded(64);
        let (replay_tx, replay_rx) = crossbeam_channel::bounded(8);
        Arc::new(Self {
            config: ArcSwap::from_pointee(config),
            status: AtomicU8::new(Status::Idle as u8),
            shutdown: AtomicBool::new(false),
            session_epoch: Mutex::new(0),
            rt,
            status_tx,
            status_rx,
            transcript_tx,
            transcript_rx,
            replay_tx,
            replay_rx,
            current_key: Mutex::new(None),
            history: Mutex::new(TranscriptHistory::new()),
            word_count: AtomicU32::new(0),
            audio,
        })
    }

    pub fn status(&self) -> Status {
        Status::from_u8(self.status.load(Ordering::Acquire))
    }

    pub fn set_status(&self, s: Status) {
        let prev = self.status.swap(s as u8, Ordering::AcqRel);
        if prev != s as u8 {
            let _ = self.status_tx.send(s);
        }
    }

    pub fn clear_status_if(&self, current: Status, next: Status) -> bool {
        self.status
            .compare_exchange(
                current as u8,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| {
                let _ = self.status_tx.send(next);
                true
            })
            .unwrap_or(false)
    }

    /// Atomically transition Starting -> Listening. No-op if main has already
    /// cleared the state back to Idle (e.g., user toggled off before the
    /// session became fully ready). Prevents a stale background session from
    /// re-lighting the pip after the user expects it gone.
    pub fn promote_starting_to_listening(&self) -> bool {
        self.status
            .compare_exchange(
                Status::Starting as u8,
                Status::Listening as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| {
                let _ = self.status_tx.send(Status::Listening);
                true
            })
            .unwrap_or(false)
    }

    pub fn next_session_epoch(&self) -> u64 {
        let mut e = self.session_epoch.lock();
        *e = e.wrapping_add(1);
        *e
    }

    pub fn current_session_epoch(&self) -> u64 {
        *self.session_epoch.lock()
    }

    pub fn invalidate_current_session(&self) {
        let mut e = self.session_epoch.lock();
        *e = e.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn most_recent_reflects_the_latest_push() {
        let mut h = TranscriptHistory::new();
        assert!(h.most_recent().is_none());
        h.push("first".into());
        h.push("second".into());
        assert_eq!(h.most_recent().unwrap().text, "second");
        assert_eq!(h.get(1).unwrap().text, "first");
    }

    #[test]
    fn pop_most_recent_removes_and_returns_the_front_entry() {
        let mut h = TranscriptHistory::new();
        h.push("first".into());
        h.push("second".into());
        let before_version = h.version();

        let popped = h.pop_most_recent().unwrap();
        assert_eq!(popped.text, "second");
        assert_eq!(h.most_recent().unwrap().text, "first");
        // Popping bumps the version, same as push does, so pollers (e.g. the
        // tray's history submenu) notice the change.
        assert_ne!(h.version(), before_version);

        // A second pop drains the last entry; a third is a no-op on empty.
        assert_eq!(h.pop_most_recent().unwrap().text, "first");
        assert!(h.pop_most_recent().is_none());
        assert!(h.most_recent().is_none());
    }

    #[test]
    fn pop_most_recent_on_empty_history_is_a_harmless_none() {
        let mut h = TranscriptHistory::new();
        assert!(h.pop_most_recent().is_none());
        assert_eq!(h.version(), 0);
    }
}
