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
//!   * **Only portable preferences sync** ([`SYNCED_KEYS`]). API keys, window
//!     geometry, `run_at_startup`, and logging flags never leave the machine.
//!
//! The access token lives only in a worker's stack for the duration of one call;
//! it is never persisted. Only the refresh token (+ a display email/name) is
//! stored, DPAPI-sealed, next to the exe as `quickdictate-connections.dat`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;

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

/// How long we wait for the user to complete sign-in in their browser.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_AVATAR_BYTES: u64 = 5 * 1024 * 1024;
const MAX_AVATAR_DIMENSION: u32 = 2048;

/// Serializes the refresh-token-using operations (`resume_and_pull`,
/// `push_now`, `disconnect`). Without it, a Settings-open resume racing a
/// Save & Restart push could fire two concurrent `refresh` exchanges and
/// interleave writes to the sealed creds file. Held only for the duration of
/// one operation; sign-in (`connect_and_reconcile`) is signed-out-only and
/// mutually exclusive with these by app state, so it stays lock-free.
static SYNC_LOCK: Mutex<()> = Mutex::new(());

/// Acquire [`SYNC_LOCK`], recovering the guard even if a previous holder
/// panicked (the lock guards ordering, not invariant-bearing data).
fn sync_guard() -> std::sync::MutexGuard<'static, ()> {
    SYNC_LOCK.lock().unwrap_or_else(|e| e.into_inner())
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
///   * `enable_logging` — a local diagnostics toggle.
///
/// Only portable preferences travel. Names match `Config`'s serde field names
/// exactly, so the transforms below stay in lock-step with the struct.
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
        Ok(merged) => {
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

/// DPAPI seal (`op=true`) / unseal (`op=false`), CurrentUser scope, no prompt.
#[cfg(windows)]
fn dpapi(encrypt: bool, data: &[u8]) -> Option<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    unsafe {
        let inb = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        let res = if encrypt {
            CryptProtectData(
                &inb,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &inb,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if res.is_err() || out.pbData.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out.pbData as *mut core::ffi::c_void));
        Some(bytes)
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

pub struct RemoteDoc {
    pub settings: Value,
    pub version: u64,
}

/// `GET /v1/app-data/{appId}` → the user's settings doc (`version:0` if never
/// written).
pub fn store_pull(access_token: &str) -> Result<RemoteDoc> {
    let resp = client()?
        .get(format!("{STORE_BASE}/{CLIENT_ID}"))
        .bearer_auth(access_token)
        .send()
        .context("store GET")?;
    let status = resp.status();
    let body: Value = resp.json().context("store GET was not JSON")?;
    if !status.is_success() {
        bail!("could not read cloud settings (HTTP {status}): {body}");
    }
    Ok(RemoteDoc {
        settings: body
            .get("settings")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default())),
        version: body.get("version").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// `POST /v1/app-data/{appId}` with the full syncable snapshot.
///
/// We use **`merge:false` (full replace)** rather than a deep-merge: every push
/// carries the *complete* allowlisted set, so a full replace correctly reflects
/// deletions (e.g. a removed text-replacement) — a deep-merge never would.
/// `baseVersion` gives optimistic concurrency; on `409` we treat a Save as an
/// explicit "make my settings the truth" and re-read the version + overwrite
/// (last-write-wins), bounded to a few tries.
pub fn store_push(access_token: &str, settings: &Value, base_version: u64) -> Result<u64> {
    let url = format!("{STORE_BASE}/{CLIENT_ID}");
    let mut base = base_version;
    for attempt in 0u64..4 {
        let resp = client()?
            .post(&url)
            .bearer_auth(access_token)
            .json(&serde_json::json!({
                "settings": settings,
                "baseVersion": base,
                "merge": false,
            }))
            .send()
            .context("store POST")?;
        let status = resp.status();
        if status.is_success() {
            let body: Value = resp.json().unwrap_or(Value::Null);
            return Ok(body
                .get("version")
                .and_then(Value::as_u64)
                .unwrap_or(base + 1));
        }
        match status.as_u16() {
            409 => {
                let body: Value = resp.json().unwrap_or(Value::Null);
                let current = body
                    .get("current")
                    .and_then(|c| c.get("version"))
                    .and_then(Value::as_u64);
                base = match current {
                    Some(v) => v,
                    None => store_pull(access_token)?.version,
                };
                // Jittered backoff so two devices saving at once don't ping-pong
                // 409s back-to-back (this runs on a worker thread, so a short
                // blocking sleep is fine).
                let jitter = rand_bytes(2)
                    .map(|b| u64::from(u16::from_le_bytes([b[0], b[1]])) % 250)
                    .unwrap_or(125);
                std::thread::sleep(Duration::from_millis((attempt + 1) * 250 + jitter));
                continue;
            }
            429 => bail!("the settings store is rate-limiting us; try again shortly"),
            413 => bail!("settings are too large to sync (over 64 KB)"),
            _ => {
                let body: Value = resp.json().unwrap_or(Value::Null);
                bail!("could not save to the cloud (HTTP {status}): {body}");
            }
        }
    }
    bail!("push kept conflicting with a newer cloud copy; try again")
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
        Ok(Connected {
            name: tokens.name,
            email: tokens.email,
            avatar,
            remote: Some(doc.settings),
            seeded: false,
        })
    }
}

/// Silent resume on Settings-window open when creds already exist: refresh →
/// pull. Returns the remote doc to apply (if any).
pub fn resume_and_pull() -> Result<Connected> {
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
    Ok(Connected {
        name: creds.name,
        email: creds.email,
        avatar,
        remote: (doc.version > 0).then_some(doc.settings),
        seeded: false,
    })
}

/// Push the current local snapshot to the cloud (used on Save). Refresh-aware.
pub fn push_now(local_snapshot: Value) -> Result<u64> {
    let _guard = sync_guard();
    let creds = load_creds().ok_or_else(|| anyhow!("not signed in"))?;
    let tokens = refresh(&creds.refresh_token)?;
    persist_rotated(&creds, &tokens);
    let base = store_pull(&tokens.access_token)?.version;
    store_push(&tokens.access_token, &local_snapshot, base)
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
}

/// Re-seal creds if a refresh rotated the refresh token.
fn persist_rotated(old: &Creds, fresh: &Tokens) {
    if !fresh.refresh_token.is_empty() && fresh.refresh_token != old.refresh_token {
        let _ = save_creds(&Creds {
            refresh_token: fresh.refresh_token.clone(),
            ..old.clone()
        });
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)] // test setup reads clearer field-by-field
mod tests {
    use super::*;

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
}
