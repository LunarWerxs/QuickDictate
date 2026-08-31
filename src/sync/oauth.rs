//! OAuth against accounts.connections.icu.
//!
//! Authorization Code + PKCE as a public client: CNG for the verifier and
//! state, the system browser for consent, a one-shot loopback listener for the
//! redirect, and the token/userinfo/avatar calls that follow.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use super::{
    AUTH_URL, CALLBACK_TIMEOUT, CLIENT_ID, MAX_AVATAR_BYTES, MAX_AVATAR_DIMENSION, REDIRECT_PATH,
    SCOPES, TOKEN_URL, USERINFO_URL, USER_AGENT,
};

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

pub(super) fn client() -> Result<reqwest::blocking::Client> {
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
pub(super) fn fetch_userinfo(access_token: &str) -> (String, String, String) {
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
            Ok((stream, _)) => {
                if let Some(code_and_state) = handle_loopback_request(stream)? {
                    return Ok(code_and_state);
                }
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

/// Handle one accepted loopback connection: answer it, and return the OAuth
/// `(code, state)` once the registered callback path delivers one. `Ok(None)`
/// means a stray request (favicon, preconnect, bare "/") was answered and the
/// caller should keep waiting. Split out of `wait_for_callback` so the
/// accept-loop's own timeout/retry handling doesn't nest inside this
/// request's parsing too.
fn handle_loopback_request(mut stream: TcpStream) -> Result<Option<(String, String)>> {
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
        // Only the registered callback path carries the OAuth response;
        // anything else (favicon, bare "/") is a stray request we answer and
        // keep waiting on.
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
        return Ok(Some((code, st)));
    }
    if !err.is_empty() {
        reply(&mut stream, FAIL_PAGE);
        bail!("authorization was denied ({err})");
    }
    // Stray request (favicon, preconnect, bare "/") — answer and wait.
    reply(&mut stream, WAIT_PAGE);
    Ok(None)
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
