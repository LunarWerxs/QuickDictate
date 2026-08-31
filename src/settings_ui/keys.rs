//! The provider list, the key arrays behind it, and the bulk key editor's
//! parsing: which providers exist, where their keys live, and how a pasted
//! block of lines becomes rows without losing the ones already tested.

use std::collections::HashSet;

use crate::config::Config;

// Split out of this file so each surface can be reviewed on its own; the

/// (id, label) for the provider dropdown. Google only exists in builds with
/// the `google` feature (the published binaries have it).
pub(super) fn providers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("elevenlabs", "ElevenLabs"),
        ("deepgram", "Deepgram"),
        ("openai", "OpenAI"),
        ("assemblyai", "AssemblyAI"),
        ("dashscope", "DashScope (Alibaba)"),
        ("google", "Google (batch)"),
        ("local", "Local (offline)"),
    ]
}

pub(super) fn provider_label(id: &str) -> &str {
    if id.eq_ignore_ascii_case("mixed") {
        return "Mixed providers";
    }
    providers()
        .iter()
        .find(|(pid, _)| *pid == id)
        .map(|(_, l)| *l)
        .unwrap_or("Unknown")
}

/// `keys_of` id for the cleanup pass's own pool. It is not an STT provider,
/// but it is edited by the very same key manager, so it rides the same path
/// rather than growing a second, near-identical keys editor. NUL-prefixed so
/// `@`-prefixed so it can never collide with a real provider id.
pub(crate) const KEYS_TARGET_POLISH: &str = "@polish";
/// `keys_of` id meaning "whatever STT provider the draft currently selects",
/// resolved at use time so switching providers with the modal shut cannot
/// leave the target pointing at the old one.
pub(crate) const KEYS_TARGET_PROVIDER: &str = "@provider";

pub(super) fn keys_of<'a>(cfg: &'a mut Config, id: &str) -> &'a mut Vec<String> {
    if id == KEYS_TARGET_POLISH {
        return &mut cfg.polish_keys;
    }
    let selected;
    let id = if id == KEYS_TARGET_PROVIDER {
        selected = cfg.stt_provider.clone();
        selected.as_str()
    } else {
        id
    };
    match id {
        "deepgram" => &mut cfg.deepgram_keys,
        "openai" => &mut cfg.openai_keys,
        "assemblyai" => &mut cfg.assemblyai_keys,
        "dashscope" => &mut cfg.dashscope_keys,
        "google" => &mut cfg.google_keys,
        _ => &mut cfg.elevenlabs_keys,
    }
}

/// `sk_c35a…dad4d0` — enough to recognize a key, never the whole secret.
pub(super) fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 4 {
        return "\u{2022}".repeat(chars.len());
    }
    if chars.len() <= 12 {
        let head: String = chars[..2].iter().collect();
        let tail: String = chars[chars.len() - 2..].iter().collect();
        return format!("{head}\u{2026}{tail}");
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 6..].iter().collect();
    format!("{head}\u{2026}{tail}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Untested,
    Testing,
    Ok,
    Fail,
}

pub(super) struct KeyRow {
    pub(super) value: String,
    pub(super) verdict: Verdict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct KeyMergeSummary {
    pub(super) added: usize,
    pub(super) duplicates: usize,
}

/// Merge one-key-per-line text into the manager without ever echoing a secret.
/// Validation is atomic: an invalid line leaves the existing rows untouched.
pub(super) fn merge_key_lines(
    rows: &mut Vec<KeyRow>,
    text: &str,
) -> Result<KeyMergeSummary, Vec<usize>> {
    let mut candidates = Vec::new();
    let mut invalid_lines = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let key = line.trim();
        if key.is_empty() {
            continue;
        }
        if key.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            invalid_lines.push(index + 1);
        } else {
            candidates.push(key.to_string());
        }
    }
    if !invalid_lines.is_empty() {
        return Err(invalid_lines);
    }

    let mut seen: HashSet<String> = rows.iter().map(|row| row.value.clone()).collect();
    let mut added = 0usize;
    let mut duplicates = 0usize;
    for key in candidates {
        if seen.insert(key.clone()) {
            rows.push(KeyRow {
                value: key,
                verdict: Verdict::Untested,
            });
            added += 1;
        } else {
            duplicates += 1;
        }
    }
    Ok(KeyMergeSummary { added, duplicates })
}

pub(super) fn deduped_key_values(rows: &[KeyRow]) -> Vec<String> {
    let mut seen = HashSet::new();
    rows.iter()
        .filter_map(|row| {
            let value = row.value.trim();
            (!value.is_empty() && seen.insert(value.to_string())).then(|| value.to_string())
        })
        .collect()
}
