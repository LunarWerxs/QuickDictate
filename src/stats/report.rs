//! Opt-in anonymized usage rollup to LunarWerx (`Config::share_usage_stats`).
//!
//! WHY: QuickDictate already computes exactly the numbers a product-analytics
//! dashboard would want -- provider mix, word/audio/dictation counts -- but
//! keeps them strictly local (see `stats::usage::UsageStats`, the existing
//! Settings-window charts). This lets LunarWerx see aggregate feature
//! adoption fleet-wide without a new pipeline: it reuses the same
//! `studio.connections.icu/v1/app/quickdictate/*` endpoint family and
//! anonymous `install_id` the update checker already established as a
//! precedent (`update::RELEASES_API`, `update::init_install_id`). Off by
//! default; a distinct, new capability from the already-shipped local usage
//! stats and from `sync::mod` (which syncs a signed-in user's *own* stats
//! back to their *own* account -- this instead sends one aggregate,
//! unattributable-to-a-person rollup to the product team, only when the user
//! opts in).
//!
//! Adapted from PostHog's product-analytics idea (`posthog/posthog`,
//! MIT-licensed), not ported: PostHog's autocapture/event pipeline has no
//! analog here, so this is a from-scratch, minimal client written in
//! QuickDictate's own idiom (blocking `reqwest` on a worker thread, an
//! on-disk daily throttle cache, both copied in shape from `update::cache`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::state::App;

use super::UsageStats;

#[cfg(test)]
mod tests;

/// Studio endpoint for the anonymized usage rollup -- a sibling of
/// `update::RELEASES_API` under the same `/v1/app/quickdictate/*` namespace.
/// Registration is an owner action (same as `sync::CLIENT_ID`'s one-time
/// OAuth-app registration); until it exists server-side, [`send_now`] simply
/// fails closed and the next daily attempt carries the same lifetime totals
/// forward, so no data is lost by the endpoint not existing yet.
pub const USAGE_REPORT_API: &str = "https://studio.connections.icu/v1/app/quickdictate/usage";

const USER_AGENT: &str = concat!("QuickDictate/", env!("CARGO_PKG_VERSION"));
const CACHE_FILE: &str = "quickdictate-usage-report.txt";

/// At most one real network send per this interval, same cadence as the
/// update checker (`update::CHECK_INTERVAL_SECS`) -- a daily aggregate is all
/// an adoption dashboard needs, and it keeps this indistinguishable from
/// background noise on the wire.
const REPORT_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Guards against a duplicate concurrent send (e.g. Settings save racing
/// startup), same role as `update::IN_FLIGHT`.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_path() -> PathBuf {
    crate::paths::data_file(CACHE_FILE)
}

fn last_sent_unix_secs() -> Option<u64> {
    std::fs::read_to_string(cache_path())
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn write_cache() {
    let _ = std::fs::write(cache_path(), now_secs().to_string());
}

/// The exact, allowlisted JSON body sent. Deliberately built field-by-field
/// rather than `serde_json::to_value(stats)`: `UsageStats` also carries
/// `local_device_id`, `devices` (per-machine breakdowns, sync merge state),
/// and `archived_device_ids`, none of which are meant to leave this machine,
/// and a future field added to that struct for the *sync* merge machinery
/// must not silently start riding along in this *report* payload too.
/// `install_id` is the one identifier included -- the same crypto-random,
/// machine-only id already sent with update checks, never derived from
/// hostname, MAC, username, or account.
pub(super) fn anonymized_payload(install_id: &str, stats: &UsageStats) -> Value {
    let mut providers = Map::new();
    for (name, p) in &stats.providers {
        providers.insert(
            name.clone(),
            json!({
                "words": p.words,
                "audio_ms": p.audio_ms,
                "dictations": p.dictations,
            }),
        );
    }
    json!({
        "install_id": install_id,
        "app_version": env!("CARGO_PKG_VERSION"),
        "total_words": stats.total_words,
        "total_audio_ms": stats.total_audio_ms,
        "total_dictations": stats.total_dictations,
        "longest_dictation_words": stats.longest_dictation_words,
        "longest_dictation_audio_ms": stats.longest_dictation_audio_ms,
        "providers": Value::Object(providers),
    })
}

fn client() -> Option<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .ok()
}

/// POST the rollup now, unconditionally -- throttling is [`spawn_daily_report`]'s
/// job. Any failure (offline, DNS, non-2xx, endpoint not registered yet) is
/// returned for the caller to log and swallow; a failed send never surfaces
/// to the user and never blocks the hotkey/session loop, since this only
/// ever runs on its own background thread.
fn send_now(install_id: &str, stats: &UsageStats) -> anyhow::Result<()> {
    let client = client().ok_or_else(|| anyhow::anyhow!("failed to build http client"))?;
    let resp = client
        .post(USAGE_REPORT_API)
        .json(&anonymized_payload(install_id, stats))
        .send()?;
    if !resp.status().is_success() {
        anyhow::bail!("usage report rejected: {}", resp.status());
    }
    Ok(())
}

/// Startup hook (see `startup::bring_up_app`): if `share_usage_stats` is on
/// and the last send was more than a day ago (or never), send one rollup on
/// a background thread. Mirrors `update::spawn_startup_check`'s throttle
/// shape and never-blocks-startup contract, but is a much smaller job -- one
/// POST, no download, no install.
pub fn spawn_daily_report(app: Arc<App>) {
    if !app.config.load().share_usage_stats {
        return;
    }
    if IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    let fresh = last_sent_unix_secs()
        .map(|ts| now_secs().saturating_sub(ts) < REPORT_INTERVAL_SECS)
        .unwrap_or(false);
    if fresh {
        tracing::debug!("usage report: skipping (cache fresh)");
        IN_FLIGHT.store(false, Ordering::Release);
        return;
    }
    std::thread::spawn(move || {
        let install_id = app.config.load().install_id.clone();
        if install_id.is_empty() {
            // Not yet assigned this launch (RNG/persist failure) -- try again
            // next startup rather than sending an unidentifiable row.
            tracing::debug!("usage report: skipped (no install id yet)");
        } else {
            let stats = app.stats.snapshot();
            match send_now(&install_id, &stats) {
                Ok(()) => {
                    write_cache();
                    tracing::debug!("usage report: sent");
                }
                Err(e) => tracing::debug!("usage report: skipped ({e})"),
            }
        }
        IN_FLIGHT.store(false, Ordering::Release);
    });
}
