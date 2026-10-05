//! Talking to Connections' public redeem door: shaping a key, the request, and
//! reading the reply.
//!
//! One endpoint does both jobs. Redeeming a new key and renewing a certificate
//! are the same call: replaying the key with the same install id answers `200`
//! with `replayed: true` and a fresh certificate. Any bad, expired or revoked
//! key answers `404 {"error":"unknown_or_expired_key"}` (verified live
//! 2026-10-02).
//!
//! FAIL DIRECTION. Only that exact refusal is read as "no". Everything else
//! that is not a clean success (no network, a 5xx, a body we cannot read) is
//! [`Reply::Unavailable`]: telling someone their real key is bad because a
//! server hiccupped would be worse than saying nothing.
//!
//! The key and the certificate are never logged, here or by any caller.

use std::io::Read;
use std::time::Duration;

use serde_json::{json, Value};

/// Turn whatever a person typed or pasted into the canonical
/// `esk_XXXXX-XXXXX-XXXXX-XXXXX` shape, or `None` if it is not shaped like a
/// licence key. Case- and dash-insensitive, as Connections is; strict on the
/// shape only. Whether the key is valid is the server's question.
pub(crate) fn normalize_key(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix("esk_")?;
    let body: String = rest.chars().filter(|&c| c != '-').collect();
    if body.len() != 20 || !body.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    // All ASCII from here, so byte slicing cannot split a character.
    let upper = body.to_ascii_uppercase();
    Some(format!(
        "esk_{}-{}-{}-{}",
        &upper[0..5],
        &upper[5..10],
        &upper[10..15],
        &upper[15..20]
    ))
}

/// The last four characters of a canonical key: the ONLY part of a key this
/// app ever shows.
pub(crate) fn key_last4(canonical: &str) -> String {
    let chars: Vec<char> = canonical.chars().filter(|c| *c != '-').collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

/// What the redeem door answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    /// `200 { ok: true }`. `certificate` can be absent, with
    /// `certificate_error` saying why: redeemed, no proof yet.
    Accepted {
        replayed: bool,
        certificate: Option<String>,
        certificate_error: Option<String>,
    },
    /// `404 unknown_or_expired_key`: the key is bad, expired or revoked.
    Refused,
    /// Another definite "no" with a reason to show (this key is bound to a
    /// different machine, too many attempts, ...). Not a refusal of a licence
    /// already held: renewal treats it as "try again later".
    Rejected { message: String },
    /// No usable answer.
    Unavailable,
}

/// The longest server sentence shown to the user; the rest is cut.
const MESSAGE_MAX_CHARS: usize = 240;

/// Read an HTTP status and body from the redeem door. Pure, so it is driven
/// from hand-written replies in the tests exactly as the network drives it.
pub(crate) fn parse_reply(status: u16, body: &[u8]) -> Reply {
    let json: Option<Value> = serde_json::from_slice(body).ok();
    let field = |k: &str| {
        json.as_ref()
            .and_then(|v| v.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(MESSAGE_MAX_CHARS).collect::<String>())
    };
    match status {
        200..=299 => {
            let ok = json
                .as_ref()
                .and_then(|v| v.get("ok"))
                .and_then(Value::as_bool);
            if ok != Some(true) {
                return Reply::Unavailable;
            }
            let replayed = json
                .as_ref()
                .and_then(|v| v.get("replayed"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let certificate = json
                .as_ref()
                .and_then(|v| v.get("certificate"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .map(String::from);
            let certificate_error = if certificate.is_none() {
                Some(field("certificateError").unwrap_or_else(|| "no certificate was sent".into()))
            } else {
                None
            };
            Reply::Accepted {
                replayed,
                certificate,
                certificate_error,
            }
        }
        404 if field("error").as_deref() == Some("unknown_or_expired_key") => Reply::Refused,
        // Any other 404 is a route that moved or a proxy's page: no verdict.
        404 => Reply::Unavailable,
        429 => Reply::Rejected {
            message: "Too many attempts. Try again in a few minutes.".into(),
        },
        400..=499 => match field("message").or_else(|| field("error")) {
            Some(message) => Reply::Rejected { message },
            // A 4xx with no body we can read is a proxy or format surprise,
            // not a verdict on the key.
            None => Reply::Unavailable,
        },
        _ => Reply::Unavailable,
    }
}

/// The request body. The install id goes in `externalSubjectId` and NEVER in
/// `sub`, which Connections reads as one of its own account ids.
pub(crate) fn request_body(key: &str, subject: &str, fingerprint: &str) -> Value {
    json!({
        "key": key,
        "externalSubjectId": subject,
        "deviceFingerprint": fingerprint,
        "label": "QuickDictate",
    })
}

/// Whole-request timeout: generous for one small JSON call, short enough that
/// a dead network does not leave the Licence page spinning for long.
const TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Every reply is a few hundred bytes; this is headroom, not an expectation.
const MAX_REPLY_BYTES: u64 = 64 * 1024;

/// POST `key` for this install. BLOCKING: worker threads only.
pub(crate) fn post(key: &str, subject: &str, fingerprint: &str) -> Reply {
    let Ok(client) = crate::http::blocking_client(TIMEOUT, CONNECT_TIMEOUT) else {
        return Reply::Unavailable;
    };
    let sent = client
        .post(super::REDEEM_URL)
        .json(&request_body(key, subject, fingerprint))
        .send();
    let resp = match sent {
        Ok(r) => r,
        Err(e) => {
            // reqwest's error names the URL, never the body, so the key is
            // not in this line.
            tracing::info!("licence: redeem request failed ({e})");
            return Reply::Unavailable;
        }
    };
    let status = resp.status().as_u16();
    let mut body = Vec::new();
    if resp
        .take(MAX_REPLY_BYTES + 1)
        .read_to_end(&mut body)
        .is_err()
        || body.len() as u64 > MAX_REPLY_BYTES
    {
        return Reply::Unavailable;
    }
    let reply = parse_reply(status, &body);
    tracing::info!(
        "licence: redeem answered {status} ({})",
        reply_for_log(&reply)
    );
    reply
}

/// A log line for a reply that carries neither the key nor the certificate.
fn reply_for_log(r: &Reply) -> String {
    match r {
        Reply::Accepted {
            replayed,
            certificate,
            ..
        } => format!(
            "accepted, replayed={replayed}, certificate={}",
            if certificate.is_some() {
                "present"
            } else {
                "absent"
            }
        ),
        Reply::Refused => "refused".into(),
        Reply::Rejected { .. } => "rejected".into(),
        Reply::Unavailable => "unavailable".into(),
    }
}
