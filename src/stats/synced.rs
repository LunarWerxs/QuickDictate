//! The numeric-only shape that travels to Connections, and the idempotent
//! merge that lets two machines' counters combine without double-counting.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::*;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(super) struct SyncedUsageStats {
    #[serde(rename = "v", alias = "schema_version")]
    pub(super) schema_version: u32,
    #[serde(rename = "r", alias = "reset_unix_secs")]
    pub(super) reset_unix_secs: u64,
    #[serde(rename = "i", alias = "reset_id")]
    pub(super) reset_id: String,
    #[serde(rename = "d", alias = "devices")]
    pub(super) devices: BTreeMap<String, DeviceStats>,
    /// Device ids already folded into the archived bucket by a previous
    /// `prune`, so a stale device that reappears in a later sync (the same
    /// remote value merged again, or a peer that never itself pruned it
    /// resending its raw row) is dropped instead of being summed into the
    /// archive a second time. `#[serde(default)]` (via the container
    /// attribute above) means an older peer that has never seen this field
    /// just reads an empty set and keeps working.
    #[serde(rename = "af", alias = "archived_device_ids")]
    pub(super) archived_device_ids: BTreeSet<String>,
}

impl From<&UsageStats> for SyncedUsageStats {
    fn from(stats: &UsageStats) -> Self {
        Self {
            schema_version: 2,
            reset_unix_secs: stats.reset_unix_secs,
            reset_id: stats.reset_id.clone(),
            devices: stats.devices.clone(),
            archived_device_ids: stats.archived_device_ids.clone(),
        }
    }
}

impl SyncedUsageStats {
    /// Trim stale hourly history and fold devices that have gone quiet for
    /// `STALE_DEVICE_SECS` into the reserved `ARCHIVED_DEVICE_ID` bucket, so
    /// a multi-year, multi-machine account does not accumulate one
    /// permanent row per install id.
    ///
    /// Callers always run this *after* unioning devices from both sides of
    /// a merge, never before: this must be a pure function of `self` and
    /// `now`, where pruning an already-pruned document, or pruning the same
    /// document twice, is a no-op. That rules out an unconditional
    /// accumulator (folding into the archive on every call), because the
    /// exact same stale device can arrive again on the next sync — its raw
    /// row simply gets re-merged in from whichever side never itself pruned
    /// it. `archived_device_ids` is the guard: once a device id has been
    /// folded in, it is never folded again, so a resurrected raw row is
    /// dropped in place instead of being summed a second time.
    ///
    /// Folding uses `add_assign` (summed) rather than `merge_monotonic`
    /// (maxed), because unlike two reports of the *same* device, two
    /// different retired devices contributed independently and their counts
    /// should add up. `hours` is intentionally not carried into the
    /// archive: it is already capped to `RECENT_HISTORY_HOURS`, and a device
    /// stale enough to evict has no hour buckets left inside that window
    /// anyway. The archived bucket itself is exempt from eviction so it
    /// cannot fold into itself.
    pub(super) fn prune(&mut self, now: u64) {
        let oldest = now.saturating_sub(RECENT_HISTORY_HOURS * HOUR_SECS);
        for device in self.devices.values_mut() {
            device.hours.retain(|hour, _| *hour >= oldest);
        }

        let cutoff = now.saturating_sub(STALE_DEVICE_SECS);
        let stale_ids: Vec<String> = self
            .devices
            .iter()
            .filter(|(id, device)| {
                id.as_str() != ARCHIVED_DEVICE_ID && device.last_dictation_unix_secs < cutoff
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale_ids {
            let Some(stale) = self.devices.remove(&id) else {
                continue;
            };
            if !self.archived_device_ids.insert(id) {
                // Already folded on a previous prune; the raw row simply
                // reappeared, so drop it without summing it in again.
                continue;
            }
            let archived = self
                .devices
                .entry(ARCHIVED_DEVICE_ID.to_string())
                .or_default();
            archived.tracking_started_unix_secs = earliest_nonzero(
                archived.tracking_started_unix_secs,
                stale.tracking_started_unix_secs,
            );
            archived.last_dictation_unix_secs = archived
                .last_dictation_unix_secs
                .max(stale.last_dictation_unix_secs);
            archived.totals.add_assign(&stale.totals);
        }
    }

    pub(super) fn merge(&mut self, other: &Self) {
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
        self.archived_device_ids
            .extend(other.archived_device_ids.iter().cloned());
    }
}
