//! What travels to the cloud, and what must never.
//!
//! The `SYNCED_KEYS` allowlist and the transforms between a `Config` and the
//! flat JSON document the store holds, including the stats merge.

use serde_json::Value;

use crate::config::Config;
use crate::stats::UsageStats;

use super::STATS_KEY;

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
///   * `data_dir` — an absolute path on *this* PC. Syncing it would point a
///     second machine at a folder that may not exist there (or, worse, at
///     somebody else's folder that does);
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
pub(super) const SYNCED_KEYS: &[&str] = &[
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
    "mouse_hotkey_passthrough",
    "input_device",
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
    // The LLM cleanup pass. Portable and secret-free: whether you want it,
    // how long the paste may wait for it, and which endpoint/model answers.
    // Its key is a secret and stays in NEVER_SYNCED with the others.
    "polish_enabled",
    "polish_deadline_ms",
    "polish_endpoint",
    "polish_model",
    // ── Widened 2026-08-25 ──────────────────────────────────────────────────────
    // Four fields whose exclusion was an assertion rather than a mechanism. Everything
    // still in NEVER_SYNCED below has an actual reason it cannot travel; these did not.
    "hide_tray_icon", // "don't show me a tray icon" is a fact about YOU, not about the PC
    "enable_logging", // a diagnostics preference; the log itself never leaves the machine
    "max_log_mb",     // the cap that goes with it
    // "seal my API keys at rest" is a portable intent. It was excluded as "meaningless
    // elsewhere" because DPAPI binds to one Windows account — but the SETTING is not the
    // sealed blob: on another machine it seals THAT machine's keys with THAT account,
    // which is exactly what someone who turned it on here would want.
    "protect_keys_at_rest",
    // "I'm fine with LunarWerx seeing an anonymized usage rollup" is a stated preference
    // about the person, same shape as `update_auto_check` — not a machine property. Only
    // the boolean travels; each machine still reports under its own `install_id` (which
    // stays in NEVER_SYNCED below), so this can never merge two machines' identities.
    "share_usage_stats",
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
pub(super) const NEVER_SYNCED: &[&str] = &[
    "elevenlabs_keys", // secret API key array
    "deepgram_keys",   // secret API key array
    "openai_keys",     // secret API key array
    "assemblyai_keys", // secret API key array
    "dashscope_keys",  // secret API key array
    "google_keys",     // secret API key array
    "polish_keys",     // secret API key array (LLM cleanup endpoint)
    "local_keys",      // legacy secret API key array, folds into elevenlabs_keys
    "window_width",    // machine-local window geometry
    "window_height",   // machine-local window geometry
    "window_x",        // machine-local window geometry
    "window_y",        // machine-local window geometry
    // Writes an HKCU Run entry on whatever machine it lands on. That is a change to the
    // system, not a preference being read — so it stays a per-machine decision.
    "run_at_startup",
    "data_dir", // an absolute path on THIS PC; meaningless (or wrong) on another
    // The one logging flag that stays: it writes your dictated TEXT to disk. Its companions
    // above now sync; turning this one on somewhere from somewhere else is a privacy change
    // being made for you, on a machine you were not looking at.
    "log_transcripts",
    "install_id", // anonymous per-install id; syncing would merge two machines' identities
    "update_auto_install", // machine-local unattended-update policy choice
];

// ---- Allowlist transforms (Config <-> synced JSON) -------------------------

/// The portable subset of a `Config` as a flat JSON object — exactly the keys in
/// [`SYNCED_KEYS`], nothing else. This is what we push to the store.
pub(super) fn config_to_synced(cfg: &Config) -> Value {
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

/// Merge only the stats portion of `other` into `preferred`; portable settings
/// in `preferred` keep their existing conflict policy.
pub(super) fn merge_stats(preferred: &mut Value, other: &Value) -> bool {
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
