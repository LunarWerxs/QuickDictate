//! Downloading a release, proving it is the release, and swapping it in.
//!
//! A portable single exe has no installer, so the swap is the install: verify
//! the bytes, rename the old exe aside, put the new one in, relaunch.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::*;

// ---------------------------------------------------------------------------
// Download, verify, swap, relaunch
// ---------------------------------------------------------------------------

pub(crate) struct Asset {
    pub(super) url: String,
    pub(super) size: u64,
    pub(super) sha256: String,
}

pub fn trusted_asset_url(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .path()
            .starts_with("/LunarWerxs/QuickDictate/releases/download/")
}

/// Resolve only the stable, human-facing `quickdictate.exe` asset. Exact naming keeps the
/// updater independent of GitHub upload order and prevents a future helper/debug executable
/// from being mistaken for the portable application.
pub fn exe_asset_from_json(json: &serde_json::Value) -> Option<(String, Asset)> {
    let tag = json
        .get("tag_name")?
        .as_str()?
        .trim_start_matches(['v', 'V'])
        .to_string();
    let asset = json.get("assets")?.as_array()?.iter().find(|a| {
        a.get("name")
            .and_then(|n| n.as_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("quickdictate.exe"))
    })?;
    let sha256 = asset
        .get("digest")
        .and_then(|d| d.as_str())
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)?;
    let url = asset.get("browser_download_url")?.as_str()?;
    if !trusted_asset_url(url) {
        tracing::warn!("update: refusing an unexpected release asset URL");
        return None;
    }
    Some((
        tag,
        Asset {
            url: url.to_string(),
            size: asset.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
            sha256,
        },
    ))
}

/// Reuse the JSON from the check that prompted this install (one ping per check, as
/// SECURITY.md promises); fetch fresh only if no check preceded it.
pub(super) fn latest_exe_asset() -> Option<(String, Asset)> {
    let cached = LAST_LATEST_JSON.lock().ok().and_then(|mut g| g.take());
    let json = match cached {
        Some(json) => json,
        None => fetch_latest_json()?,
    };
    exe_asset_from_json(&json)
}

/// SHA-256 via Windows CNG (`BCryptHash` one-shot with the SHA-256
/// pseudo-handle) — no extra crate, same approach as SageThumbs.
pub(super) fn sha256_hex(bytes: &[u8]) -> Option<String> {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, bytes, &mut out) };
    if status.is_ok() {
        Some(out.iter().map(|b| format!("{b:02x}")).collect())
    } else {
        None
    }
}

/// MZ header + exact size + mandatory GitHub-provided SHA-256.
pub(super) fn verify_exe_bytes(bytes: &[u8], asset: &Asset) -> bool {
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
    if sha256_hex(bytes).as_deref() != Some(asset.sha256.as_str()) {
        tracing::warn!("update: sha256 mismatch — refusing to install");
        return false;
    }
    true
}

/// Prove the downloaded PE is actually the release it claims to be before replacing the
/// running application. `--version` exits before any settings/audio/UI side effects.
fn verify_exe_version(path: &Path, expected: &str) -> bool {
    // Bounded. `Command::output()` blocks forever if the child never exits,
    // and this child is a binary we just downloaded, so "it hangs" is squarely
    // in scope. Run the wait on a helper thread and give up after
    // VERSION_CHECK_TIMEOUT; a downloaded exe that will not answer --version
    // promptly has already failed the check.
    const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(20);
    let expected = expected.trim_start_matches(['v', 'V']).to_string();
    let path = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    let spawn = std::thread::Builder::new()
        .name("qd-update-verify".into())
        .spawn(move || {
            let verdict = std::process::Command::new(&path)
                .arg("--version")
                .output()
                .is_ok_and(|output| {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).trim() == expected
                });
            let _ = tx.send(verdict);
        });
    if spawn.is_err() {
        tracing::warn!("update: could not spawn the version self-check");
        return false;
    }
    match rx.recv_timeout(VERSION_CHECK_TIMEOUT) {
        Ok(verdict) => verdict,
        Err(_) => {
            tracing::warn!(
                "update: downloaded executable did not answer --version within \
                 {VERSION_CHECK_TIMEOUT:?}; refusing to install it"
            );
            false
        }
    }
}

