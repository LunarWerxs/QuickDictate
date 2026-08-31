//! Optional, opt-in **"Sync my settings with Connections."**
//!
//! Implements LunarWerx's portable Connections settings-sync protocol for a
//! native Windows app: loopback-redirect OAuth (Authorization Code + PKCE,
//! public client, no secret) against `accounts.connections.icu`, and raw-HTTP
//! calls to the live per-user settings store at `studio.connections.icu/v1/app-data`.
//!
//! Design choices, all matched to how the rest of QuickDictate is built:
//!   * **Blocking `reqwest` on worker threads** (same pattern as `update.rs`) —
//!     the egui Settings window spawns these and drains results over an `mpsc`
//!     channel, never blocking a frame.
//!   * **Zero new crates.** PKCE hashing / randomness use the Windows CNG
//!     (`BCryptHash` / `BCryptGenRandom`, exactly like `update.rs::sha256_hex`);
//!     the refresh token is sealed with **DPAPI** (`CryptProtectData`, CurrentUser
//!     scope) — machine+user bound, so copying the portable folder to another PC
//!     simply asks the user to sign in again there.
//!   * **Only portable preferences and numeric usage totals sync**
//!     ([`SYNCED_KEYS`](schema::SYNCED_KEYS)). API keys, transcript text, window geometry,
//!     `run_at_startup`, and logging flags never leave the machine.
//!
//! The access token lives only in a worker's stack for the duration of one call;
//! it is never persisted. Only the refresh token (+ a display email/name) is
//! stored, DPAPI-sealed, next to the exe as `quickdictate-connections.dat`.

mod creds;
mod guard;
mod oauth;
mod schema;
mod store;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::state::App;

// Re-exported so `crate::sync::<name>` keeps resolving exactly as it did when
// this was one file; the orchestration below reads them through these too.
pub use creds::{clear_creds, is_signed_in, load_creds, save_creds, Creds};
pub use oauth::{fetch_avatar, refresh, sign_in, Tokens};
pub use schema::{apply_synced_to_config, snapshot_to_synced, synced_stats};
pub use store::{store_delete, store_pull, store_push};

use oauth::fetch_userinfo;
use schema::merge_stats;
use store::{clear_store_cache, CachedRemoteDoc};

// ---- Public constants ------------------------------------------------------

/// QuickDictate's OAuth `client_id` — **also its store `appId`**. Public value
/// (a PKCE public client ships no secret), registered once by the owner:
/// `POST https://studio.connections.icu/v1/oauth-apps` with `openid profile
/// email` scopes and bare-host loopback redirect URIs. Safe to commit.
pub const CLIENT_ID: &str = "6448e5f7a13816eb3cbfc7e406570bdf";

const AUTH_URL: &str = "https://accounts.connections.icu/oauth/authorize";
const TOKEN_URL: &str = "https://accounts.connections.icu/oauth/token";
const USERINFO_URL: &str = "https://accounts.connections.icu/oauth/userinfo";
const STORE_BASE: &str = "https://studio.connections.icu/v1/app-data";
const SCOPES: &str = "openid profile email photo";
const REDIRECT_PATH: &str = "/oauth/callback";
const CREDS_FILE: &str = "quickdictate-connections.dat";
const USER_AGENT: &str = concat!("QuickDictate/", env!("CARGO_PKG_VERSION"));
const STATS_KEY: &str = "usage_stats";

/// How long we wait for the user to complete sign-in in their browser.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_AVATAR_BYTES: u64 = 5 * 1024 * 1024;
const MAX_AVATAR_DIMENSION: u32 = 2048;
const MAX_DOCUMENT_BYTES: usize = 64 * 1024;
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(30);

/// Serializes the refresh-token-using operations (`resume_and_pull`,
/// `push_now`, `disconnect`). Without it, a Settings-open resume racing a
/// Save & Restart push could fire two concurrent `refresh` exchanges and
/// interleave writes to the sealed creds file. Held only for the duration of
/// one operation. Interactive sign-in only takes the lock after the browser
/// round-trip, before it exposes credentials or touches the store.
static SYNC_LOCK: Mutex<()> = Mutex::new(());
static STATS_SYNC_QUEUED: AtomicBool = AtomicBool::new(false);
static STORE_CACHE: Mutex<Option<CachedRemoteDoc>> = Mutex::new(None);

