//! Persistent, privacy-safe transcription usage statistics.
//!
//! Stats intentionally live outside `settings.json`: Settings keeps an editable
//! snapshot and rewrites that file wholesale, while these counters change after
//! every successful dictation. Keeping a small numeric-only file prevents stale
//! Settings saves from clobbering live totals. When Connections sync is enabled,
//! a mergeable, numeric-only copy of the stats is included in the synced payload.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::Config;

const STATS_FILE: &str = "quickdictate-stats.json";
const HOUR_SECS: u64 = 60 * 60;
const RECENT_HISTORY_HOURS: u64 = 24 * 8;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ProviderStats {
    #[serde(rename = "w", alias = "words")]
    pub words: u64,
    #[serde(rename = "a", alias = "audio_ms")]
    pub audio_ms: u64,
    #[serde(rename = "d", alias = "dictations")]
    pub dictations: u64,
}

impl ProviderStats {
    fn add_assign(&mut self, other: &Self) {
        self.words = self.words.saturating_add(other.words);
        self.audio_ms = self.audio_ms.saturating_add(other.audio_ms);
        self.dictations = self.dictations.saturating_add(other.dictations);
    }

    fn merge_monotonic(&mut self, other: &Self) {
        self.words = self.words.max(other.words);
        self.audio_ms = self.audio_ms.max(other.audio_ms);
        self.dictations = self.dictations.max(other.dictations);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct PeriodStats {
    #[serde(rename = "w", alias = "words")]
    pub words: u64,
    #[serde(rename = "a", alias = "audio_ms")]
    pub audio_ms: u64,
    #[serde(rename = "d", alias = "dictations")]
    pub dictations: u64,
    #[serde(rename = "lw", alias = "longest_dictation_words")]
    pub longest_dictation_words: u64,
    #[serde(rename = "la", alias = "longest_dictation_audio_ms")]
    pub longest_dictation_audio_ms: u64,
    #[serde(rename = "p", alias = "providers")]
    pub providers: BTreeMap<String, ProviderStats>,
}

impl PeriodStats {
    fn record(&mut self, provider: &str, words: u64, audio_ms: u64) {
        self.words = self.words.saturating_add(words);
        self.audio_ms = self.audio_ms.saturating_add(audio_ms);
        self.dictations = self.dictations.saturating_add(1);
        self.longest_dictation_words = self.longest_dictation_words.max(words);
        self.longest_dictation_audio_ms = self.longest_dictation_audio_ms.max(audio_ms);
        let totals = self.providers.entry(provider.to_string()).or_default();
        totals.words = totals.words.saturating_add(words);
        totals.audio_ms = totals.audio_ms.saturating_add(audio_ms);
        totals.dictations = totals.dictations.saturating_add(1);
    }

    fn add_assign(&mut self, other: &Self) {
        self.words = self.words.saturating_add(other.words);
        self.audio_ms = self.audio_ms.saturating_add(other.audio_ms);
        self.dictations = self.dictations.saturating_add(other.dictations);
        self.longest_dictation_words = self
            .longest_dictation_words
            .max(other.longest_dictation_words);
        self.longest_dictation_audio_ms = self
            .longest_dictation_audio_ms
            .max(other.longest_dictation_audio_ms);
        for (provider, totals) in &other.providers {
            self.providers
                .entry(provider.clone())
                .or_default()
                .add_assign(totals);
        }
    }

    fn merge_monotonic(&mut self, other: &Self) {
        self.words = self.words.max(other.words);
        self.audio_ms = self.audio_ms.max(other.audio_ms);
        self.dictations = self.dictations.max(other.dictations);
        self.longest_dictation_words = self
            .longest_dictation_words
            .max(other.longest_dictation_words);
        self.longest_dictation_audio_ms = self
            .longest_dictation_audio_ms
            .max(other.longest_dictation_audio_ms);
        for (provider, totals) in &other.providers {
            self.providers
                .entry(provider.clone())
                .or_default()
                .merge_monotonic(totals);
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct DeviceStats {
    #[serde(rename = "s", alias = "tracking_started_unix_secs")]
    pub tracking_started_unix_secs: u64,
    #[serde(rename = "l", alias = "last_dictation_unix_secs")]
    pub last_dictation_unix_secs: u64,
    #[serde(rename = "t", alias = "totals")]
    pub totals: PeriodStats,
    /// Sparse UTC-hour buckets. Only the most recent week is retained.
    #[serde(rename = "h", alias = "hours")]
    pub hours: BTreeMap<u64, PeriodStats>,
}

impl DeviceStats {
    fn record(&mut self, provider: &str, words: u64, audio_ms: u64, now: u64) {
        if self.tracking_started_unix_secs == 0 {
            self.tracking_started_unix_secs = now;
        }
        self.last_dictation_unix_secs = now;
        self.totals.record(provider, words, audio_ms);
        self.hours
            .entry(now / HOUR_SECS * HOUR_SECS)
            .or_default()
            .record(provider, words, audio_ms);
        let oldest = now.saturating_sub(RECENT_HISTORY_HOURS * HOUR_SECS);
        self.hours.retain(|hour, _| *hour >= oldest);
    }

    fn merge_monotonic(&mut self, other: &Self) {
        self.tracking_started_unix_secs = earliest_nonzero(
            self.tracking_started_unix_secs,
            other.tracking_started_unix_secs,
        );
        self.last_dictation_unix_secs = self
            .last_dictation_unix_secs
            .max(other.last_dictation_unix_secs);
        self.totals.merge_monotonic(&other.totals);
        for (hour, totals) in &other.hours {
            self.hours.entry(*hour).or_default().merge_monotonic(totals);
        }
    }
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
    /// Local-only identity used as the merge key for this install's counters.
    pub local_device_id: String,
    /// A reset creates a newer generation so old cloud counters cannot return.
    pub reset_unix_secs: u64,
    pub reset_id: String,
    /// Per-install monotonic counters make cloud merges idempotent.
    pub devices: BTreeMap<String, DeviceStats>,
}

impl Default for UsageStats {
    fn default() -> Self {
        Self {
            schema_version: 2,
            tracking_started_unix_secs: 0,
            last_dictation_unix_secs: 0,
            total_words: 0,
            total_audio_ms: 0,
            total_dictations: 0,
            longest_dictation_words: 0,
            longest_dictation_audio_ms: 0,
            providers: BTreeMap::new(),
            local_device_id: String::new(),
            reset_unix_secs: 0,
            reset_id: String::new(),
            devices: BTreeMap::new(),
        }
    }
}

impl UsageStats {
    fn normalize(&mut self, now: u64) {
        if self.local_device_id.is_empty() {
            self.local_device_id = new_local_device_id();
        }

        // Schema-v1 migration: preserve all lifetime totals as this install's
        // contribution. Recent ranges start filling from this version onward.
        if self.devices.is_empty() && self.total_dictations > 0 {
            self.devices.insert(
                self.local_device_id.clone(),
                DeviceStats {
                    tracking_started_unix_secs: self.tracking_started_unix_secs,
                    last_dictation_unix_secs: self.last_dictation_unix_secs,
                    totals: PeriodStats {
                        words: self.total_words,
                        audio_ms: self.total_audio_ms,
                        dictations: self.total_dictations,
                        longest_dictation_words: self.longest_dictation_words,
                        longest_dictation_audio_ms: self.longest_dictation_audio_ms,
                        providers: self.providers.clone(),
                    },
                    hours: BTreeMap::new(),
                },
            );
        }
        let oldest = now.saturating_sub(RECENT_HISTORY_HOURS * HOUR_SECS);
        for device in self.devices.values_mut() {
            device.hours.retain(|hour, _| *hour >= oldest);
        }
        self.schema_version = 2;
        self.rebuild_totals();
    }

    fn record(&mut self, provider: &str, words: u64, audio_ms: u64, now: u64) {
        if words == 0 {
            return;
        }
        let provider = provider.trim().to_ascii_lowercase();
        let provider = if provider.is_empty() {
            "unknown".to_string()
        } else {
            provider
        };
        self.normalize(now);
        self.devices
            .entry(self.local_device_id.clone())
            .or_default()
            .record(&provider, words, audio_ms, now);
        self.rebuild_totals();
    }

    fn rebuild_totals(&mut self) {
        let mut totals = PeriodStats::default();
        let mut started = 0;
        let mut last = 0;
        for device in self.devices.values() {
            totals.add_assign(&device.totals);
            started = earliest_nonzero(started, device.tracking_started_unix_secs);
            last = last.max(device.last_dictation_unix_secs);
        }
        self.tracking_started_unix_secs = started;
        self.last_dictation_unix_secs = last;
        self.total_words = totals.words;
        self.total_audio_ms = totals.audio_ms;
        self.total_dictations = totals.dictations;
        self.longest_dictation_words = totals.longest_dictation_words;
        self.longest_dictation_audio_ms = totals.longest_dictation_audio_ms;
        self.providers = totals.providers;
    }

    pub fn view(&self, range: StatsRange, now: u64) -> StatsView {
        let (totals, chart, chart_caption) = match range {
            StatsRange::Last24Hours => (
                self.recent_totals(now, 24),
                self.chart(now, 24, 1),
                "Dictations by hour".to_string(),
            ),
            StatsRange::Last7Days => (
                self.recent_totals(now, 24 * 7),
                self.chart(now, 24 * 7, 24),
                "Dictations across the last 7 days".to_string(),
            ),
            StatsRange::AllTime => (
                PeriodStats {
                    words: self.total_words,
                    audio_ms: self.total_audio_ms,
                    dictations: self.total_dictations,
                    longest_dictation_words: self.longest_dictation_words,
                    longest_dictation_audio_ms: self.longest_dictation_audio_ms,
                    providers: self.providers.clone(),
                },
                self.chart(now, 24 * 7, 24),
                "Recent 7-day activity".to_string(),
            ),
        };
        StatsView {
            totals,
            chart,
            chart_caption,
        }
    }

    fn recent_totals(&self, now: u64, hours: u64) -> PeriodStats {
        let current_hour = now / HOUR_SECS * HOUR_SECS;
        let first_hour = current_hour.saturating_sub(hours.saturating_sub(1) * HOUR_SECS);
        let mut totals = PeriodStats::default();
        for device in self.devices.values() {
            for (_, hour) in device.hours.range(first_hour..=current_hour) {
                totals.add_assign(hour);
            }
        }
        totals
    }

    fn chart(&self, now: u64, hours: u64, hours_per_bar: u64) -> Vec<u64> {
        let bars = hours.div_ceil(hours_per_bar) as usize;
        let current_hour = now / HOUR_SECS * HOUR_SECS;
        let first_hour = current_hour.saturating_sub(hours.saturating_sub(1) * HOUR_SECS);
        let mut points = vec![0u64; bars];
        for device in self.devices.values() {
            for (hour, totals) in device.hours.range(first_hour..=current_hour) {
                let index = ((*hour - first_hour) / (hours_per_bar * HOUR_SECS)) as usize;
                if let Some(point) = points.get_mut(index) {
                    *point = point.saturating_add(totals.dictations);
                }
            }
        }
        points
    }

    pub fn synced_value(&self) -> Value {
        serde_json::to_value(SyncedUsageStats::from(self)).unwrap_or(Value::Null)
    }

    pub fn merge_synced_value(&mut self, remote: &Value) -> bool {
        let Ok(mut remote) = serde_json::from_value::<SyncedUsageStats>(remote.clone()) else {
            return false;
        };
        let now = unix_now();
        self.normalize(now);
        remote.prune(now);
        let before = SyncedUsageStats::from(&*self);
        let mut merged = before.clone();
        merged.merge(&remote);
        if merged == before {
            return false;
        }
        self.reset_unix_secs = merged.reset_unix_secs;
        self.reset_id = merged.reset_id;
        self.devices = merged.devices;
        self.normalize(now);
        true
    }

    pub fn merge_synced_values(local: &Value, remote: &Value) -> Value {
        let Ok(mut local) = serde_json::from_value::<SyncedUsageStats>(local.clone()) else {
            return remote.clone();
        };
        let Ok(mut remote) = serde_json::from_value::<SyncedUsageStats>(remote.clone()) else {
            return serde_json::to_value(local).unwrap_or(Value::Null);
        };
        let now = unix_now();
        local.prune(now);
        remote.prune(now);
        local.merge(&remote);
        serde_json::to_value(local).unwrap_or(Value::Null)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatsRange {
    Last24Hours,
    Last7Days,
    #[default]
    AllTime,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatsView {
    pub totals: PeriodStats,
    pub chart: Vec<u64>,
    pub chart_caption: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
struct SyncedUsageStats {
    #[serde(rename = "v", alias = "schema_version")]
    schema_version: u32,
    #[serde(rename = "r", alias = "reset_unix_secs")]
    reset_unix_secs: u64,
    #[serde(rename = "i", alias = "reset_id")]
    reset_id: String,
    #[serde(rename = "d", alias = "devices")]
    devices: BTreeMap<String, DeviceStats>,
}

impl From<&UsageStats> for SyncedUsageStats {
    fn from(stats: &UsageStats) -> Self {
        Self {
            schema_version: 2,
            reset_unix_secs: stats.reset_unix_secs,
            reset_id: stats.reset_id.clone(),
            devices: stats.devices.clone(),
        }
    }
}

impl SyncedUsageStats {
    fn prune(&mut self, now: u64) {
        let oldest = now.saturating_sub(RECENT_HISTORY_HOURS * HOUR_SECS);
        for device in self.devices.values_mut() {
            device.hours.retain(|hour, _| *hour >= oldest);
        }
    }

    fn merge(&mut self, other: &Self) {
        let local_generation = (self.reset_unix_secs, self.reset_id.as_str());
        let remote_generation = (other.reset_unix_secs, other.reset_id.as_str());
        if remote_generation > local_generation {
            *self = other.clone();
            return;
        }
        if remote_generation < local_generation {
            return;
        }
        self.schema_version = 2;
        for (id, remote) in &other.devices {
            self.devices
                .entry(id.clone())
                .or_default()
                .merge_monotonic(remote);
        }
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn earliest_nonzero(left: u64, right: u64) -> u64 {
    match (left, right) {
        (0, value) | (value, 0) => value,
        _ => left.min(right),
    }
}

fn new_local_device_id() -> String {
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(nonce.to_le_bytes());
    let digest = hasher.finalize();
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
        assert_eq!(recovered.total_dictations, 0);
        assert_eq!(recovered.total_words, 0);
        assert!(recovered.devices.is_empty());
        assert!(!recovered.local_device_id.is_empty());
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
    fn recent_ranges_and_chart_use_hourly_history() {
        let now = 1_000 * HOUR_SECS;
        let mut stats = UsageStats::default();
        stats.record("local", 5, 1_000, now - 25 * HOUR_SECS);
        stats.record("local", 7, 2_000, now - 3 * HOUR_SECS);
        stats.record("openai", 11, 3_000, now);

        let day = stats.view(StatsRange::Last24Hours, now);
        assert_eq!(day.totals.words, 18);
        assert_eq!(day.totals.dictations, 2);
        assert_eq!(day.chart.iter().sum::<u64>(), 2);

        let week = stats.view(StatsRange::Last7Days, now);
        assert_eq!(week.totals.words, 23);
        assert_eq!(week.totals.dictations, 3);
        assert_eq!(week.chart.iter().sum::<u64>(), 3);

        let all = stats.view(StatsRange::AllTime, now);
        assert_eq!(all.totals.words, 23);
        assert_eq!(all.totals.dictations, 3);
    }

    #[test]
    fn synced_device_counters_merge_idempotently() {
        let now = 1_000 * HOUR_SECS;
        let mut first = UsageStats {
            local_device_id: "first-device".into(),
            ..UsageStats::default()
        };
        first.record("local", 5, 1_000, now);
        let mut second = UsageStats {
            local_device_id: "second-device".into(),
            ..UsageStats::default()
        };
        second.record("openai", 7, 2_000, now);

        let merged = UsageStats::merge_synced_values(&first.synced_value(), &second.synced_value());
        let mut received = UsageStats::default();
        assert!(received.merge_synced_value(&merged));
        assert_eq!(received.total_words, 12);
        assert_eq!(received.total_dictations, 2);
        assert!(!received.merge_synced_value(&merged));
        assert_eq!(received.total_words, 12);
    }

    #[test]
    fn reset_is_durable_and_rejects_a_stale_synced_generation() {
        let path = temp_stats_path("stats-reset");
        let store = Arc::new(StatsStore::load_from(path.clone()));
        store.record_dictation("local", 9, 3_000);
        store.flush();
        let stale = store.snapshot().synced_value();

        store.reset();
        store.flush();
        assert_eq!(store.snapshot().total_dictations, 0);
        assert!(!store.apply_synced(&stale));
        assert_eq!(store.snapshot().total_dictations, 0);
        assert_eq!(
            StatsStore::load_from(path.clone())
                .snapshot()
                .total_dictations,
            0
        );

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
