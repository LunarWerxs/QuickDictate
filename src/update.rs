//! Check-for-update + silent portable self-update.
//!
//! Modeled directly on SageThumbs 2K's updater: the GitHub "latest release"
//! JSON (reached via LunarWerx's Studio proxy — see [`RELEASES_API`]),
//! lenient `vX.Y.Z` tag parsing with a plain tuple compare, a daily-throttled
//! on-disk cache so we hit the network at most once per day, and a
//! MZ-header + size + SHA-256 verified download. The install step differs
//! because QuickDictate is a **portable single exe** (no Inno Setup): instead
//! of launching a `/SILENT` installer we swap the exe in place —
//! `quickdictate.exe` → `quickdictate.exe.old`, new file in, relaunch with
//! `--updated <ver>` — which is the portable equivalent of a silent update.
//!
//! Trigger points:
//!   * startup auto-check (gated by `update_auto_check` in settings, default
//!     on; throttled to one network hit per 24 h via the cache file)
//!   * tray menu "Check for Updates…" (always available, ignores throttle)
//!   * the About window's status pill calls [`check`] directly.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_TOPMOST,
    MB_YESNO,
};

use crate::config::Config;
use crate::state::App;

/// "Latest release" endpoint: the Connections Studio proxy, which relays
/// GitHub's `releases/latest` JSON for LunarWerxs/QuickDictate **verbatim**
/// (so parsing here is unchanged from the GitHub API) and logs one anonymous
/// analytics row per hit as an install-count statistic — no personal data,
/// 90-day retention; the request carries the `X-Install-Id` header resolved
/// by [`init_install_id`] plus the app version (`?v=`, for anonymous
/// version-adoption stats). See SECURITY.md for the full disclosure. Release
/// *binaries* still download straight from GitHub via the asset URLs in the
/// payload. On any failure the check reports Failed — which the auto path
/// treats as silence.
pub const RELEASES_API: &str = "https://studio.connections.icu/v1/app/quickdictate/latest";
pub const RELEASES_URL: &str = "https://github.com/LunarWerxs/QuickDictate/releases";

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateCheck {
    UpToDate,
    /// Newer version available; payload is the tag without the `v` prefix.
    Available(String),
    Failed,
}

/// Lenient `vX.Y.Z` / `X.Y` / `X.Y.Z-rc1` parser (copied from SageThumbs).
fn parse_ver(s: &str) -> Option<(u32, u32, u32)> {
    let core = s.trim().trim_start_matches(['v', 'V']);
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut it = core.split('.');
    let maj = it.next()?.parse::<u32>().ok()?;
    let min = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let pat = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((maj, min, pat))
}

fn client() -> Option<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .ok()
}

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
    let resp = req.send().ok()?;
    if !resp.status().is_success() {
        tracing::info!("update: releases API returned HTTP {}", resp.status());
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

// ---------------------------------------------------------------------------
// Anonymous install id (X-Install-Id)
// ---------------------------------------------------------------------------

/// Crypto-random UUIDv4 via CNG (`BCryptGenRandom`, the same checked call as
/// `sync.rs::rand_bytes`). Deliberately **never** derived from hostname, MAC,
/// username, or any other machine identifier — the id must identify nothing
/// but itself. `None` if the system RNG fails (no id beats a predictable one).
fn new_install_id() -> Option<String> {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut b = [0u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            BCRYPT_ALG_HANDLE::default(),
            &mut b,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if !status.is_ok() {
        return None;
    }
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    ))
}

/// Resolve the anonymous install id and cache it for [`fetch_latest_json`]:
/// reuse the one persisted in settings.json, or on the very first launch
/// generate a fresh UUID and persist it (via [`Config::save_install_id`],
/// which fills the template's empty slot in place rather than rewriting the
/// whole file). Called once from `main()` before any check can run — both
/// the startup auto-check and the tray/About manual path (which has no `App`
/// handle) read the cached value. An id that failed to persist is **not**
/// sent: it would change every launch and inflate the install count.
pub fn init_install_id(app: &App) {
    let cfg = app.config.load();
    let existing = cfg.install_id.trim();
    if !existing.is_empty() {
        let _ = INSTALL_ID.set(existing.to_string());
        return;
    }
    let Some(id) = new_install_id() else {
        tracing::warn!("update: system RNG failed; checks will carry no install id");
        return;
    };
    let mut new_cfg = (**cfg).clone();
    new_cfg.install_id = id.clone();
    match new_cfg.save_install_id(&Config::settings_path()) {
        Ok(()) => {
            app.config.store(Arc::new(new_cfg));
            let _ = INSTALL_ID.set(id);
            tracing::info!("update: generated anonymous install id");
        }
        Err(e) => {
            tracing::warn!("update: could not persist install id ({e}); checks will carry none");
        }
    }
}

