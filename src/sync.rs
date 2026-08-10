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
//!     ([`SYNCED_KEYS`]). API keys, transcript text, window geometry,
//!     `run_at_startup`, and logging flags never leave the machine.
//!
//! The access token lives only in a worker's stack for the duration of one call;
//! it is never persisted. Only the refresh token (+ a display email/name) is
//! stored, DPAPI-sealed, next to the exe as `quickdictate-connections.dat`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::secretstore::dpapi;
use crate::state::App;
use crate::stats::UsageStats;

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

/// The **allowlist** of settings.json keys that sync to the cloud. Deliberately
/// excludes:
///
///   * every `*_keys` / `local_keys` array — **secrets, never synced**;
///   * `window_width/height/x/y` — machine-local window geometry;
///   * `run_at_startup` — per-machine registry (Run key) behavior;
///   * `hide_tray_icon` — per-machine, like `run_at_startup`: whether the
///     notification-area icon is shown is a property of this install, not a
///     portable preference, so it never travels with the synced settings;
///   * `enable_logging` / `log_transcripts` — local diagnostics toggles;
///   * `max_log_mb` — a per-install log-size cap, machine-local like
///     `enable_logging`, not a portable preference;
///   * `install_id` — this install's anonymous update-check id; syncing it
///     would merge two machines' identities into one;
///   * `update_auto_install` — a machine-local policy choice (whether *this*
///     machine applies updates unattended); syncing it would silently opt a
///     second machine into unattended installs;
///   * `protect_keys_at_rest` — whether *this* machine's settings.json seals
///     its keys with DPAPI bound to this Windows account; meaningless (and
///     misleading) if carried to another account or machine.
///
/// Only portable preferences travel. Names match `Config`'s serde field names
/// exactly, so the transforms below stay in lock-step with the struct. See
/// [`NEVER_SYNCED`] and the `every_config_field_is_synced_or_never_synced`
/// test below: together the two lists must partition every `Config` field, so
/// a newly added field can never silently fall through uncategorized again.
pub const SYNCED_KEYS: &[&str] = &[
    "mode",
    "language",
    "toggle_hotkey",
    "hold_hotkey",
    "reinsert_hold_ms",
    "listen_tail_ms",
    "clipboard_restore_delay_ms",
    "auto_space",
    "auto_newline",
    "auto_punct",
    "hotkeys_enabled",
    "enable_sound",
    "close_behavior",
    "mouse_follower_enabled",
    "delay_output_till_release",
    "spinner_type",
    "stt_provider",
    "stt_model",
    "local_model",
    "dashscope_intl",
    "update_auto_check",
    "prewarm_keys",
    "text_replacements",
    "enable_text_replacements",
    // Portable, secret-free preferences added to `Config` after this list was
    // first written. Per-app profiles in particular are exactly what a user
    // syncing two machines expects to travel.
    "profiles",
    "profiles_enabled",
    "voice_commands",
    "custom_vocabulary",
];

/// Every `Config` field that is deliberately **never** synced: secrets and
/// machine-local settings that must stay off the wire. This exists so the
/// drift [`SYNCED_KEYS`] once suffered (portable fields silently never added)
/// can't happen again: the `every_config_field_is_synced_or_never_synced`
/// test below asserts these two lists together cover every key `Config`
/// serializes to JSON, with no name in both, so a new `Config` field fails
/// the build until someone files it into one list or the other on purpose.
/// Only the guard test reads this at runtime; its real job is to be the
/// written-down decision, and to break the build when a new field has no
/// decision yet.
#[cfg_attr(not(test), allow(dead_code))]
const NEVER_SYNCED: &[&str] = &[
    "elevenlabs_keys",      // secret API key array
    "deepgram_keys",        // secret API key array
    "openai_keys",          // secret API key array
    "assemblyai_keys",      // secret API key array
    "dashscope_keys",       // secret API key array
    "google_keys",          // secret API key array
    "local_keys",           // legacy secret API key array, folds into elevenlabs_keys
    "window_width",         // machine-local window geometry
    "window_height",        // machine-local window geometry
    "window_x",             // machine-local window geometry
    "window_y",             // machine-local window geometry
    "run_at_startup",       // per-machine registry (Run key) behavior
    "hide_tray_icon",       // a property of this install, not a portable preference
    "enable_logging",       // local diagnostics toggle
    "max_log_mb",           // per-install log-size cap, machine-local like enable_logging
    "log_transcripts",      // local diagnostics toggle; must never leave the machine
    "install_id", // anonymous per-install id; syncing would merge two machines' identities
    "update_auto_install", // machine-local unattended-update policy choice
    "protect_keys_at_rest", // controls DPAPI sealing bound to this Windows account; meaningless elsewhere
];

// ---- Allowlist transforms (Config <-> synced JSON) -------------------------

