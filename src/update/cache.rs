//! The on-disk throttle so a launch hits the network at most once a day,
//! and backs off for an hour after a failed check.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

// ---------------------------------------------------------------------------
// Throttle cache
// ---------------------------------------------------------------------------

fn cache_path() -> Option<PathBuf> {
    Some(crate::paths::data_file(CACHE_FILE))
}

pub(super) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(super) fn read_cache() -> Option<(u64, String)> {
    let text = std::fs::read_to_string(cache_path()?).ok()?;
    let mut lines = text.lines();
    let ts = lines.next()?.trim().parse::<u64>().ok()?;
    let tag = lines.next().unwrap_or("").trim().to_string();
    Some((ts, tag))
}

pub(super) fn write_cache(tag: &str) {
    if let Some(p) = cache_path() {
        let _ = std::fs::write(p, format!("{}\n{}\n", now_secs(), tag));
    }
}

/// After a FAILED check, retry this much later instead of the full 24 h.
const FAILED_RETRY_SECS: u64 = 60 * 60;

/// Stamp the cache after a failed check, backdated so it reads as fresh for
/// only [`FAILED_RETRY_SECS`]. Two constraints meet here: an offline machine
/// must not hammer the endpoint on every launch (why the failure is cached at
/// all), but one bad check at boot, Wi-Fi not up yet, a DNS blip, must not
/// count as a real answer, or the machine spends a full day believing it is
/// up to date when a release is out (updates are notify-only now, so a
/// suppressed check IS a suppressed notice). Backdating keeps the cache file
/// format and every reader unchanged.
pub(super) fn write_cache_failed() {
    if let Some(p) = cache_path() {
        let ts = now_secs().saturating_sub(CHECK_INTERVAL_SECS - FAILED_RETRY_SECS);
        let _ = std::fs::write(p, format!("{}\n{}\n", ts, env!("CARGO_PKG_VERSION")));
    }
}
