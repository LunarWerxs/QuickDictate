//! Defaults for every serde-optional field.
//!
//! Named functions rather than literals because that is what `#[serde(default
//! = "...")]` takes, and because [`Config::default`](super::Config::default)
//! is built from the same set -- one value, one place.

use std::collections::BTreeMap;

pub(super) const fn default_true() -> bool {
    true
}
pub(super) const fn default_false() -> bool {
    false
}
pub(super) fn default_toggle_hotkey() -> String {
    "f14".into()
}
pub(super) fn default_hold_hotkey() -> String {
    "f13".into()
}
pub(super) fn default_reinsert_hold_ms() -> u64 {
    1500
}
pub(super) fn default_listen_tail_ms() -> u64 {
    800
}
pub(super) fn default_clipboard_restore_delay_ms() -> u64 {
    300
}
pub(super) fn default_max_log_mb() -> u64 {
    5
}
pub(super) fn default_language() -> String {
    "en-US".into()
}
pub(super) fn default_provider() -> String {
    "elevenlabs".into()
}
pub(super) fn default_local_model() -> String {
    crate::local_stt::default_model_id()
}
pub(super) fn default_mode() -> String {
    "toggle".into()
}
pub(super) fn default_close() -> String {
    "minimize".into()
}
pub(super) fn default_spinner() -> String {
    "star_wars".into()
}
pub(super) fn default_width() -> u32 {
    280
}
pub(super) fn default_height() -> u32 {
    140
}

pub(super) fn default_replacements() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for (k, v) in [
        ("Super Bass", "Supabase"),
        ("super bass", "Supabase"),
        ("Clouded flyer", "Cloudflare"),
        ("clouded flyer", "Cloudflare"),
        ("Cloud flyer", "Cloudflare"),
        ("cloud flyer", "Cloudflare"),
        ("Chat GPT", "ChatGPT"),
        ("chat gpt", "ChatGPT"),
        ("Github", "GitHub"),
        ("github", "GitHub"),
    ] {
        m.insert(k.into(), v.into());
    }
    m
}

pub(super) fn default_replacements_mode() -> String {
    "extend".into()
}

pub(super) fn default_polish_deadline_ms() -> u64 {
    300
}

pub(super) fn default_polish_endpoint() -> String {
    "https://api.openai.com/v1/chat/completions".into()
}

/// The default is the best OpenAI option, because `polish_keys` falls back to
/// `openai_keys` and that is the key most people already have. **It is not the
/// best option overall.** Measured 2026-08-13, same 520-character dictation
/// and same edit-list prompt for every row, median of 3:
///
/// | model                 | median  | result                                |
/// |-----------------------|---------|---------------------------------------|
/// | gemini-3.5-flash-lite | ~0.56 s | 4 edits, all correct. **Best.**       |
/// | gemini-flash-lite-latest | ~0.63 s | 3 edits, all correct.              |
/// | gemini-3.6-flash      | ~0.99 s | 3 edits, all correct.                 |
/// | gemini-3.1-flash-lite | ~1.07 s | 3 edits, all correct.                 |
/// | gpt-4.1-nano          | ~1.7 s  | 3 edits, every one a no-op. Useless.  |
/// | gpt-4.1-mini          | ~2.0 s  | 3 edits, all correct.                 |
/// | gemini-3.7-flash      | ~2.1 s  | 3 edits, all correct. Overkill here.  |
/// | gemini-3.5-flash      | ~3.3 s  | 3 edits, all correct.                 |
/// | gpt-5-nano            | ~7.1 s  | spent the whole token budget thinking |
/// | gpt-5-mini            | ~10.4 s | same, worse.                          |
///
/// Three things that keep being true: the *lite* tiers win outright (this is
/// a small mechanical edit, not a reasoning problem, and the biggest model is
/// the slowest for no gain); never pick a model that thinks before answering,
/// which is what buried both gpt-5 rows; and `-nano`-class OpenAI models are
/// too weak to produce a single real edit.
///
/// Point `polish_endpoint` at
/// `https://generativelanguage.googleapis.com/v1beta/openai/chat/completions`
/// with `polish_model: "gemini-3.5-flash-lite"` and `polish_keys` to get the
/// top row. At ~0.6 s the deadline race is winnable outright rather than
/// depending on the speculative pass.
pub(super) fn default_polish_model() -> String {
    "gpt-4.1-mini".into()
}
