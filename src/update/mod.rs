//! Check-for-update + silent portable self-update.
//!
//! Modeled directly on SageThumbs 2K's updater: the GitHub "latest release"
//! JSON (reached via LunarWerx's Studio proxy — see [`RELEASES_API`]),
//! lenient `vX.Y.Z` tag parsing with a plain tuple compare, a daily-throttled
//! on-disk cache so we hit the network at most once per day, and a
//! trusted GitHub URL + MZ-header + size + SHA-256 + version-self-check verified download. The install step differs
//! because QuickDictate is a **portable single exe** (no Inno Setup): instead
//! of launching a `/SILENT` installer we swap the exe in place —
//! `quickdictate.exe` → `quickdictate.exe.old`, new file in, relaunch with
//! `--updated <ver>` — which is the portable equivalent of a silent update.
//!
//! Trigger points:
//!   * startup auto-check (gated by `update_auto_check` in settings, default
//!     on; throttled to one network hit per 24 h via the cache file). A newer
//!     release is **reported, not installed**: [`pending_update`] is published
//!     for the tray tooltip and the About pill, and clicking the pill is the
//!     consent to install. Setting `update_auto_install` restores the old
//!     silent behaviour (download, verify, swap, relaunch), deferring the
//!     relaunch until you are idle so it never interrupts a dictation.
//!     The default changed in v0.5.4 because the download URL and its SHA-256
//!     both come from the release payload: hash pinning proves the bytes match
//!     what was uploaded, not that the maintainer intended to upload them, so
//!     anything able to publish a release could otherwise reach every install
//!     unattended within a day.
//!   * the About window (Settings → About, or its "Check for updates" item):
//!     the status pill checks on open and on click, and when an update is
//!     waiting, clicking the pill installs it in-app via
//!     [`download_and_install_now`] — it no longer opens the browser.

mod cache;
mod flows;
mod install;
mod install_id;

#[cfg(test)]
mod tests;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::state::App;

pub use flows::msg_box;
pub use flows::{handle_startup_artifacts, spawn_startup_check};
pub use install::download_and_install_now;
// Reached only by the mutation-fuzz suite, which is itself `#[cfg(test)]`;
// re-exporting them unconditionally would be an unused import in a real build.
#[cfg(test)]
pub(crate) use install::{exe_asset_from_json, trusted_asset_url};
pub use install_id::init_install_id;

use cache::*;
use install::*;

/// "Latest release" endpoint: the Connections Studio proxy, which relays
/// GitHub's `releases/latest` JSON for LunarWerxs/QuickDictate **verbatim**
/// (so parsing here is unchanged from the GitHub API) and logs one anonymous
/// analytics row per hit as an install-count statistic — random id, version,
/// and coarse CDN-derived geo, never the caller's IP; 90-day retention. The
/// request carries the `X-Install-Id` header resolved by [`init_install_id`]
/// plus the app version (`?v=`, for anonymous version-adoption stats). See
/// SECURITY.md for the full disclosure. Release
/// *binaries* still download straight from GitHub via the asset URLs in the
/// payload. On any failure the check reports Failed — which the auto path
/// treats as silence.
pub const RELEASES_API: &str = "https://studio.connections.icu/v1/app/quickdictate/latest";
pub const RELEASES_URL: &str = "https://github.com/LunarWerxs/QuickDictate/releases";

/// Resilience backstop for the check above, used only when the Studio proxy fails (see
/// [`fetch_github_fallback_json`]). GitHub's own releases/latest is the right one to fall back
/// to precisely because it is the only URL here a rename cannot orphan: GitHub redirects both
/// owner and repo renames, so this keeps resolving even if either changes.
///
/// Why this exists (YTSort, 2026-08): a shipped artifact whose single baked-in update URL later
/// stopped resolving left every install silently polling a dead link for six months, with no
/// signal to the users or the maintainer. One hardcoded endpoint and no second opinion is that
/// same failure waiting to happen, and a compiled binary cannot be repointed after the fact.
pub const GITHUB_LATEST_API: &str =
    "https://api.github.com/repos/LunarWerxs/QuickDictate/releases/latest";

/// GitHub rejects requests without a User-Agent (the release download still
/// goes there directly); the Studio proxy sees the same header.
const USER_AGENT: &str = concat!("QuickDictate/", env!("CARGO_PKG_VERSION"));

/// At most one real network check per this interval (auto path only).
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Hard cap on a downloaded release binary. Our exe is ~4 MB; anything close
/// to this is wrong.
const MAX_EXE_BYTES: u64 = 64 * 1024 * 1024;

/// Cache file next to the exe: `<unix_secs>\n<latest_tag>\n` (same shape as
/// SageThumbs' `%LOCALAPPDATA%` cache, but kept next to the exe because
/// QuickDictate is portable). Gitignored.
const CACHE_FILE: &str = "quickdictate-update.txt";

/// Only one check/download may run at a time (tray spam, About + auto, etc.).
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Anonymous install id sent as `X-Install-Id` with releases-API hits so the
/// endpoint can count unique installs rather than raw checks. Resolved once
/// at startup by [`init_install_id`]; unset (RNG or persist failure) simply
/// means the header is omitted.
static INSTALL_ID: OnceLock<String> = OnceLock::new();

/// The shared [`App`], published at startup so the **manual** update path — the
/// About window, which runs on its own thread with no `App` reference — can
/// signal a clean shutdown when it relaunches into the freshly-swapped exe. The
/// main loop polls `app.shutdown` every 50 ms and exits, handing off to the new
/// process. Unset before [`set_app_handle`] runs (only `download_and_swap` can
/// even be reached that early, and it never touches this).
static APP_HANDLE: OnceLock<Arc<App>> = OnceLock::new();

