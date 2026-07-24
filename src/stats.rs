//! Persistent, privacy-safe transcription usage statistics.
//!
//! Stats intentionally live outside `settings.json`: Settings keeps an editable
//! snapshot and rewrites that file wholesale, while these counters change after
//! every successful dictation. Keeping a small numeric-only file prevents stale
//! Settings saves (and settings sync) from clobbering live totals.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;

const STATS_FILE: &str = "quickdictate-stats.json";

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProviderStats {
    pub words: u64,
    pub audio_ms: u64,
    pub dictations: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct UsageStats {
    pub schema_version: u32,
    pub tracking_started_unix_secs: u64,
    pub last_dictation_unix_secs: u64,
    pub total_words: u64,
    pub total_audio_ms: u64,
    pub total_dictations: u64,
    pub longest_dictation_words: u64,
    pub longest_dictation_audio_ms: u64,
    pub providers: BTreeMap<String, ProviderStats>,
}

impl Default for UsageStats {
    fn default() -> Self {
        Self {
            schema_version: 1,
            tracking_started_unix_secs: 0,
            last_dictation_unix_secs: 0,
            total_words: 0,
            total_audio_ms: 0,
            total_dictations: 0,
            longest_dictation_words: 0,
            longest_dictation_audio_ms: 0,
            providers: BTreeMap::new(),
        }
    }
}

impl UsageStats {
    fn record(&mut self, provider: &str, words: u64, audio_ms: u64, now: u64) {
        if words == 0 {
            return;
        }
        if self.tracking_started_unix_secs == 0 {
            self.tracking_started_unix_secs = now;
        }
        self.schema_version = 1;
        self.last_dictation_unix_secs = now;
        self.total_words = self.total_words.saturating_add(words);
        self.total_audio_ms = self.total_audio_ms.saturating_add(audio_ms);
        self.total_dictations = self.total_dictations.saturating_add(1);
        self.longest_dictation_words = self.longest_dictation_words.max(words);
        self.longest_dictation_audio_ms = self.longest_dictation_audio_ms.max(audio_ms);

        let provider = provider.trim().to_ascii_lowercase();
        let provider = if provider.is_empty() {
            "unknown".to_string()
        } else {
            provider
        };
        let totals = self.providers.entry(provider).or_default();
        totals.words = totals.words.saturating_add(words);
        totals.audio_ms = totals.audio_ms.saturating_add(audio_ms);
        totals.dictations = totals.dictations.saturating_add(1);
    }
}

/// Thread-safe lifetime counter store owned by [`crate::state::App`].
pub struct StatsStore {
    path: PathBuf,
    inner: Mutex<(UsageStats, u64)>,
    persist_lock: Mutex<()>,
    persisted_revision: AtomicU64,
    pending_writes: Mutex<usize>,
    pending_cv: Condvar,
    active_sessions: Mutex<usize>,
    active_sessions_cv: Condvar,
    writable: bool,
}

impl StatsStore {
    pub fn load() -> Self {
        Self::load_from(stats_path())
    }

    fn load_from(path: PathBuf) -> Self {
        let backup = path.with_extension("json.bak");
        let source = if path.exists() || !backup.exists() {
            path.clone()
        } else {
            tracing::warn!(
                "{} was missing after an interrupted save; recovering stats from {}",
                path.display(),
                backup.display()
            );
            backup
        };
        let (stats, writable) = match fs::read_to_string(&source) {
            Ok(json) => match serde_json::from_str(&json) {
                Ok(stats) => (stats, true),
                Err(e) => {
                    let bad = path.with_extension("json.bad");
                    match fs::copy(&source, &bad) {
                        Ok(_) => tracing::warn!(
                            "could not parse {}: {e}; backed it up to {} and reset stats",
                            source.display(),
                            bad.display()
                        ),
                        Err(copy_err) => tracing::warn!(
                            "could not parse {}: {e}; reset stats (backup failed: {copy_err})",
                            source.display()
                        ),
                    }
                    (UsageStats::default(), true)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (UsageStats::default(), true),
            Err(e) => {
                tracing::warn!(
                    "could not read {}: {e}; keeping stats read-only so existing history cannot be overwritten",
                    source.display()
                );
                (UsageStats::default(), false)
            }
        };
        Self {
            path,
            inner: Mutex::new((stats, 0)),
            persist_lock: Mutex::new(()),
            persisted_revision: AtomicU64::new(0),
            pending_writes: Mutex::new(0),
            pending_cv: Condvar::new(),
            active_sessions: Mutex::new(0),
            active_sessions_cv: Condvar::new(),
            writable,
        }
    }

    pub fn snapshot(&self) -> UsageStats {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0
            .clone()
    }

    /// Add one completed dictation and queue an immediate durable write. Only
    /// aggregate numbers are stored; transcript text and API keys never enter
    /// this file.
    pub fn record_dictation(self: &Arc<Self>, provider: &str, words: u64, audio_ms: u64) {
        if words == 0 {
            return;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (snapshot, revision) = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.0.record(provider, words, audio_ms, now);
            state.1 = state.1.saturating_add(1);
            (state.0.clone(), state.1)
        };

        if !self.writable {
            tracing::warn!(
                "transcription stats updated in memory but not saved (store is read-only)"
            );
            return;
        }
        self.begin_pending_write();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let store = Arc::clone(self);
            handle.spawn_blocking(move || {
                store.persist(snapshot, revision);
                store.finish_pending_write();
            });
        } else {
            self.persist(snapshot, revision);
            self.finish_pending_write();
        }
    }

    /// Wait until every currently queued stats snapshot is durable.
    pub fn flush(&self) {
        let mut pending = self
            .pending_writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *pending > 0 {
            pending = self
                .pending_cv
                .wait(pending)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Register one physical hotkey session as a potential future stats writer.
    /// The guard's cancellation-safe `Drop` ensures shutdown never waits on a
    /// task that Tokio had to tear down.
    pub fn session_guard(self: &Arc<Self>) -> StatsSessionGuard {
        let mut active = self
            .active_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_add(1);
        drop(active);
        StatsSessionGuard {
            store: Arc::clone(self),
        }
    }

    /// Shutdown barrier: let every session enqueue its final aggregate, then
    /// wait for those queued writes. The replacement process also waits for the
    /// old process mutex, so it cannot load stats before this barrier completes.
    pub fn finish_sessions_and_flush(&self) {
        let mut active = self
            .active_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *active > 0 {
            active = self
                .active_sessions_cv
                .wait(active)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        drop(active);
        self.flush();
    }

    fn begin_pending_write(&self) {
        let mut pending = self
            .pending_writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *pending = pending.saturating_add(1);
    }

    fn finish_pending_write(&self) {
        let mut pending = self
            .pending_writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *pending = pending.saturating_sub(1);
        if *pending == 0 {
            self.pending_cv.notify_all();
        }
    }

    fn finish_session(&self) {
        let mut active = self
            .active_sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_sub(1);
        if *active == 0 {
            self.active_sessions_cv.notify_all();
        }
    }

    fn persist(&self, stats: UsageStats, revision: u64) {
        let _guard = self
            .persist_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if revision <= self.persisted_revision.load(Ordering::Acquire) {
            return;
        }
        if let Err(e) = save_atomic(&self.path, &stats) {
            tracing::warn!("could not save transcription stats: {e}");
        } else {
            self.persisted_revision.store(revision, Ordering::Release);
        }
    }
}

pub struct StatsSessionGuard {
    store: Arc<StatsStore>,
}

impl Drop for StatsSessionGuard {
    fn drop(&mut self) {
        self.store.finish_session();
    }
}

fn stats_path() -> PathBuf {
    Config::settings_path()
        .parent()
        .map(|dir| dir.join(STATS_FILE))
        .unwrap_or_else(|| PathBuf::from(STATS_FILE))
}

fn save_atomic(path: &Path, stats: &UsageStats) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(stats)
        .map_err(|e| format!("could not serialize {}: {e}", path.display()))?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let backup = path.with_extension("json.bak");
    let mut file =
        fs::File::create(&tmp).map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    file.write_all(&json)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("could not flush {}: {e}", tmp.display()))?;
    drop(file);

    if path.exists() {
        match fs::remove_file(&backup) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "could not clear old stats backup {}: {e}",
                    backup.display()
                ))
            }
        }
        fs::rename(path, &backup).map_err(|e| {
            format!(
                "could not stage {} for replacement at {}: {e}",
                path.display(),
                backup.display()
            )
        })?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        if backup.exists() && !path.exists() {
            let _ = fs::rename(&backup, path);
        }
        return Err(format!("could not save {}: {e}", path.display()));
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_stats_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "quickdictate-{name}-{}-{nonce}.json",
            std::process::id()
        ))
    }

    #[test]
    fn aggregation_tracks_totals_longest_and_providers() {
        let mut stats = UsageStats::default();
        stats.record("ElevenLabs", 12, 5_000, 100);
        stats.record("local", 30, 20_000, 200);
        stats.record("local", 8, 40_000, 300);

        assert_eq!(stats.total_words, 50);
        assert_eq!(stats.total_audio_ms, 65_000);
        assert_eq!(stats.total_dictations, 3);
        assert_eq!(stats.longest_dictation_words, 30);
        assert_eq!(stats.longest_dictation_audio_ms, 40_000);
        assert_eq!(stats.tracking_started_unix_secs, 100);
        assert_eq!(stats.last_dictation_unix_secs, 300);
        assert_eq!(
            stats.providers["local"],
            ProviderStats {
                words: 38,
                audio_ms: 60_000,
                dictations: 2,
            }
        );
    }

    #[test]
    fn zero_word_attempt_is_not_a_dictation() {
        let mut stats = UsageStats::default();
        stats.record("local", 0, 99_000, 100);
        assert_eq!(stats, UsageStats::default());
    }

    #[test]
    fn old_json_with_missing_fields_loads_via_defaults() {
        let stats: UsageStats = serde_json::from_str(r#"{"total_words":42}"#).unwrap();
        assert_eq!(stats.total_words, 42);
        assert_eq!(stats.total_dictations, 0);
        assert!(stats.providers.is_empty());
    }

    #[test]
    fn store_round_trips_and_recovers_from_corrupt_json() {
        let path = temp_stats_path("stats-round-trip");
        let store = Arc::new(StatsStore::load_from(path.clone()));
        store.record_dictation("openai", 7, 2_500);
        let reloaded = StatsStore::load_from(path.clone()).snapshot();
        assert_eq!(reloaded.total_words, 7);
        assert_eq!(reloaded.total_audio_ms, 2_500);

        fs::write(&path, b"{not json").unwrap();
        let recovered = StatsStore::load_from(path.clone()).snapshot();
        assert_eq!(recovered, UsageStats::default());
        assert!(path.with_extension("json.bad").exists());

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("json.bad"));
    }

    #[test]
    fn store_recovers_backup_left_by_an_interrupted_save() {
        let path = temp_stats_path("stats-backup-recovery");
        let backup = path.with_extension("json.bak");
        let mut stats = UsageStats::default();
        stats.record("local", 11, 4_000, 100);
        fs::write(&backup, serde_json::to_vec(&stats).unwrap()).unwrap();

        let recovered = StatsStore::load_from(path.clone()).snapshot();
        assert_eq!(recovered.total_words, 11);
        assert_eq!(recovered.total_audio_ms, 4_000);

        let _ = fs::remove_file(path);
        let _ = fs::remove_file(backup);
    }

    #[test]
    fn flush_hands_complete_totals_to_a_replacement_store() {
        let path = temp_stats_path("stats-relaunch-flush");
        let old = Arc::new(StatsStore::load_from(path.clone()));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        runtime.block_on(async {
            old.record_dictation("local", 9, 3_000);
        });
        old.flush();

        // Mirrors the replacement process: it is only launched after flush,
        // loads the durable total, then records its own later dictation.
        let replacement = Arc::new(StatsStore::load_from(path.clone()));
        replacement.record_dictation("local", 4, 1_000);
        let final_stats = StatsStore::load_from(path.clone()).snapshot();
        assert_eq!(final_stats.total_words, 13);
        assert_eq!(final_stats.total_audio_ms, 4_000);
        assert_eq!(final_stats.total_dictations, 2);

        drop(runtime);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn shutdown_barrier_waits_for_the_session_guard() {
        let path = temp_stats_path("stats-session-barrier");
        let store = Arc::new(StatsStore::load_from(path.clone()));
        let guard = store.session_guard();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let barrier_store = Arc::clone(&store);
        let join = std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            barrier_store.finish_sessions_and_flush();
            done_tx.send(()).unwrap();
        });

        entered_rx.recv().unwrap();
        assert!(done_rx.try_recv().is_err());
        drop(guard);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("barrier should finish after the session guard drops");
        join.join().unwrap();

        let _ = fs::remove_file(path);
    }
}