/// Acquire [`SYNC_LOCK`], recovering the guard even if a previous holder
/// panicked (the lock guards ordering, not invariant-bearing data).
fn sync_guard() -> std::sync::MutexGuard<'static, ()> {
    SYNC_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn store_cache() -> std::sync::MutexGuard<'static, Option<CachedRemoteDoc>> {
    STORE_CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- High-level orchestration (called on worker threads by the UI) ---------

/// Result of a sign-in / resume, ready for the UI thread to apply.
pub struct Connected {
    /// Display name from /oauth/userinfo (empty for creds saved before we fetched it — backfilled
    /// on the next resume). The UI prefers this over the privacy-relay `email`.
    pub name: String,
    pub email: String,
    /// Decoded avatar `(w, h, rgba8)` from the profile picture, decoded off the UI thread. The UI
    /// thread uploads it as an egui texture. `None` → no avatar (initials/no image).
    pub avatar: Option<(u32, u32, Vec<u8>)>,
    /// `Some(settings)` if the cloud had a saved doc to apply locally; `None`
    /// if we just seeded an empty cloud with the local settings.
    pub remote: Option<Value>,
    pub seeded: bool,
}

/// Interactive first connect: sign in → persist creds → pull; if the cloud is
/// empty, seed it with `local_snapshot`.
pub fn connect_and_reconcile(local_snapshot: Value) -> Result<Connected> {
    let tokens = sign_in()?;
    let _guard = sync_guard();
    clear_store_cache();
    if tokens.refresh_token.is_empty() {
        tracing::warn!("connections: no refresh_token returned; sync won't survive restart");
    } else {
        let _ = save_creds(&Creds {
            refresh_token: tokens.refresh_token.clone(),
            sub: tokens.sub.clone(),
            email: tokens.email.clone(),
            name: tokens.name.clone(),
            picture: tokens.picture.clone(),
        });
    }
    let avatar = fetch_avatar(&tokens.picture);
    let doc = store_pull(&tokens.access_token)?;
    if doc.version == 0 {
        store_push(&tokens.access_token, &local_snapshot, 0)?;
        Ok(Connected {
            name: tokens.name,
            email: tokens.email,
            avatar,
            remote: None,
            seeded: true,
        })
    } else {
        let mut merged = doc.settings;
        if merge_stats(&mut merged, &local_snapshot) {
            store_push(&tokens.access_token, &merged, doc.version)?;
        }
        Ok(Connected {
            name: tokens.name,
            email: tokens.email,
            avatar,
            remote: Some(merged),
            seeded: false,
        })
    }
}

/// Silent resume on Settings-window open when creds already exist: refresh →
/// pull. Returns the remote doc to apply (if any).
pub fn resume_and_pull(local_snapshot: Value) -> Result<Connected> {
    let _guard = sync_guard();
    let mut creds = load_creds().ok_or_else(|| anyhow!("not signed in"))?;
    let tokens = refresh(&creds.refresh_token)?;
    persist_rotated(&creds, &tokens);
    // Backfill the display name/email for creds saved before we fetched userinfo (older builds
    // decoded identity from the id_token, which carries neither). One-time re-seal on next resume.
    if creds.name.is_empty() || creds.email.is_empty() || creds.picture.is_empty() {
        let (email, name, picture) = fetch_userinfo(&tokens.access_token);
        let changed = (!name.is_empty() && name != creds.name)
            || (!email.is_empty() && email != creds.email)
            || (!picture.is_empty() && picture != creds.picture);
        if changed {
            if !name.is_empty() {
                creds.name = name;
            }
            if !email.is_empty() {
                creds.email = email;
            }
            if !picture.is_empty() {
                creds.picture = picture;
            }
            let _ = save_creds(&Creds {
                refresh_token: tokens.refresh_token.clone(),
                ..creds.clone()
            });
        }
    }
    let avatar = fetch_avatar(&creds.picture);
    let doc = store_pull(&tokens.access_token)?;
    let mut merged = doc.settings;
    if doc.version > 0 && merge_stats(&mut merged, &local_snapshot) {
        store_push(&tokens.access_token, &merged, doc.version)?;
    }
    Ok(Connected {
        name: creds.name,
        email: creds.email,
        avatar,
        remote: (doc.version > 0).then_some(merged),
        seeded: false,
    })
}

/// Push the current local snapshot to the cloud (used on Save). Refresh-aware.
pub fn push_now(mut local_snapshot: Value) -> Result<u64> {
    let _guard = sync_guard();
    let creds = load_creds().ok_or_else(|| anyhow!("not signed in"))?;
    let tokens = refresh(&creds.refresh_token)?;
    persist_rotated(&creds, &tokens);
    let remote = store_pull(&tokens.access_token)?;
    merge_stats(&mut local_snapshot, &remote.settings);
    store_push(&tokens.access_token, &local_snapshot, remote.version)
}