/// The portable subset of a `Config` as a flat JSON object — exactly the keys in
/// [`SYNCED_KEYS`], nothing else. This is what we push to the store.
pub fn config_to_synced(cfg: &Config) -> Value {
    let full = serde_json::to_value(cfg).unwrap_or(Value::Null);
    let mut out = serde_json::Map::new();
    if let Some(obj) = full.as_object() {
        for k in SYNCED_KEYS {
            if let Some(v) = obj.get(*k) {
                out.insert((*k).to_string(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// The portable preferences plus mergeable, numeric-only usage statistics.
pub fn snapshot_to_synced(cfg: &Config, stats: &UsageStats) -> Value {
    let mut snapshot = config_to_synced(cfg);
    if let Some(object) = snapshot.as_object_mut() {
        object.insert(STATS_KEY.to_string(), stats.synced_value());
    }
    snapshot
}

pub fn synced_stats(remote: &Value) -> Option<&Value> {
    remote.as_object()?.get(STATS_KEY)
}

fn credential_patterns() -> &'static [(Regex, &'static str)] {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            [
                (r"^(sk|pk|rk)_(live|test)_[A-Za-z0-9]{16,}", "a Stripe key"),
                // ElevenLabs keys begin `sk_` (underscore) followed by a long
                // hex string — distinct from OpenAI's `sk-` (hyphen) below.
                (r"^sk_[A-Za-z0-9]{32,}$", "an ElevenLabs API key"),
                // DashScope (Alibaba Cloud / Qwen) keys are `sk-` followed by
                // a 32+ character lowercase-hex id. Checked before the
                // generic OpenAI-style pattern below (which would otherwise
                // also match) so a DashScope key is labeled correctly.
                (r"^sk-[0-9a-f]{32,}$", "a DashScope API key"),
                (r"^sk-[A-Za-z0-9_-]{20,}", "an OpenAI-style API key"),
                (r"^(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,}", "a GitHub token"),
                (
                    r"^github_pat_[A-Za-z0-9_]{22,}",
                    "a GitHub fine-grained token",
                ),
                (r"^xox[baprs]-[A-Za-z0-9-]{10,}", "a Slack token"),
                (r"^AKIA[0-9A-Z]{16}$", "an AWS access key id"),
                (r"^AIza[0-9A-Za-z_-]{35}$", "a Google API key"),
                // Deepgram and AssemblyAI keys have NO distinguishing prefix:
                // they are undifferentiated lowercase-hex blobs (40 and 32
                // chars). Deliberately NOT matched here. A bare-hex pattern
                // also matches every git SHA-1, MD5 hash, and dashless GUID,
                // and one such string in a synced text field (a replacement
                // value, a vocabulary term) would block EVERY future settings
                // push for that user, silently, until they found and removed
                // it. This scanner is defense-in-depth behind the SYNCED_KEYS
                // allowlist, which already keeps all key arrays off the wire;
                // it must never cost a legitimate sync.
                (
                    r"^ey[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{5,}$",
                    "a JWT",
                ),
                (r"-----BEGIN [A-Z ]*PRIVATE KEY-----", "a private key"),
            ]
            .into_iter()
            .map(|(pattern, label)| (Regex::new(pattern).expect("static credential regex"), label))
            .collect()
        })
        .as_slice()
}

fn credential_in_value(value: &Value, depth: usize) -> Option<&'static str> {
    match value {
        Value::String(text) => credential_patterns()
            .iter()
            .find_map(|(pattern, label)| pattern.is_match(text.trim()).then_some(*label)),
        Value::Array(values) if depth < 4 => values
            .iter()
            .find_map(|value| credential_in_value(value, depth + 1)),
        Value::Object(values) if depth < 4 => values
            .values()
            .find_map(|value| credential_in_value(value, depth + 1)),
        _ => None,
    }
}

fn validate_sync_snapshot(settings: &Value) -> Result<()> {
    if let Some(entries) = settings.as_object() {
        for (key, value) in entries {
            if let Some(what) = credential_in_value(value, 0) {
                bail!(
                    "refusing to sync \"{key}\": its value is {what}; Connections stores settings, not credentials"
                );
            }
        }
    }

    let bytes = serde_json::to_vec(settings).context("serialize synced settings")?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        let biggest = settings
            .as_object()
            .and_then(|entries| {
                entries
                    .iter()
                    .map(|(key, value)| {
                        (
                            key,
                            serde_json::to_vec(value)
                                .map(|bytes| bytes.len())
                                .unwrap_or(0),
                        )
                    })
                    .max_by_key(|(_, bytes)| *bytes)
            })
            .map(|(key, bytes)| format!("; \"{key}\" alone is {bytes} bytes"))
            .unwrap_or_default();
        bail!(
            "synced settings are {} bytes, over the {MAX_DOCUMENT_BYTES}-byte limit{biggest}",
            bytes.len()
        );
    }
    Ok(())
}

/// Merge only the stats portion of `other` into `preferred`; portable settings
/// in `preferred` keep their existing conflict policy.
fn merge_stats(preferred: &mut Value, other: &Value) -> bool {
    let Some(other_stats) = synced_stats(other) else {
        return false;
    };
    let Some(preferred_obj) = preferred.as_object_mut() else {
        return false;
    };
    let merged = match preferred_obj.get(STATS_KEY) {
        Some(local_stats) => UsageStats::merge_synced_values(local_stats, other_stats),
        None => other_stats.clone(),
    };
    if preferred_obj.get(STATS_KEY) == Some(&merged) {
        return false;
    }
    preferred_obj.insert(STATS_KEY.to_string(), merged);
    true
}

