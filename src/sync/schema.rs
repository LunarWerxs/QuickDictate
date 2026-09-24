//! What travels to the cloud, and what must never.
//!
//! The `SYNCED_KEYS` allowlist and the transforms between a `Config` and the
//! flat JSON document the store holds, including the stats merge.

use serde_json::Value;

use crate::config::Config;
use crate::stats::UsageStats;

use super::STATS_KEY;

/// The **allowlist** of settings.json keys that sync to the cloud. Deliberately
/// excludes (the full list, with reasons, is [`NEVER_SYNCED`]):
///
///   * every `*_keys` / `local_keys` array — **secrets, never synced**;
///   * `window_width/height/x/y` — machine-local window geometry;
///   * `run_at_startup` — per-machine registry (Run key) behavior;
///   * `log_transcripts` — writes your dictated text to disk, so turning it on
///     from another machine would be a privacy change made for you;
///   * `data_dir` — an absolute path on *this* PC. Syncing it would point a
///     second machine at a folder that may not exist there (or, worse, at
///     somebody else's folder that does);
///   * `install_id` — this install's anonymous update-check id; syncing it
///     would merge two machines' identities into one;
///   * `update_auto_install` — a machine-local policy choice (whether *this*
///     machine applies updates unattended); syncing it would silently opt a
///     second machine into unattended installs.
///
/// `hide_tray_icon`, `enable_logging`, `max_log_mb` and `protect_keys_at_rest`
/// were once on that list and now sync; the note at their entries says why.
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
    "duck_other_audio",
    "duck_volume_percent",
    "duck_fade",
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
    // "keep my dictation history between restarts" is a preference about the person, the
    // same shape as `enable_logging`. Only the boolean travels; the history file itself
    // is local-only and never synced (see `history_store`).
    "persist_history",
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
    // Same reasoning as `log_transcripts`: turning this on somewhere from
    // somewhere else would be a privacy-relevant decision (whether local
    // diagnostics get assembled into a report you can hand to LunarWerx)
    // made on a machine you were not looking at.
    "error_reporting_enabled",
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

/// The usage statistics alone, as a document of their own: what the
/// background and exit pushes send. Merge mode leaves every other cloud key as
/// it is, so a machine that has not pulled lately cannot write its stale
/// settings over a change made on another machine since.
pub(super) fn stats_to_synced(stats: &UsageStats) -> Value {
    let mut out = serde_json::Map::new();
    out.insert(STATS_KEY.to_string(), stats.synced_value());
    Value::Object(out)
}

/// Make a push delete what this machine removed. The store merges pushes as
/// RFC 7386 JSON Merge Patch, deep: a key a nested object leaves out is KEPT
/// on the server, and only an explicit `null` deletes it. So without this, a
/// text replacement removed here stayed in the cloud and came back on the next
/// pull. For every object-valued key `local` carries (text_replacements
/// today), each key `remote` holds under it that `local` no longer does gets a
/// `null`, recursively. Top-level keys `local` omits are left alone on
/// purpose: a stats-only push omits every preference, and must not wipe them.
/// The stats object is exempt too; `merge_stats` already unions it.
pub(super) fn with_deletions(local: &mut Value, remote: &Value) {
    let (Some(local), Some(remote)) = (local.as_object_mut(), remote.as_object()) else {
        return;
    };
    for (key, value) in local.iter_mut() {
        if key == STATS_KEY {
            continue;
        }
        if let Some(theirs) = remote.get(key) {
            null_out_removed(value, theirs);
        }
    }
}

fn null_out_removed(ours: &mut Value, theirs: &Value) {
    let (Some(ours), Some(theirs)) = (ours.as_object_mut(), theirs.as_object()) else {
        return;
    };
    for (key, their_value) in theirs {
        match ours.get_mut(key) {
            Some(our_value) => null_out_removed(our_value, their_value),
            None => {
                ours.insert(key.clone(), Value::Null);
            }
        }
    }
}

/// What `local` changed since `base` (the document as this machine last saw
/// it), as a merge patch: only keys whose value moved, nested objects diffed
/// key by key with `null` for what was removed. Pushing this instead of the
/// whole snapshot is a three-way merge: a setting another PC changed that
/// this one did not touch is left as the other PC set it, instead of being
/// overwritten with this PC's stale copy. Top-level keys `local` omits are not
/// compared (a stats-only push carries no preferences), and the stats object
/// always goes, already unioned by `merge_stats`.
pub(super) fn changes_since(local: &Value, base: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(local) = local.as_object() {
        for (key, value) in local {
            let change = if key == STATS_KEY {
                Some(value.clone())
            } else {
                diff_value(value, base.get(key))
            };
            if let Some(change) = change {
                out.insert(key.clone(), change);
            }
        }
    }
    Value::Object(out)
}

/// One value's part of [`changes_since`]: `None` if unchanged since `base`;
/// for two objects, a nested patch naming only what changed; otherwise the
/// value whole.
fn diff_value(ours: &Value, base: Option<&Value>) -> Option<Value> {
    match (ours, base) {
        (_, Some(base)) if ours == base => None,
        (Value::Object(ours), Some(Value::Object(base))) => {
            let mut patch = serde_json::Map::new();
            for (key, value) in ours {
                if let Some(change) = diff_value(value, base.get(key)) {
                    patch.insert(key.clone(), change);
                }
            }
            for key in base.keys().filter(|key| !ours.contains_key(*key)) {
                patch.insert(key.clone(), Value::Null);
            }
            (!patch.is_empty()).then_some(Value::Object(patch))
        }
        _ => Some(ours.clone()),
    }
}

/// Apply `patch` to `target` the way the store does (RFC 7386): objects merge
/// key by key, `null` deletes, anything else replaces. Keeps this machine's
/// picture of the cloud copy in step after a push without pulling it again.
pub(super) fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(serde_json::Map::new());
    }
    if let Value::Object(target) = target {
        for (key, value) in patch {
            if value.is_null() {
                target.remove(key);
            } else {
                merge_patch(target.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
    }
}

/// The part of `remote` to apply here: every key whose value changed since
/// `base`, whole (the Settings overlay replaces a key's value outright). A
/// key the cloud still holds as this machine last saw it is left out, so a
/// local change that has not reached the cloud yet is not reverted by it.
pub(super) fn remote_changes(remote: &Value, base: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(remote) = remote.as_object() {
        for (key, value) in remote {
            if base.get(key) != Some(value) {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(out)
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