// ---------------------------------------------------------------------------
// Throttle cache
// ---------------------------------------------------------------------------

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|p| p.to_path_buf())
}

fn cache_path() -> Option<PathBuf> {
    exe_dir().map(|d| d.join(CACHE_FILE))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_cache() -> Option<(u64, String)> {
    let text = std::fs::read_to_string(cache_path()?).ok()?;
    let mut lines = text.lines();
    let ts = lines.next()?.trim().parse::<u64>().ok()?;
    let tag = lines.next().unwrap_or("").trim().to_string();
    Some((ts, tag))
}

fn write_cache(tag: &str) {
    if let Some(p) = cache_path() {
        let _ = std::fs::write(p, format!("{}\n{}\n", now_secs(), tag));
    }
}

// ---------------------------------------------------------------------------
// Download, verify, swap, relaunch
// ---------------------------------------------------------------------------

struct Asset {
    url: String,
    size: u64,
    sha256: Option<String>,
}

/// Pick the release's exe asset. Prefers a name containing "quickdictate";
/// falls back to the first `.exe` asset.
fn latest_exe_asset() -> Option<(String, Asset)> {
    let json = fetch_latest_json()?;
    let tag = json
        .get("tag_name")?
        .as_str()?
        .trim_start_matches(['v', 'V'])
        .to_string();
    let assets = json.get("assets")?.as_array()?;
    let pick = |pred: &dyn Fn(&str) -> bool| -> Option<&serde_json::Value> {
        assets.iter().find(|a| {
            a.get("name")
                .and_then(|n| n.as_str())
                .map(|n| {
                    let n = n.to_ascii_lowercase();
                    n.ends_with(".exe") && pred(&n)
                })
                .unwrap_or(false)
        })
    };
    let asset = pick(&|n| n.contains("quickdictate")).or_else(|| pick(&|_| true))?;
    let sha256 = asset
        .get("digest")
        .and_then(|d| d.as_str())
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(|h| h.to_ascii_lowercase());
    Some((
        tag,
        Asset {
            url: asset.get("browser_download_url")?.as_str()?.to_string(),
            size: asset.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
            sha256,
        },
    ))
}

/// SHA-256 via Windows CNG (`BCryptHash` one-shot with the SHA-256
/// pseudo-handle) — no extra crate, same approach as SageThumbs.
fn sha256_hex(bytes: &[u8]) -> Option<String> {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, bytes, &mut out) };
    if status.is_ok() {
        Some(out.iter().map(|b| format!("{b:02x}")).collect())
    } else {
        None
    }
}

/// MZ header + exact size + (when the release carried a digest) SHA-256.
fn verify_exe_bytes(bytes: &[u8], asset: &Asset) -> bool {
    if bytes.len() < 2 || &bytes[..2] != b"MZ" {
        tracing::warn!("update: downloaded file is not a Windows executable");
        return false;
    }
    if asset.size != 0 && bytes.len() as u64 != asset.size {
        tracing::warn!(
            "update: size mismatch (got {}, expected {})",
            bytes.len(),
            asset.size
        );
        return false;
    }
    if let Some(want) = &asset.sha256 {
        if sha256_hex(bytes).as_deref() != Some(want.as_str()) {
            tracing::warn!("update: sha256 mismatch — refusing to install");
            return false;
        }
    } else {
        tracing::warn!("update: release had no sha256 digest; verified MZ + size only");
    }
    true
}