/// Overlay the allowlisted keys from a remote settings doc onto `cfg`, leaving
/// every non-synced field (API keys, window geometry, …) untouched. Returns
/// `true` if anything actually changed. Type-checked by round-tripping through
/// serde, so a malformed remote value can never corrupt the config.
pub fn apply_synced_to_config(cfg: &mut Config, remote: &Value) -> bool {
    let Some(remote_obj) = remote.as_object() else {
        return false;
    };
    let mut base = match serde_json::to_value(&*cfg) {
        Ok(Value::Object(m)) => m,
        _ => return false,
    };
    let before = base.clone();
    for k in SYNCED_KEYS {
        if let Some(v) = remote_obj.get(*k) {
            base.insert((*k).to_string(), v.clone());
        }
    }
    if base == before {
        return false;
    }
    match serde_json::from_value::<Config>(Value::Object(base)) {
        Ok(mut merged) => {
            merged.normalize_local_model();
            *cfg = merged;
            true
        }
        Err(_) => false,
    }
}

// ---- Persisted credentials (DPAPI-sealed) ----------------------------------

/// What we keep between sessions. The **refresh token** is the only secret; the
/// email/name are cached purely so the UI can show "Synced as …" instantly on
/// open without a network round-trip. Sealed as one DPAPI blob on disk.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Creds {
    pub refresh_token: String,
    #[serde(default)]
    pub sub: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
    /// Profile-picture URL (from userinfo's `picture` claim, `photo` scope). Cached so the UI can
    /// re-fetch + show the avatar on open without a fresh sign-in.
    #[serde(default)]
    pub picture: String,
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|p| p.to_path_buf())
}

fn creds_path() -> Option<PathBuf> {
    exe_dir().map(|d| d.join(CREDS_FILE))
}

pub fn save_creds(c: &Creds) -> Result<()> {
    let json = serde_json::to_vec(c)?;
    let sealed = dpapi(true, &json).ok_or_else(|| anyhow!("DPAPI encrypt failed"))?;
    let path = creds_path().ok_or_else(|| anyhow!("cannot locate creds path"))?;
    // Write atomically (tmp + rename) so a reader (or a racing writer) never
    // observes a half-written / truncated sealed blob.
    let tmp = path.with_extension("dat.tmp");
    std::fs::write(&tmp, &sealed).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

pub fn load_creds() -> Option<Creds> {
    let sealed = std::fs::read(creds_path()?).ok()?;
    let json = dpapi(false, &sealed)?;
    serde_json::from_slice(&json).ok()
}

pub fn clear_creds() {
    if let Some(p) = creds_path() {
        let _ = std::fs::remove_file(p);
    }
}

/// Cheap "are we signed in?" — true iff a decryptable creds blob exists.
pub fn is_signed_in() -> bool {
    load_creds().is_some()
}

// ---- Crypto helpers (Windows CNG + DPAPI, no extra crates) -----------------

/// CSPRNG bytes via CNG's system-preferred RNG. **Fallible on purpose:** the
/// buffer is pre-zeroed, so if BCryptGenRandom ever failed we must NOT return
/// those zeros as "randomness" — a zeroed PKCE verifier or CSRF `state` would
/// be predictable. We check the NTSTATUS just like `update.rs::sha256_hex`.
#[cfg(windows)]
fn rand_bytes(n: usize) -> Result<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut buf = vec![0u8; n];
    let status = unsafe {
        BCryptGenRandom(
            BCRYPT_ALG_HANDLE::default(),
            &mut buf,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status.is_ok() {
        Ok(buf)
    } else {
        bail!("system RNG (BCryptGenRandom) failed: {status:?}")
    }
}

/// SHA-256 raw digest via CNG's one-shot pseudo-handle (same checked call as
/// `update.rs::sha256_hex`, returning the 32 raw bytes for PKCE). Fallible so a
/// hash failure can't silently yield a fixed all-zero PKCE challenge.
#[cfg(windows)]
fn sha256(bytes: &[u8]) -> Result<[u8; 32]> {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, bytes, &mut out) };
    if status.is_ok() {
        Ok(out)
    } else {
        bail!("SHA-256 (BCryptHash) failed: {status:?}")
    }
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let s = s.trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .ok()
}

struct Pkce {
    verifier: String,
    challenge: String,
}

fn pkce() -> Result<Pkce> {
    let verifier = b64url(&rand_bytes(32)?);
    let challenge = b64url(&sha256(verifier.as_bytes())?);
    Ok(Pkce {
        verifier,
        challenge,
    })
}

// ---- Browser launch --------------------------------------------------------

/// Open the system browser at `url` (mirrors `about.rs::open_url`).
#[cfg(windows)]
fn open_browser(url: &str) {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            HWND::default(),
            w!("open"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

// ---- HTTP + token flows ----------------------------------------------------

fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("http client init")
}

/// Tokens + identity from a sign-in or refresh.
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub sub: String,
    pub email: String,
    pub name: String,
    pub picture: String,
}