/// Publish the shared [`App`] handle for the manual update path. Called once
/// from `main()`, before the UI (hence any manual install) can come up.
pub fn set_app_handle(app: &Arc<App>) {
    let _ = APP_HANDLE.set(Arc::clone(app));
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateCheck {
    UpToDate,
    /// Newer version available; payload is the tag without the `v` prefix.
    Available(String),
    Failed,
}

/// Lenient `vX.Y.Z` / `X.Y` / `X.Y.Z-rc1` parser.
///
/// The fourth element is a prerelease rank: `0` for a prerelease, `1` for a
/// final release. Semver orders `1.0.0-rc1` BELOW `1.0.0`, and dropping the
/// suffix entirely (as this did) made them compare equal, so a final release
/// following its own release candidate reported "up to date" and was never
/// delivered. Build metadata (`+build7`) is still ignored, which is correct:
/// semver says it does not affect precedence.
///
/// Note this deliberately does NOT distinguish two DIFFERENT builds published
/// under the same tag. Detecting that would mean trusting the release payload
/// to say "this is newer than the identical-looking thing you have", which is
/// exactly the input an attacker controls. Republishing therefore requires a
/// version bump; `docs/RELEASING.md` says so.
pub(crate) fn parse_ver(s: &str) -> Option<(u32, u32, u32, u8)> {
    let trimmed = s.trim().trim_start_matches(['v', 'V']);
    // Strip build metadata first, then split off any prerelease tag.
    let no_build = trimmed.split('+').next().unwrap_or(trimmed);
    let mut parts = no_build.splitn(2, '-');
    let core = parts.next().unwrap_or(no_build);
    let is_prerelease = parts.next().is_some_and(|p| !p.is_empty());
    let mut it = core.split('.');
    let maj = it.next()?.parse::<u32>().ok()?;
    let min = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let pat = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((maj, min, pat, u8::from(!is_prerelease)))
}

/// A newer release the user has not installed yet, published by the auto-check
/// when `update_auto_install` is off (the default). The About window's pill and
/// the tray tooltip read this so "an update is waiting" is visible without the
/// app having replaced its own binary behind the user's back.
static PENDING_UPDATE: Mutex<Option<String>> = Mutex::new(None);

/// The tag of a newer release found by the last auto-check, if any.
pub fn pending_update() -> Option<String> {
    PENDING_UPDATE.lock().ok().and_then(|g| g.clone())
}

fn client() -> Option<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .ok()
}

/// The last successful `fetch_latest_json` payload, held so the install step
/// can reuse the JSON the user just said yes to instead of re-fetching — the
/// SECURITY.md promise is **one anonymous row per check**, and a second fetch
/// would log a second row. `latest_exe_asset` *takes* it (single use), so a
/// manual install path with no prior check still fetches fresh.
static LAST_LATEST_JSON: Mutex<Option<serde_json::Value>> = Mutex::new(None);

fn fetch_latest_json() -> Option<serde_json::Value> {
    // ?v= reports the running version for the endpoint's anonymous
    // version-adoption stats. The server also falls back to parsing the
    // User-Agent, but the explicit param is its preferred channel and
    // survives any edge/CDN header-forwarding change.
    let url = format!("{RELEASES_API}?v={}", env!("CARGO_PKG_VERSION"));
    let mut req = client()?.get(url);
    // Only the latest-release check carries the install id — the binary
    // download in download_and_install() goes to GitHub and must not.
    if let Some(id) = INSTALL_ID.get() {
        req = req.header("X-Install-Id", id.as_str());
    }
    let primary = match req.send() {
        Ok(resp) if resp.status().is_success() => resp.json::<serde_json::Value>().ok(),
        Ok(resp) => {
            tracing::info!("update: releases API returned HTTP {}", resp.status());
            None
        }
        Err(e) => {
            tracing::info!("update: releases API unreachable: {e}");
            None
        }
    };
    // Studio did not answer usefully. Ask GitHub directly rather than leaving this install
    // unable to ever discover a release again.
    let json = match primary {
        Some(j) => j,
        None => fetch_github_fallback_json()?,
    };
    if let Ok(mut cached) = LAST_LATEST_JSON.lock() {
        *cached = Some(json.clone());
    }
    Some(json)
}

/// Ask GitHub directly when the Studio proxy fails.
///
/// Deliberately carries no `X-Install-Id` and no `?v=`: this is a plain unauthenticated read,
/// so it stays inside GitHub's anonymous rate limit and logs no analytics row, which keeps the
/// SECURITY.md promise of one anonymous row per check intact (a fallback logs none at all).
fn fetch_github_fallback_json() -> Option<serde_json::Value> {
    let resp = client()?.get(GITHUB_LATEST_API).send().ok()?;
    if !resp.status().is_success() {
        tracing::info!("update: GitHub fallback returned HTTP {}", resp.status());
        return None;
    }
    resp.json().ok()
}

/// One real network check: latest tag vs compiled-in version.
pub fn check() -> UpdateCheck {
    let Some(json) = fetch_latest_json() else {
        return UpdateCheck::Failed;
    };
    let Some(tag) = json.get("tag_name").and_then(|v| v.as_str()) else {
        return UpdateCheck::Failed;
    };
    match (parse_ver(tag), parse_ver(env!("CARGO_PKG_VERSION"))) {
        (Some(latest), Some(current)) if latest > current => {
            UpdateCheck::Available(tag.trim_start_matches(['v', 'V']).to_string())
        }
        (Some(_), Some(_)) => UpdateCheck::UpToDate,
        _ => UpdateCheck::Failed, // unparseable tag — don't guess
    }
}
