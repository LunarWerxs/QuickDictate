//! Raw HTTP against the per-user app-data store.
//!
//! Conditional GET/PUT around the document's ETag version, the cached copy
//! that makes a pull-then-push one round trip, and the rate-limit backoff.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use super::guard::validate_sync_snapshot;
use super::oauth::client;
use super::{store_cache, CLIENT_ID, MAX_RATE_LIMIT_WAIT, STORE_BASE};

// ---- Store calls (§5a / §5f) ----------------------------------------------

#[derive(Clone)]
pub struct RemoteDoc {
    pub settings: Value,
    pub version: u64,
}

#[derive(Clone)]
pub(super) struct CachedRemoteDoc {
    doc: RemoteDoc,
    etag: String,
}

pub(super) fn parse_etag_version(etag: &str) -> Option<u64> {
    etag.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .parse()
        .ok()
}

pub(super) fn retry_after_seconds(body: &Value) -> Option<u64> {
    body.get("retry_after_seconds")
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 0)
}

fn rate_limit_wait(body: &Value) -> Duration {
    Duration::from_secs(retry_after_seconds(body).unwrap_or(1)).min(MAX_RATE_LIMIT_WAIT)
}

pub(super) fn clear_store_cache() {
    *store_cache() = None;
}

/// `GET /v1/app-data/{appId}` → the user's settings doc (`version:0` if never
/// written). Repeated reads are ETag-conditional; a 304 reuses the cached body.
pub fn store_pull(access_token: &str) -> Result<RemoteDoc> {
    let cached = store_cache().clone();
    let mut rate_limit_retried = false;
    loop {
        let resp = send_pull(access_token, cached.as_ref())?;
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
        let doc = remote_doc_from_body(&body, etag.as_deref());
        let cache_etag = etag.unwrap_or_else(|| format!("\"{}\"", doc.version));
        *store_cache() = Some(CachedRemoteDoc {
            doc: doc.clone(),
            etag: cache_etag,
        });
        return Ok(doc);
    }
}

/// One `GET` of the store document, conditional on the cached ETag if any.
fn send_pull(
    access_token: &str,
    cached: Option<&CachedRemoteDoc>,
) -> Result<reqwest::blocking::Response> {
    let mut request = client()?
        .get(format!("{STORE_BASE}/{CLIENT_ID}"))
        .bearer_auth(access_token);
    if let Some(cached) = cached {
        request = request.header(reqwest::header::IF_NONE_MATCH, &cached.etag);
    }
    request.send().context("store GET")
}

/// The document a successful `GET` returned. Pure, so the version and
/// missing-field fallbacks are testable without a server.
pub(super) fn remote_doc_from_body(body: &Value, etag: Option<&str>) -> RemoteDoc {
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
    RemoteDoc {
        settings: body
            .get("settings")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default())),
        // The ETag is the authoritative conditional-read version. Fall
        // back to the body for older/self-hosted implementations.
        version: etag.and_then(parse_etag_version).unwrap_or(body_version),
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
        let (status, body) = post_patch(access_token, &url, settings, base)?;
        match push_step(status.as_u16(), &body, base) {
            PushStep::Saved(version) => {
                clear_store_cache();
                return Ok(version);
            }
            PushStep::Conflict => {
                conflicts += 1;
                if conflicts >= 3 {
                    bail!("push kept conflicting with a newer cloud copy; try again");
                }
                base = conflict_current_version(&body)
                    .unwrap_or_else(|| refetch_version(access_token, base));
            }
            PushStep::RateLimited if !rate_limit_retried => {
                rate_limit_retried = true;
                std::thread::sleep(rate_limit_wait(&body));
            }
            PushStep::RateLimited => bail!(
                "the settings store is still rate-limiting us after waiting {} seconds",
                retry_after_seconds(&body).unwrap_or(1)
            ),
            PushStep::TooLarge => bail!("settings are too large to sync (over 64 KB)"),
            PushStep::Failed => bail!("could not save to the cloud (HTTP {status}): {body}"),
        }
    }
}

/// One merge-mode POST of `settings` against `base`: the status and the body.
fn post_patch(
    access_token: &str,
    url: &str,
    settings: &Value,
    base: u64,
) -> Result<(reqwest::StatusCode, Value)> {
    let resp = client()?
        .post(url)
        .bearer_auth(access_token)
        .json(&serde_json::json!({
            "settings": settings,
            "baseVersion": base,
            "merge": true,
        }))
        .send()
        .context("store POST")?;
    let status = resp.status();
    Ok((status, resp.json().unwrap_or(Value::Null)))
}

/// What one push attempt's answer means for the loop in [`store_push`].
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PushStep {
    /// Saved; the new version (the body's, or `base + 1` if it named none).
    Saved(u64),
    /// 409: someone else wrote first; retry against the newer version.
    Conflict,
    /// 429: wait as told, once.
    RateLimited,
    /// 413: over the store's size cap.
    TooLarge,
    Failed,
}

pub(super) fn push_step(status: u16, body: &Value, base: u64) -> PushStep {
    match status {
        200..=299 => PushStep::Saved(
            body.get("version")
                .and_then(Value::as_u64)
                .unwrap_or(base + 1),
        ),
        409 => PushStep::Conflict,
        429 => PushStep::RateLimited,
        413 => PushStep::TooLarge,
        _ => PushStep::Failed,
    }
}

/// The server's current version from a 409 body (`current.version`), which
/// is the base the retried push must name.
pub(super) fn conflict_current_version(body: &Value) -> Option<u64> {
    body.get("current")
        .and_then(|current| current.get("version"))
        .and_then(Value::as_u64)
}

/// The latest version by a fresh (uncached) pull, for a 409 body that did not
/// carry one; `fallback` if even that fails.
fn refetch_version(access_token: &str, fallback: u64) -> u64 {
    clear_store_cache();
    store_pull(access_token)
        .map(|latest| latest.version)
        .unwrap_or(fallback)
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