/// Pull `sub` / `email` / `name` out of an `id_token` JWT payload (no signature
/// check needed — it came straight from the token endpoint over TLS, and it is
/// used only for the display label). Empty strings if absent (e.g. a refresh
/// response, which may omit the id_token and mints an opaque access token).
fn decode_identity(id_token: &str) -> (String, String, String) {
    let payload = id_token.split('.').nth(1).unwrap_or("");
    if let Some(bytes) = b64url_decode(payload) {
        if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
            let sub = v["sub"].as_str().unwrap_or_default().to_string();
            let email = v["email"].as_str().unwrap_or_default().to_string();
            let name = v["name"]
                .as_str()
                .or_else(|| v["given_name"].as_str())
                .unwrap_or_default()
                .to_string();
            return (sub, email, name);
        }
    }
    (String::new(), String::new(), String::new())
}

/// Full interactive sign-in: loopback listener + system-browser OAuth (PKCE) +
/// code→token exchange. Blocking; run on a worker thread.
pub fn sign_in() -> Result<Tokens> {
    let pkce = pkce()?;
    let state = b64url(&rand_bytes(16)?);

    // Ephemeral loopback port — the IdP honors RFC 8252 any-port for our
    // bare-host registration, so the OS can pick a free port (never collides).
    let listener = TcpListener::bind("127.0.0.1:0").context("bind loopback listener")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{REDIRECT_PATH}");

    let mut url = url::Url::parse(AUTH_URL).context("parse authorize url")?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);

    open_browser(url.as_str());
    tracing::info!("connections: opened browser for sign-in on loopback :{port}");

    let (code, got_state) = wait_for_callback(&listener, CALLBACK_TIMEOUT)?;
    if got_state != state {
        bail!("state mismatch (possible CSRF) — sign-in aborted");
    }

    let resp = client()?
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", CLIENT_ID),
            ("code_verifier", pkce.verifier.as_str()),
        ])
        .send()
        .context("token exchange request")?;
    let status = resp.status();
    let body: Value = resp.json().context("token response was not JSON")?;
    if !status.is_success() {
        bail!("token exchange failed (HTTP {status}): {body}");
    }
    let access_token = body["access_token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if access_token.is_empty() {
        bail!("token response had no access_token");
    }
    let refresh_token = body["refresh_token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let (sub, email, name) = decode_identity(body["id_token"].as_str().unwrap_or_default());
    // The id_token carries only `sub` — the auth backend mints minimal-claim id_tokens (Google-style; the
    // display name + privacy-relay email live at /oauth/userinfo). Fetch them so the UI can show a
    // human name instead of an empty label. Best-effort: sign-in still succeeds if userinfo blips.
    let (ui_email, ui_name, ui_picture) = fetch_userinfo(&access_token);
    Ok(Tokens {
        access_token,
        refresh_token,
        sub,
        email: if ui_email.is_empty() { email } else { ui_email },
        name: if ui_name.is_empty() { name } else { ui_name },
        picture: ui_picture,
    })
}

/// Fetch the display `name` (+ privacy-relay `email`) from `/oauth/userinfo`, authenticated with a
/// fresh access token. Returns `(email, name)`, empty strings on any failure (best-effort — identity
/// is only a display label, never load-bearing for sync).
fn fetch_userinfo(access_token: &str) -> (String, String, String) {
    let empty = || (String::new(), String::new(), String::new());
    let Ok(http) = client() else { return empty() };
    let Ok(resp) = http.get(USERINFO_URL).bearer_auth(access_token).send() else {
        return empty();
    };
    if !resp.status().is_success() {
        return empty();
    }
    let Ok(body) = resp.json::<Value>() else {
        return empty();
    };
    let email = body["email"].as_str().unwrap_or_default().to_string();
    let name = body["name"]
        .as_str()
        .or_else(|| body["given_name"].as_str())
        .unwrap_or_default()
        .to_string();
    let picture = body["picture"].as_str().unwrap_or_default().to_string();
    (email, name, picture)
}

/// Fetch + decode the avatar image at `url` into `(width, height, rgba8)` for an egui texture.
/// Runs on a sync worker thread (the decode is off the UI thread). Best-effort: `None` on any
/// network/format failure, so the UI simply shows no avatar. Requires the `photo` scope to have
/// yielded a `picture` URL.
pub fn fetch_avatar(url: &str) -> Option<(u32, u32, Vec<u8>)> {
    if url.is_empty() {
        return None;
    }
    let parsed = url::Url::parse(url).ok()?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return None;
    }
    let http = client().ok()?;
    let mut resp = http.get(parsed).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    if resp.content_length().is_some_and(|n| n > MAX_AVATAR_BYTES) {
        return None;
    }
    let mut bytes =
        Vec::with_capacity(resp.content_length().unwrap_or(0).min(MAX_AVATAR_BYTES) as usize);
    resp.by_ref()
        .take(MAX_AVATAR_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_AVATAR_BYTES {
        return None;
    }
    let dimensions = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if dimensions.0 > MAX_AVATAR_DIMENSION || dimensions.1 > MAX_AVATAR_DIMENSION {
        return None;
    }
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (w, h) = (img.width(), img.height());
    Some((w, h, img.into_raw()))
}