/// Coalesce successful dictations into a quiet background stats push whenever
/// the user has opted into Connections sync.
pub fn schedule_stats_push(app: Arc<App>) {
    if !is_signed_in() || STATS_SYNC_QUEUED.swap(true, Ordering::AcqRel) {
        return;
    }
    if std::thread::Builder::new()
        .name("qd-stats-sync".into())
        .spawn(move || {
            // A short debounce captures quick successive dictations in one
            // request and keeps network work away from the transcription path.
            std::thread::sleep(Duration::from_secs(3));
            let config = app.config.load();
            let snapshot = snapshot_to_synced(&config, &app.stats.snapshot());
            if let Err(error) = push_now(snapshot) {
                tracing::warn!("connections: background stats sync failed: {error}");
            }
            STATS_SYNC_QUEUED.store(false, Ordering::Release);
        })
        .is_err()
    {
        STATS_SYNC_QUEUED.store(false, Ordering::Release);
    }
}

/// Disconnect: best-effort delete the remote doc, then always drop local creds.
pub fn disconnect() {
    let _guard = sync_guard();
    if let Some(creds) = load_creds() {
        if let Ok(tokens) = refresh(&creds.refresh_token) {
            let _ = store_delete(&tokens.access_token);
        }
    }
    clear_creds();
    clear_store_cache();
}

/// Best-effort final flush used by the process shutdown path. The worker is
/// bounded so a dead network cannot hold Quit indefinitely.
pub fn flush_before_exit(app: &Arc<App>, timeout: Duration) {
    if !is_signed_in() {
        return;
    }
    let config = app.config.load();
    let snapshot = snapshot_to_synced(&config, &app.stats.snapshot());
    let (tx, rx) = std::sync::mpsc::channel();
    if std::thread::Builder::new()
        .name("qd-sync-flush".into())
        .spawn(move || {
            let _ = tx.send(push_now(snapshot));
        })
        .is_err()
    {
        tracing::warn!("connections: could not start final sync flush");
        return;
    }
    match rx.recv_timeout(timeout) {
        Ok(Ok(_)) => tracing::info!("connections: final sync flush completed"),
        Ok(Err(error)) => tracing::warn!("connections: final sync flush failed: {error}"),
        Err(_) => tracing::warn!(
            "connections: final sync flush did not finish within {} seconds",
            timeout.as_secs()
        ),
    }
}

/// Re-seal creds if a refresh rotated the refresh token. OAuth servers
/// commonly invalidate the OLD refresh token the instant a rotated one is
/// issued, so a failed write here is not a safe no-op: the in-memory session
/// keeps working for the rest of this run, but the NEXT launch would load the
/// now-dead old token off disk and get an opaque "token refresh failed" with
/// no clue why. Rather than leave that trap, a save failure on a genuine
/// rotation logs the real cause and clears the local creds, so the next
/// launch presents a clean signed-out state the user can act on (sign in
/// again) instead of a confusing refresh error. This is distinct from the
/// benign identity-backfill re-seal in `resume_and_pull` (same `save_creds`,
/// but backfilling a display name/email, not persisting a rotated token) —
/// that path is left as best-effort, unchanged.
fn persist_rotated(old: &Creds, fresh: &Tokens) {
    if fresh.refresh_token.is_empty() || fresh.refresh_token == old.refresh_token {
        return;
    }
    let rotated = Creds {
        refresh_token: fresh.refresh_token.clone(),
        ..old.clone()
    };
    // One retry after a beat: the classic failure here is an AV scanner or
    // backup tool briefly holding the file, which clears in milliseconds.
    // Rotation happens on ROUTINE background refreshes, so treating one
    // transient write failure as a sign-out forced a full browser re-login
    // for a disk hiccup the user never saw. Only after both attempts fail do
    // we keep the old file and say so loudly; the next launch then shows a
    // refresh error rather than a silent sign-out, which at least names the
    // moment things went wrong.
    if let Err(first) = save_creds(&rotated) {
        std::thread::sleep(Duration::from_millis(250));
        if let Err(second) = save_creds(&rotated) {
            tracing::error!(
                "connections: failed twice to persist a rotated refresh token \
                 (first: {first}; retry: {second}); the old token may already be \
                 invalidated server-side, so the next launch's sync resume may \
                 fail and ask you to sign in again"
            );
        }
    }
}
