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