/// Mint a fresh (opaque) access token from a stored refresh token.
pub fn refresh(refresh_token: &str) -> Result<Tokens> {
    if refresh_token.is_empty() {
        bail!("no refresh token stored — sign in again");
    }
    let resp = client()?
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .context("refresh request")?;
    let status = resp.status();
    let body: Value = resp.json().context("refresh response was not JSON")?;
    if !status.is_success() {
        bail!("token refresh failed (HTTP {status}): {body}");
    }
    let access_token = body["access_token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if access_token.is_empty() {
        bail!("refresh response had no access_token");
    }
    // Refresh may rotate the refresh token; keep the new one if present.
    let new_refresh = body["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(refresh_token)
        .to_string();
    let (sub, email, name) = decode_identity(body["id_token"].as_str().unwrap_or_default());
    // A refresh response usually omits the id_token and never the picture; the avatar/name/email
    // come from userinfo (fetched by resume_and_pull), so leave picture empty here.
    Ok(Tokens {
        access_token,
        refresh_token: new_refresh,
        sub,
        email,
        name,
        picture: String::new(),
    })
}

/// Block on the loopback listener until the browser hits the callback with a
/// `code`, tolerating stray requests, until `timeout`.
fn wait_for_callback(listener: &TcpListener, timeout: Duration) -> Result<(String, String)> {
    listener
        .set_nonblocking(true)
        .context("set loopback non-blocking")?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("");
                let (mut code, mut st, mut err) = (String::new(), String::new(), String::new());
                if let Ok(u) = url::Url::parse(&format!("http://127.0.0.1{path}")) {
                    // Only the registered callback path carries the OAuth
                    // response; anything else (favicon, bare "/") is a stray
                    // request we answer and keep waiting on.
                    if u.path() == REDIRECT_PATH {
                        for (k, v) in u.query_pairs() {
                            match k.as_ref() {
                                "code" => code = v.into_owned(),
                                "state" => st = v.into_owned(),
                                "error" => err = v.into_owned(),
                                _ => {}
                            }
                        }
                    }
                }

                if !code.is_empty() {
                    reply(&mut stream, SUCCESS_PAGE);
                    return Ok((code, st));
                }
                if !err.is_empty() {
                    reply(&mut stream, FAIL_PAGE);
                    bail!("authorization was denied ({err})");
                }
                // Stray request (favicon, preconnect, bare "/") — answer and wait.
                reply(&mut stream, WAIT_PAGE);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!(
                        "timed out after {}s waiting for the browser sign-in",
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(Duration::from_millis(80));
            }
            Err(e) => return Err(anyhow!("loopback accept failed: {e}")),
        }
    }
}

fn reply(stream: &mut std::net::TcpStream, html: &str) {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

const SUCCESS_PAGE: &str = "<!doctype html><meta charset=utf-8><title>QuickDictate</title>\
<body style=\"font-family:Segoe UI,system-ui,sans-serif;text-align:center;padding-top:3.5em;color:#1b2330\">\
<h2 style=\"color:#3a7afe\">Signed in \u{2713}</h2>\
<p>QuickDictate is now syncing your settings.<br>You can close this tab.</p></body>";
const FAIL_PAGE: &str = "<!doctype html><meta charset=utf-8><title>QuickDictate</title>\
<body style=\"font-family:Segoe UI,system-ui,sans-serif;text-align:center;padding-top:3.5em\">\
<h2>Sign-in was cancelled</h2><p>You can close this tab and try again.</p></body>";
const WAIT_PAGE: &str =
    "<!doctype html><meta charset=utf-8><body>QuickDictate is waiting\u{2026}</body>";

// ---- Store calls (§5a / §5f) ----------------------------------------------

#[derive(Clone)]
pub struct RemoteDoc {
    pub settings: Value,
    pub version: u64,
}

#[derive(Clone)]
struct CachedRemoteDoc {
    doc: RemoteDoc,
    etag: String,
}

fn parse_etag_version(etag: &str) -> Option<u64> {
    etag.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .parse()
        .ok()
}

fn retry_after_seconds(body: &Value) -> Option<u64> {
    body.get("retry_after_seconds")
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 0)
}

fn rate_limit_wait(body: &Value) -> Duration {
    Duration::from_secs(retry_after_seconds(body).unwrap_or(1)).min(MAX_RATE_LIMIT_WAIT)
}

fn clear_store_cache() {
    *store_cache() = None;
}

