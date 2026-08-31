//! Persistent, privacy-safe transcription usage statistics.
//!
//! Stats intentionally live outside `settings.json`: Settings keeps an editable
//! snapshot and rewrites that file wholesale, while these counters change after
//! every successful dictation. Keeping a small numeric-only file prevents stale
//! Settings saves from clobbering live totals. When Connections sync is enabled,
//! a mergeable, numeric-only copy of the stats is included in the synced payload.

mod aggregate;
mod store;
mod synced;
mod usage;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

pub use aggregate::{DeviceStats, PeriodStats, ProviderStats};
pub use store::StatsStore;
pub use usage::{StatsRange, StatsView, UsageStats};

use synced::SyncedUsageStats;

const STATS_FILE: &str = "quickdictate-stats.json";
const HOUR_SECS: u64 = 60 * 60;
const RECENT_HISTORY_HOURS: u64 = 24 * 8;
/// A device with no activity for this long is folded into the archived
/// bucket by `SyncedUsageStats::prune`, so a multi-year, multi-machine
/// account does not accumulate one permanent entry per install id.
const STALE_DEVICE_SECS: u64 = 60 * 60 * 24 * 180;
/// Reserved device id that absorbs lifetime totals folded from evicted
/// devices. Never evicted itself, and distinct from any real install id
/// (those come from `new_local_device_id`, which is 32 hex chars).
const ARCHIVED_DEVICE_ID: &str = "archived";

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