/// Download the new exe, verify it, swap it into place, relaunch, and signal
/// shutdown. Returns a user-facing error string on failure.
fn download_and_install(app: &App, tag: &str) -> Result<(), String> {
    let (asset_tag, asset) = latest_exe_asset().ok_or("could not resolve a release .exe asset")?;
    if asset_tag != tag {
        tracing::info!("update: release moved while prompting ({tag} -> {asset_tag}); continuing");
    }
    if asset.size > MAX_EXE_BYTES {
        return Err("release asset is implausibly large".into());
    }

    tracing::info!("update: downloading {}", asset.url);
    let resp = client()
        .ok_or("http client init failed")?
        .get(&asset.url)
        .send()
        .map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().map_err(|e| format!("download failed: {e}"))?;
    if bytes.len() as u64 > MAX_EXE_BYTES {
        return Err("downloaded file exceeds the size cap".into());
    }
    if !verify_exe_bytes(&bytes, &asset) {
        return Err("downloaded file failed verification".into());
    }

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let new = exe.with_extension("exe.new");
    let old = exe.with_extension("exe.old");
    std::fs::write(&new, &bytes).map_err(|e| format!("write {}: {e}", new.display()))?;
    // Re-read + re-verify from disk to close the TOCTOU window (as SageThumbs
    // does before launching its installer).
    let reread = std::fs::read(&new).map_err(|e| format!("re-read: {e}"))?;
    if !verify_exe_bytes(&reread, &asset) {
        let _ = std::fs::remove_file(&new);
        return Err("on-disk verification failed".into());
    }

    // The swap: a running exe can be renamed on Windows, just not deleted.
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("rename current exe: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        // Roll back so the app still launches next time.
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("swap in new exe: {e}"));
    }

    tracing::info!("update: swapped to v{tag}; relaunching");
    std::process::Command::new(&exe)
        .args(["--updated", tag])
        .spawn()
        .map_err(|e| format!("relaunch: {e}"))?;
    app.shutdown.store(true, Ordering::Release);
    Ok(())
}

// ---------------------------------------------------------------------------
// User-facing flows (worker threads; MessageBoxes are fine off the UI thread)
// ---------------------------------------------------------------------------

pub(crate) fn msg_box(
    title: &str,
    body: &str,
    style: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE,
) -> windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_RESULT {
    let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let body_w: Vec<u16> = body.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            HWND::default(),
            PCWSTR(body_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            style | MB_TOPMOST,
        )
    }
}