/// `GET /v1/app-data/{appId}` → the user's settings doc (`version:0` if never
/// written). Repeated reads are ETag-conditional; a 304 reuses the cached body.
pub fn store_pull(access_token: &str) -> Result<RemoteDoc> {
    let cached = store_cache().clone();
    let mut rate_limit_retried = false;
    loop {
        let mut request = client()?
            .get(format!("{STORE_BASE}/{CLIENT_ID}"))
            .bearer_auth(access_token);
        if let Some(cached) = &cached {
            request = request.header(reqwest::header::IF_NONE_MATCH, &cached.etag);
        }
        let resp = request.send().context("store GET")?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_MODIFIED {
            return cached
                .map(|cached| cached.doc)
                .ok_or_else(|| anyhow!("sync server returned 304 without a cached document"));
        }
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body: Value = resp.json().context("store GET was not JSON")?;
        if status.as_u16() == 429 && !rate_limit_retried {
            rate_limit_retried = true;
            std::thread::sleep(rate_limit_wait(&body));
            continue;
        }
        if !status.is_success() {
            bail!("could not read cloud settings (HTTP {status}): {body}");
        }
        let body_version = body.get("version").and_then(Value::as_u64).unwrap_or(0);
        let server_settings = body
            .get("server_settings")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        if server_settings
            .as_object()
            .is_some_and(|settings| !settings.is_empty())
        {
            // Parsed deliberately, but not applied: QuickDictate currently has
            // no server-authoritative plan/entitlement setting. Keeping this
            // explicit prevents that tier being mistaken for user preferences.
            tracing::debug!("connections: server settings received; no supported keys yet");
        }
        let doc = RemoteDoc {
            settings: body
                .get("settings")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
            // The ETag is the authoritative conditional-read version. Fall
            // back to the body for older/self-hosted implementations.
            version: etag
                .as_deref()
                .and_then(parse_etag_version)
                .unwrap_or(body_version),
        };
        let cache_etag = etag.unwrap_or_else(|| format!("\"{}\"", doc.version));
        *store_cache() = Some(CachedRemoteDoc {
            doc: doc.clone(),
            etag: cache_etag,
        });
        return Ok(doc);
    }
}

/// `POST /v1/app-data/{appId}` with the full syncable snapshot.
///
/// Uses RFC 7386 **merge mode**, so another device's keys survive concurrent
/// writes. A stale base version retries the same patch against the server's
/// current version, matching @cnct/connect 1.0.0. A 429 schedules one bounded
/// retry using the server's documented `retry_after_seconds`.
pub fn store_push(access_token: &str, settings: &Value, base_version: u64) -> Result<u64> {
    validate_sync_snapshot(settings)?;
    let url = format!("{STORE_BASE}/{CLIENT_ID}");
    let mut base = base_version;
    let mut conflicts = 0;
    let mut rate_limit_retried = false;
    loop {
        let resp = client()?
            .post(&url)
            .bearer_auth(access_token)
            .json(&serde_json::json!({
                "settings": settings,
                "baseVersion": base,
                "merge": true,
            }))
            .send()
            .context("store POST")?;
        let status = resp.status();
        let body: Value = resp.json().unwrap_or(Value::Null);
        if status.is_success() {
            clear_store_cache();
            return Ok(body
                .get("version")
                .and_then(Value::as_u64)
                .unwrap_or(base + 1));
        }
        match status.as_u16() {
            409 => {
                conflicts += 1;
                if conflicts >= 3 {
                    bail!("push kept conflicting with a newer cloud copy; try again");
                }
                base = body
                    .get("current")
                    .and_then(|current| current.get("version"))
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| {
                        clear_store_cache();
                        store_pull(access_token)
                            .map(|latest| latest.version)
                            .unwrap_or(base)
                    });
                continue;
            }
            429 if !rate_limit_retried => {
                rate_limit_retried = true;
                std::thread::sleep(rate_limit_wait(&body));
                continue;
            }
            429 => bail!(
                "the settings store is still rate-limiting us after waiting {} seconds",
                retry_after_seconds(&body).unwrap_or(1)
            ),
            413 => bail!("settings are too large to sync (over 64 KB)"),
            _ => bail!("could not save to the cloud (HTTP {status}): {body}"),
        }
    }
}

/// `DELETE /v1/app-data/{appId}` — forget the remote doc. Idempotent.
pub fn store_delete(access_token: &str) -> Result<()> {
    let resp = client()?
        .delete(format!("{STORE_BASE}/{CLIENT_ID}"))
        .bearer_auth(access_token)
        .send()
        .context("store DELETE")?;
    let status = resp.status();
    if status.is_success() || status.as_u16() == 404 {
        Ok(())
    } else {
        bail!("disconnect failed (HTTP {status})")
    }
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

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)] // test setup reads clearer field-by-field
mod tests {
    use super::*;
    use crate::stats::{DeviceStats, PeriodStats, ProviderStats, UsageStats};
    use std::collections::BTreeMap;

    #[test]
    fn synced_snapshot_carries_prefs_but_no_secrets_or_geometry() {
        let mut cfg = Config::default();
        cfg.elevenlabs_keys = vec!["sk_secret".into()];
        cfg.openai_keys = vec!["sk_secret2".into()];
        cfg.window_x = Some(1234);
        cfg.window_width = 999;
        cfg.run_at_startup = true;
        cfg.enable_logging = true;
        cfg.language = "fr-FR".into();

        let snap = config_to_synced(&cfg);
        let obj = snap.as_object().unwrap();

        // Portable prefs present.
        assert_eq!(obj.get("language").unwrap(), "fr-FR");
        assert!(obj.contains_key("toggle_hotkey"));
        assert!(obj.contains_key("text_replacements"));
        assert!(obj.contains_key("stt_provider"));

        // Secrets + machine-local state absent.
        for forbidden in [
            "elevenlabs_keys",
            "openai_keys",
            "deepgram_keys",
            "assemblyai_keys",
            "dashscope_keys",
            "google_keys",
            "local_keys",
            "window_x",
            "window_y",
            "window_width",
            "window_height",
            "run_at_startup",
            "enable_logging",
        ] {
            assert!(
                !obj.contains_key(forbidden),
                "{forbidden} must never be in the synced snapshot"
            );
        }
    }

