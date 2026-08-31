//! The whole picture: every device's counters, the local device's identity,
//! and the ranged views the Settings window charts.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::*;

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
    /// Device ids already folded into the `ARCHIVED_DEVICE_ID` bucket by a
    /// previous `SyncedUsageStats::prune`. Carried on `UsageStats` (not just
    /// the synced wire format) so the ledger survives across separate
    /// `merge_synced_value` calls: without it, a stale device's raw row
    /// reappearing in a later sync would look "new" again and get folded a
    /// second time.
    pub archived_device_ids: BTreeSet<String>,
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
            archived_device_ids: BTreeSet::new(),
        }
    }
}

impl UsageStats {
    pub(super) fn normalize(&mut self, now: u64) {
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

    pub(super) fn record(&mut self, provider: &str, words: u64, audio_ms: u64, now: u64) {
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

    /// Merge a remote synced snapshot into this store. Devices from both
    /// sides are unioned monotonically first, and only the combined result
    /// is pruned (see `SyncedUsageStats::prune`) — never either side in
    /// isolation — so stale-device eviction always sees the full picture.
    /// Returns whether anything actually changed.
    pub fn merge_synced_value(&mut self, remote: &Value) -> bool {
        let Ok(remote) = serde_json::from_value::<SyncedUsageStats>(remote.clone()) else {
            return false;
        };
        let now = unix_now();
        self.normalize(now);
        let before = SyncedUsageStats::from(&*self);
        let mut merged = before.clone();
        merged.merge(&remote);
        merged.prune(now);
        if merged == before {
            return false;
        }
        self.reset_unix_secs = merged.reset_unix_secs;
        self.reset_id = merged.reset_id;
        self.devices = merged.devices;
        self.archived_device_ids = merged.archived_device_ids;
        self.normalize(now);
        true
    }

    /// Merge two synced snapshots directly (used to reconcile two pushes
    /// racing against the same cloud document). Devices are unioned first
    /// and the combined result is pruned once, for the same reason
    /// `merge_synced_value` prunes after merging rather than before:
    /// pruning either side in isolation could fold a different subset of
    /// stale devices on each side, and a later monotonic (max) merge of two
    /// independently-folded archive totals would understate the true sum.
    pub fn merge_synced_values(local: &Value, remote: &Value) -> Value {
        let Ok(mut local) = serde_json::from_value::<SyncedUsageStats>(local.clone()) else {
            return remote.clone();
        };
        let Ok(remote) = serde_json::from_value::<SyncedUsageStats>(remote.clone()) else {
            return serde_json::to_value(local).unwrap_or(Value::Null);
        };
        let now = unix_now();
        local.merge(&remote);
        local.prune(now);
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
