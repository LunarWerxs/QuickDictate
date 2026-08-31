//! The stats file on disk: a thread-safe store over it, the crash-safe write,
//! and the guard that keeps the process alive until a dictation is durable.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::*;

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

    pub(super) fn load_from(path: PathBuf) -> Self {
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
        let (mut stats, writable) = match fs::read_to_string(&source) {
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
        let before_normalize = stats.clone();
        stats.normalize(unix_now());
        if writable && stats != before_normalize {
            if let Err(error) = save_atomic(&path, &stats) {
                tracing::warn!("could not persist stats schema migration: {error}");
            }
        }
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

    /// Merge a cloud stats generation into the local store. Per-device
    /// counters merge monotonically, so repeated pulls cannot double-count.
    pub fn apply_synced(self: &Arc<Self>, remote: &Value) -> bool {
        let queued = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !state.0.merge_synced_value(remote) {
                return false;
            }
            state.1 = state.1.saturating_add(1);
            (state.0.clone(), state.1)
        };
        self.queue_persist(queued.0, queued.1);
        true
    }

    /// Clear every local and synced contribution, starting a new generation so
    /// a stale device/cloud snapshot cannot restore the old counters.
    pub fn reset(self: &Arc<Self>) {
        let now = unix_now();
        let queued = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let local_device_id = state.0.local_device_id.clone();
            state.0 = UsageStats {
                local_device_id: local_device_id.clone(),
                reset_unix_secs: now,
                reset_id: format!("{now}-{local_device_id}"),
                ..UsageStats::default()
            };
            state.1 = state.1.saturating_add(1);
            (state.0.clone(), state.1)
        };
        self.queue_persist(queued.0, queued.1);
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

        self.queue_persist(snapshot, revision);
    }

    fn queue_persist(self: &Arc<Self>, snapshot: UsageStats, revision: u64) {
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
    crate::paths::data_file(STATS_FILE)
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