/// Download the new exe, verify it, and swap it into place. Returns the path to
/// the now-current exe on success. Deliberately does **not** relaunch — the
/// caller decides when: the manual path relaunches immediately (see
/// [`download_and_install_now`]), the auto path defers if a dictation is live
/// (see [`spawn_startup_check`]). Because a swapped exe already takes effect on
/// the next launch, deferring costs nothing. A user-facing error string on
/// failure. The caller must serialize calls (via `IN_FLIGHT`) — the `.new` /
/// `.old` scratch names are fixed, so two swaps at once would race.
pub(super) fn download_and_swap(tag: &str) -> Result<PathBuf, String> {
    let (asset_tag, asset) = latest_exe_asset().ok_or("could not resolve a release .exe asset")?;
    if asset_tag != tag {
        tracing::info!("update: release moved while prompting ({tag} -> {asset_tag}); continuing");
    }
    if asset.size > MAX_EXE_BYTES {
        return Err("release asset is implausibly large".into());
    }

    tracing::info!("update: downloading {}", asset.url);
    let mut resp = client()
        .ok_or("http client init failed")?
        .get(&asset.url)
        .send()
        .map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    if resp.content_length().is_some_and(|n| n > MAX_EXE_BYTES) {
        return Err("downloaded file exceeds the size cap".into());
    }
    let mut bytes = Vec::with_capacity(asset.size.min(MAX_EXE_BYTES) as usize);
    resp.by_ref()
        .take(MAX_EXE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("download failed: {e}"))?;
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
    if !verify_exe_version(&new, &asset_tag) {
        let _ = std::fs::remove_file(&new);
        return Err("downloaded executable failed its version self-check".into());
    }

    // The swap: a running exe can be renamed on Windows, just not deleted.
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("rename current exe: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        // Roll back so the app still launches next time.
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("swap in new exe: {e}"));
    }
    Ok(exe)
}

/// Launch the freshly-swapped `exe` with `--updated <tag>` and signal the
/// running instance to shut down cleanly. Shutdown goes through the global
/// [`APP_HANDLE`] so both the auto path (which holds an `Arc<App>`) and the
/// manual About path (which does not) share one relaunch routine.
///
/// `reopen_about` adds `--show-about` so the new process reopens the About
/// window (see [`handle_startup_artifacts`]). Only the **manual** update (the
/// user clicked the About pill) sets it; a silent background auto-update stays
/// silent — no window pops up unprompted.
pub(super) fn relaunch(exe: &Path, tag: &str, reopen_about: bool) -> Result<(), String> {
    tracing::info!("update: swapped to v{tag}; relaunching");
    if let Some(app) = APP_HANDLE.get() {
        app.stats.flush();
    }
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--updated", tag]);
    if reopen_about {
        cmd.arg("--show-about");
    }
    cmd.spawn().map_err(|e| format!("relaunch: {e}"))?;
    if let Some(app) = APP_HANDLE.get() {
        app.shutdown.store(true, Ordering::Release);
    } else {
        // No handle published (shouldn't happen post-startup): the new process
        // is already up, so fall back to exiting this one directly rather than
        // leaving two instances running.
        tracing::warn!("update: no App handle for clean shutdown; exiting directly");
        std::process::exit(0);
    }
    Ok(())
}

/// Manual "update now" — the About window's status pill when a newer release is
/// waiting. Download + verify + swap the release, then relaunch immediately:
/// the user clicked the pill, so the click is the consent (no extra Yes/No).
/// Serialized against the auto-check via `IN_FLIGHT`. Returns an error string
/// on failure (the About worker surfaces it, with the manual-download link); on
/// success the process relaunches and this instance begins shutting down.
pub fn download_and_install_now(tag: &str) -> Result<(), String> {
    if IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return Err("an update is already in progress".into());
    }
    // Manual install from the About window → reopen About after the relaunch so
    // the user lands back where they were and sees the new version.
    let result = download_and_swap(tag).and_then(|exe| relaunch(&exe, tag, true));
    if result.is_ok() {
        if let Ok(mut slot) = PENDING_UPDATE.lock() {
            *slot = None;
        }
    }
    if result.is_err() {
        // Free the lock so a later retry can run. On success we intentionally
        // leave it set — the process is on its way out.
        IN_FLIGHT.store(false, Ordering::Release);
    }
    result
}
