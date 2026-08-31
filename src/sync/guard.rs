//! The last line of defence before anything leaves the machine.
//!
//! Scans an outgoing document for values that look like credentials and
//! refuses the push, so an allowlist mistake cannot become a key leak.

use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::Value;

use super::MAX_DOCUMENT_BYTES;

/// The raw (pattern, label) pairs behind [`credential_patterns`], named so the
/// compiled list can be checked against it: a pattern that fails to compile is
/// dropped rather than panicking (right for a background thread), and a
/// silently-dropped pattern is a silently-disabled arm of the scanner. See
/// `every_credential_pattern_compiles`.
pub(super) const CREDENTIAL_PATTERNS: &[(&str, &str)] = &[
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
];

pub(super) fn credential_patterns() -> &'static [(Regex, &'static str)] {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            CREDENTIAL_PATTERNS
                .iter()
                // Dropping a bad pattern beats panicking here: this runs on the
                // sync worker, where a panic is silent. The drop is what
                // `every_credential_pattern_compiles` exists to catch.
                .filter_map(|(pattern, label)| Regex::new(pattern).ok().map(|re| (re, *label)))
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

pub(super) fn validate_sync_snapshot(settings: &Value) -> Result<()> {
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