fn prompt_and_install(app: &App, tag: &str) {
    let cur = env!("CARGO_PKG_VERSION");
    let body = format!(
        "QuickDictate v{tag} is available (you have v{cur}).\n\n\
         Download and update now? QuickDictate will restart itself."
    );
    if msg_box("QuickDictate update", &body, MB_YESNO | MB_ICONQUESTION) != IDYES {
        return;
    }
    if let Err(e) = download_and_install(app, tag) {
        tracing::error!("update: {e}");
        msg_box(
            "QuickDictate update failed",
            &format!("{e}\n\nYou can download the update manually from:\n{RELEASES_URL}"),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// Startup auto-check (settings `update_auto_check`, default on). Throttled to
/// one network hit per 24 h; silent unless an update is actually available.
pub fn spawn_startup_check(app: Arc<App>) {
    if IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        let fresh = read_cache()
            .map(|(ts, _)| now_secs().saturating_sub(ts) < CHECK_INTERVAL_SECS)
            .unwrap_or(false);
        if fresh {
            tracing::debug!("update: skipping auto-check (cache fresh)");
        } else {
            match check() {
                UpdateCheck::Available(tag) => {
                    write_cache(&tag);
                    tracing::info!("update: v{tag} available");
                    prompt_and_install(&app, &tag);
                }
                UpdateCheck::UpToDate => {
                    write_cache(env!("CARGO_PKG_VERSION"));
                    tracing::info!("update: up to date");
                }
                UpdateCheck::Failed => {
                    // Silent: no release yet / offline is not the user's problem.
                    tracing::info!("update: auto-check failed (silent)");
                }
            }
        }
        IN_FLIGHT.store(false, Ordering::Release);
    });
}

/// Startup housekeeping: delete the `.old` exe left by a previous self-update,
/// and show the post-update notice when relaunched with `--updated <ver>`.
pub fn handle_startup_artifacts() {
    if let Ok(exe) = std::env::current_exe() {
        let old = exe.with_extension("exe.old");
        if old.exists() {
            match std::fs::remove_file(&old) {
                Ok(()) => tracing::info!("update: removed leftover {}", old.display()),
                // The old instance may still be exiting; next launch gets it.
                Err(e) => tracing::debug!("update: could not remove {} yet: {e}", old.display()),
            }
        }
    }
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--updated") {
        let ver = args.get(i + 1).cloned().unwrap_or_default();
        tracing::info!("update: relaunched after update to v{ver}");
        // Non-blocking equivalent of SageThumbs' post-update tray toast.
        std::thread::spawn(move || {
            msg_box(
                "QuickDictate updated",
                &format!("You're now on version {ver}."),
                MB_OK | MB_ICONINFORMATION,
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_prefixed_tags() {
        assert_eq!(parse_ver("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_ver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_ver("V2.0"), Some((2, 0, 0)));
        assert_eq!(parse_ver("1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_ver("1.2.3+build7"), Some((1, 2, 3)));
        assert_eq!(parse_ver("garbage"), None);
        assert_eq!(parse_ver(""), None);
    }

    #[test]
    fn tuple_compare_orders_versions() {
        assert!(parse_ver("0.2.0") > parse_ver("0.1.9"));
        assert!(parse_ver("1.0.0") > parse_ver("0.99.99"));
        assert!(parse_ver("0.1.0") == parse_ver("v0.1.0"));
        assert!(parse_ver("0.1.1") > parse_ver(env!("CARGO_PKG_VERSION")).map(|_| (0, 1, 0)));
    }

    #[test]
    fn install_id_is_a_lowercase_v4_uuid_and_unique() {
        let a = new_install_id().expect("system RNG available");
        let b = new_install_id().expect("system RNG available");
        assert_ne!(a, b, "two ids must not collide");
        assert_eq!(a.len(), 36);
        for (i, ch) in a.chars().enumerate() {
            match i {
                8 | 13 | 18 | 23 => assert_eq!(ch, '-', "dash expected at {i} in {a}"),
                _ => assert!(
                    matches!(ch, '0'..='9' | 'a'..='f'),
                    "lowercase hex expected at {i} in {a}"
                ),
            }
        }
        assert_eq!(&a[14..15], "4", "version nibble in {a}");
        assert!(
            matches!(&a[19..20], "8" | "9" | "a" | "b"),
            "RFC 4122 variant nibble in {a}"
        );
    }

    #[test]
    #[ignore = "live network"]
    fn live_studio_latest_release_parses() {
        // The Studio proxy must relay GitHub's releases/latest JSON verbatim —
        // the same fields check() and latest_exe_asset() consume. NOTE: each
        // run logs one anonymous analytics row on the endpoint.
        let resp = client()
            .unwrap()
            .get(RELEASES_API)
            .send()
            .expect("Studio endpoint reachable");
        assert!(resp.status().is_success(), "HTTP {}", resp.status());
        let json: serde_json::Value = resp.json().unwrap();
        let tag = json.get("tag_name").and_then(|v| v.as_str()).unwrap();
        println!("latest QuickDictate tag = {tag}");
        assert!(parse_ver(tag).is_some(), "tag {tag} should parse");
    }

    #[test]
    #[ignore = "live network"]
    fn live_github_latest_release_parses() {
        // The Studio endpoint proxies GitHub's releases/latest verbatim, so
        // GitHub's shape is still the contract; validate the fetch + tag-parse
        // path against the sibling LunarWerx project's public releases.
        let resp = client()
            .unwrap()
            .get("https://api.github.com/repos/LunarWerxs/SageThumbs-2k/releases/latest")
            .send()
            .expect("GitHub API reachable");
        assert!(resp.status().is_success(), "HTTP {}", resp.status());
        let json: serde_json::Value = resp.json().unwrap();
        let tag = json.get("tag_name").and_then(|v| v.as_str()).unwrap();
        println!("latest SageThumbs tag = {tag}");
        assert!(parse_ver(tag).is_some(), "tag {tag} should parse");
    }

    #[test]
    fn sha256_matches_known_vector() {
        // SHA-256("abc") — canonical NIST test vector.
        assert_eq!(
            sha256_hex(b"abc").as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[test]
    fn verify_rejects_bad_bytes() {
        let asset = Asset {
            url: String::new(),
            size: 4,
            sha256: None,
        };
        assert!(!verify_exe_bytes(b"PK\x03\x04", &asset)); // not MZ
        assert!(verify_exe_bytes(b"MZ\x90\x00", &asset)); // MZ + right size
        let wrong_size = Asset {
            url: String::new(),
            size: 5,
            sha256: None,
        };
        assert!(!verify_exe_bytes(b"MZ\x90\x00", &wrong_size));
        let bad_hash = Asset {
            url: String::new(),
            size: 4,
            sha256: Some("00".repeat(32)),
        };
        assert!(!verify_exe_bytes(b"MZ\x90\x00", &bad_hash));
    }
}