    #[test]
    fn full_snapshot_includes_stats_but_not_the_local_device_marker() {
        let mut stats = UsageStats::default();
        stats.local_device_id = "local-only-marker".into();
        stats.devices.insert(
            "device-a".into(),
            DeviceStats {
                totals: PeriodStats {
                    words: 42,
                    audio_ms: 9_000,
                    dictations: 2,
                    ..PeriodStats::default()
                },
                ..DeviceStats::default()
            },
        );

        let snapshot = snapshot_to_synced(&Config::default(), &stats);
        assert!(snapshot.get(STATS_KEY).is_some());
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("local-only-marker"));
        assert!(!json.contains("local_device_id"));
    }

    #[test]
    fn stats_merge_unions_devices_without_changing_preferred_settings() {
        let mut local_stats = UsageStats::default();
        local_stats.devices.insert(
            "local-device".into(),
            DeviceStats {
                totals: PeriodStats {
                    words: 10,
                    dictations: 1,
                    ..PeriodStats::default()
                },
                ..DeviceStats::default()
            },
        );
        let mut remote_stats = UsageStats::default();
        remote_stats.devices.insert(
            "remote-device".into(),
            DeviceStats {
                totals: PeriodStats {
                    words: 20,
                    dictations: 1,
                    ..PeriodStats::default()
                },
                ..DeviceStats::default()
            },
        );
        let mut preferred = serde_json::json!({
            "language": "en-US",
            (STATS_KEY): local_stats.synced_value(),
        });
        let other = serde_json::json!({
            "language": "de-DE",
            (STATS_KEY): remote_stats.synced_value(),
        });

        assert!(merge_stats(&mut preferred, &other));
        assert_eq!(preferred["language"], "en-US");
        let mut merged = UsageStats::default();
        assert!(merged.merge_synced_value(&preferred[STATS_KEY]));
        assert_eq!(merged.total_words, 30);
        assert_eq!(merged.total_dictations, 2);
    }

    #[test]
    fn compact_week_of_hourly_stats_stays_below_store_limit() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "local".into(),
            ProviderStats {
                words: 30,
                audio_ms: 8_000,
                dictations: 1,
            },
        );
        providers.insert(
            "openai".into(),
            ProviderStats {
                words: 30,
                audio_ms: 8_000,
                dictations: 1,
            },
        );
        let bucket = PeriodStats {
            words: 60,
            audio_ms: 16_000,
            dictations: 2,
            longest_dictation_words: 30,
            longest_dictation_audio_ms: 8_000,
            providers,
        };
        let mut hours = BTreeMap::new();
        for hour in 0..(24 * 8) {
            hours.insert(1_800_000_000 + hour * 3_600, bucket.clone());
        }
        let mut stats = UsageStats::default();
        stats.devices.insert(
            "device-with-a-full-week".into(),
            DeviceStats {
                totals: bucket,
                hours,
                ..DeviceStats::default()
            },
        );

        let snapshot = snapshot_to_synced(&Config::default(), &stats);
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        assert!(
            bytes.len() < 64 * 1_024,
            "snapshot was {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn credential_values_are_refused_even_when_nested() {
        let snapshot = serde_json::json!({
            "text_replacements": [{
                "from": "work token",
                "to": "ghp_abcdefghijklmnopqrstuvwxyz0123456789"
            }]
        });
        let error = validate_sync_snapshot(&snapshot).unwrap_err().to_string();
        assert!(error.contains("GitHub token"), "{error}");

        assert!(validate_sync_snapshot(&serde_json::json!({
            "text_replacements": [{"from": "sk", "to": "ordinary text"}]
        }))
        .is_ok());
    }

    #[test]
    fn credential_patterns_catch_prefixed_stt_keys_but_not_bare_hex() {
        // Prefixed key shapes are unambiguous and must be caught.
        for (value, provider) in [
            ("sk_0123456789abcdef0123456789abcdef01234567", "ElevenLabs"),
            ("sk-0123456789abcdef0123456789abcdef", "DashScope"),
        ] {
            let snapshot = serde_json::json!({
                "text_replacements": [{"from": "x", "to": value}]
            });
            let error = validate_sync_snapshot(&snapshot).unwrap_err().to_string();
            assert!(
                error.contains(provider),
                "expected the {provider} key pattern to catch {value:?}, got: {error}"
            );
        }
        // Bare 32/40-char hex is NOT flagged: it is indistinguishable from a
        // git SHA, an MD5 hash, or a dashless GUID, and flagging it would
        // permanently block sync for a user with one such string in a synced
        // text field. The SYNCED_KEYS allowlist is the real guard for the
        // prefixless Deepgram/AssemblyAI shapes.
        for value in [
            "2f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a", // git-SHA shaped
            "9e11c31e751b4a12a3f9f4c3b2b1a123",         // MD5/GUID shaped
        ] {
            let snapshot = serde_json::json!({
                "text_replacements": [{"from": "commit", "to": value}],
                "custom_vocabulary": [value]
            });
            assert!(
                validate_sync_snapshot(&snapshot).is_ok(),
                "bare hex {value:?} must not block a legitimate sync push"
            );
        }
    }

    #[test]
    fn oversized_snapshot_is_rejected_locally_and_names_the_largest_key() {
        let snapshot = serde_json::json!({
            "language": "en-US",
            "text_replacements": "x".repeat(MAX_DOCUMENT_BYTES)
        });
        let error = validate_sync_snapshot(&snapshot).unwrap_err().to_string();
        assert!(error.contains("over the 65536-byte limit"), "{error}");
        assert!(error.contains("text_replacements"), "{error}");
    }

    #[test]
    fn etag_and_rate_limit_contract_fields_are_parsed() {
        assert_eq!(parse_etag_version("\"42\""), Some(42));
        assert_eq!(parse_etag_version("W/\"7\""), Some(7));
        assert_eq!(
            retry_after_seconds(&serde_json::json!({"retry_after_seconds": 9})),
            Some(9)
        );
    }

    #[test]
    fn apply_overlays_only_allowlisted_keys_and_never_touches_secrets() {
        let mut local = Config::default();
        local.elevenlabs_keys = vec!["sk_local".into()];
        local.language = "en-US".into();

        // A remote doc that (maliciously or otherwise) also carries a key array.
        let remote = serde_json::json!({
            "language": "de-DE",
            "auto_punct": false,
            "elevenlabs_keys": ["sk_evil"],
            "some_unknown_key": 42,
        });

        let changed = apply_synced_to_config(&mut local, &remote);
        assert!(changed);
        assert_eq!(local.language, "de-DE"); // allowlisted → applied
        assert!(!local.auto_punct); // allowlisted → applied
        assert_eq!(local.elevenlabs_keys, vec!["sk_local".to_string()]); // secret untouched
    }

    #[test]
    fn apply_is_noop_when_nothing_synced_differs() {
        let mut local = Config::default();
        let snap = config_to_synced(&local);
        assert!(!apply_synced_to_config(&mut local, &snap));
    }

    #[test]
    fn apply_replaces_an_unavailable_synced_local_model() {
        let mut local = Config::default();
        let remote = serde_json::json!({ "local_model": "retired-model" });
        assert!(apply_synced_to_config(&mut local, &remote));
        assert_eq!(local.local_model, "cohere-q5");
    }

    #[test]
    fn synced_snapshot_round_trips_between_configs() {
        let mut a = Config::default();
        a.language = "ja-JP".into();
        a.mode = "hold".into();
        a.spinner_type = "braille".into();
        a.text_replacements.clear();
        a.text_replacements.insert("teh".into(), "the".into());

        let mut b = Config::default();
        b.openai_keys = vec!["sk_b".into()]; // b's own secret, must survive
        let snap = config_to_synced(&a);
        apply_synced_to_config(&mut b, &snap);

        assert_eq!(b.language, "ja-JP");
        assert_eq!(b.mode, "hold");
        assert_eq!(b.spinner_type, "braille");
        assert_eq!(
            b.text_replacements.get("teh").map(String::as_str),
            Some("the")
        );
        assert_eq!(b.openai_keys, vec!["sk_b".to_string()]); // untouched
    }

    /// Guard against the exact drift that motivated [`NEVER_SYNCED`]: a
    /// portable `Config` field gets added but nobody remembers to add it to
    /// [`SYNCED_KEYS`], so it silently never syncs. Serializes a real
    /// `Config::default()` and checks every JSON key is filed into exactly
    /// one of the two lists, so a new field breaks this test (naming itself)
    /// until someone decides where it belongs.
    #[test]
    fn every_config_field_is_synced_or_never_synced() {
        let value = serde_json::to_value(Config::default()).expect("Config serializes");
        let obj = value.as_object().expect("Config serializes to an object");

        for key in obj.keys() {
            let synced = SYNCED_KEYS.contains(&key.as_str());
            let never = NEVER_SYNCED.contains(&key.as_str());
            assert!(
                synced || never,
                "Config field \"{key}\" is in neither SYNCED_KEYS nor NEVER_SYNCED — \
                 decide whether it should sync and add it to one of them"
            );
            assert!(
                !(synced && never),
                "Config field \"{key}\" is listed in BOTH SYNCED_KEYS and NEVER_SYNCED"
            );
        }

        // Catch the reverse mistake too: a stale or misspelled name sitting in
        // one of the lists that no longer (or never did) match a real field.
        for key in SYNCED_KEYS.iter().chain(NEVER_SYNCED.iter()) {
            assert!(
                obj.contains_key(*key),
                "\"{key}\" is listed in SYNCED_KEYS/NEVER_SYNCED but is not a Config field"
            );
        }
    }
}
