#![allow(clippy::field_reassign_with_default)] // test setup reads clearer field-by-field
//! Tests for the Connections settings sync.

use super::guard::{credential_patterns, validate_sync_snapshot, CREDENTIAL_PATTERNS};
use super::schema::{config_to_synced, NEVER_SYNCED, SYNCED_KEYS};
use super::store::{parse_etag_version, retry_after_seconds};
use super::*;
use crate::config::Config;
use crate::stats::{DeviceStats, PeriodStats, ProviderStats, UsageStats};
use std::collections::BTreeMap;

/// `credential_patterns` drops a pattern that fails to compile rather than
/// panicking, which is right for a background thread and wrong to leave
/// unchecked: a silently-dropped pattern is a silently-disabled arm of the
/// scanner that stops secrets reaching the sync endpoint. Pin the count so
/// a typo in a literal fails here instead of shipping a hole.
#[test]
fn every_credential_pattern_compiles() {
    let compiled: Vec<&str> = credential_patterns().iter().map(|(_, l)| *l).collect();
    let declared: Vec<&str> = CREDENTIAL_PATTERNS.iter().map(|(_, l)| *l).collect();
    assert_eq!(
        compiled, declared,
        "a credential pattern failed to compile and was silently dropped"
    );
}

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
        // The transcript log is the one diagnostics flag that stays home: it writes your
        // dictated TEXT to disk, so switching it on from another machine would be a privacy
        // change made on your behalf somewhere you were not looking. `enable_logging` and
        // `max_log_mb` DO sync as of 2026-08-25 — they only control an ordinary app log.
        "log_transcripts",
        "install_id",
        "data_dir",
    ] {
        assert!(
            !obj.contains_key(forbidden),
            "{forbidden} must never be in the synced snapshot"
        );
    }

    // The four widened on 2026-08-25. Asserted by name because the failure they fix was
    // silent in exactly this direction: a portable preference simply never travelling, with
    // nothing anywhere going red about it.
    for widened in [
        "hide_tray_icon",
        "enable_logging",
        "max_log_mb",
        "protect_keys_at_rest",
    ] {
        assert!(
            obj.contains_key(widened),
            "{widened} was widened into SYNCED_KEYS and must appear in the snapshot"
        );
    }
}

#[test]
fn full_snapshot_includes_stats_but_not_the_local_device_marker() {
    let mut stats = UsageStats::default();
    stats.local_device_id = "local-only-marker".into();
    stats.devices.insert(
        "device-a".into(),
        DeviceStats {
            totals: PeriodStats {
                words: 42,
                audio_ms: 9_000,
                dictations: 2,
                ..PeriodStats::default()
            },
            ..DeviceStats::default()
        },
    );

    let snapshot = snapshot_to_synced(&Config::default(), &stats);
    assert!(snapshot.get(STATS_KEY).is_some());
    let json = serde_json::to_string(&snapshot).unwrap();
    assert!(!json.contains("local-only-marker"));
    assert!(!json.contains("local_device_id"));
}

#[test]
fn stats_merge_unions_devices_without_changing_preferred_settings() {
    let mut local_stats = UsageStats::default();
    local_stats.devices.insert(
        "local-device".into(),
        DeviceStats {
            totals: PeriodStats {
                words: 10,
                dictations: 1,
                ..PeriodStats::default()
            },
            ..DeviceStats::default()
        },
    );
    let mut remote_stats = UsageStats::default();
    remote_stats.devices.insert(
        "remote-device".into(),
        DeviceStats {
            totals: PeriodStats {
                words: 20,
                dictations: 1,
                ..PeriodStats::default()
            },
            ..DeviceStats::default()
        },
    );
    let mut preferred = serde_json::json!({
        "language": "en-US",
        (STATS_KEY): local_stats.synced_value(),
    });
    let other = serde_json::json!({
        "language": "de-DE",
        (STATS_KEY): remote_stats.synced_value(),
    });

    assert!(merge_stats(&mut preferred, &other));
    assert_eq!(preferred["language"], "en-US");
    let mut merged = UsageStats::default();
    assert!(merged.merge_synced_value(&preferred[STATS_KEY]));
    assert_eq!(merged.total_words, 30);
    assert_eq!(merged.total_dictations, 2);
}

#[test]
fn compact_week_of_hourly_stats_stays_below_store_limit() {
    let mut providers = BTreeMap::new();
    providers.insert(
        "local".into(),
        ProviderStats {
            words: 30,
            audio_ms: 8_000,
            dictations: 1,
        },
    );
    providers.insert(
        "openai".into(),
        ProviderStats {
            words: 30,
            audio_ms: 8_000,
            dictations: 1,
        },
    );
    let bucket = PeriodStats {
        words: 60,
        audio_ms: 16_000,
        dictations: 2,
        longest_dictation_words: 30,
        longest_dictation_audio_ms: 8_000,
        providers,
    };
    let mut hours = BTreeMap::new();
    for hour in 0..(24 * 8) {
        hours.insert(1_800_000_000 + hour * 3_600, bucket.clone());
    }
    let mut stats = UsageStats::default();
    stats.devices.insert(
        "device-with-a-full-week".into(),
        DeviceStats {
            totals: bucket,
            hours,
            ..DeviceStats::default()
        },
    );

    let snapshot = snapshot_to_synced(&Config::default(), &stats);
    let bytes = serde_json::to_vec(&snapshot).unwrap();
    assert!(
        bytes.len() < 64 * 1_024,
        "snapshot was {} bytes",
        bytes.len()
    );
}

#[test]
fn credential_values_are_refused_even_when_nested() {
    let snapshot = serde_json::json!({
        "text_replacements": [{
            "from": "work token",
            "to": "ghp_abcdefghijklmnopqrstuvwxyz0123456789"
        }]
    });
    let error = validate_sync_snapshot(&snapshot).unwrap_err().to_string();
    assert!(error.contains("GitHub token"), "{error}");

    assert!(validate_sync_snapshot(&serde_json::json!({
        "text_replacements": [{"from": "sk", "to": "ordinary text"}]
    }))
    .is_ok());
}

#[test]
fn credential_patterns_catch_prefixed_stt_keys_but_not_bare_hex() {
    // Prefixed key shapes are unambiguous and must be caught.
    for (value, provider) in [
        ("sk_0123456789abcdef0123456789abcdef01234567", "ElevenLabs"),
        ("sk-0123456789abcdef0123456789abcdef", "DashScope"),
    ] {
        let snapshot = serde_json::json!({
            "text_replacements": [{"from": "x", "to": value}]
        });
        let error = validate_sync_snapshot(&snapshot).unwrap_err().to_string();
        assert!(
            error.contains(provider),
            "expected the {provider} key pattern to catch {value:?}, got: {error}"
        );
    }
    // Bare 32/40-char hex is NOT flagged: it is indistinguishable from a
    // git SHA, an MD5 hash, or a dashless GUID, and flagging it would
    // permanently block sync for a user with one such string in a synced
    // text field. The SYNCED_KEYS allowlist is the real guard for the
    // prefixless Deepgram/AssemblyAI shapes.
    for value in [
        "2f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a", // git-SHA shaped
        "9e11c31e751b4a12a3f9f4c3b2b1a123",         // MD5/GUID shaped
    ] {
        let snapshot = serde_json::json!({
            "text_replacements": [{"from": "commit", "to": value}],
            "custom_vocabulary": [value]
        });
        assert!(
            validate_sync_snapshot(&snapshot).is_ok(),
            "bare hex {value:?} must not block a legitimate sync push"
        );
    }
}

#[test]
fn oversized_snapshot_is_rejected_locally_and_names_the_largest_key() {
    let snapshot = serde_json::json!({
        "language": "en-US",
        "text_replacements": "x".repeat(MAX_DOCUMENT_BYTES)
    });
    let error = validate_sync_snapshot(&snapshot).unwrap_err().to_string();
    assert!(error.contains("over the 65536-byte limit"), "{error}");
    assert!(error.contains("text_replacements"), "{error}");
}

#[test]
fn etag_and_rate_limit_contract_fields_are_parsed() {
    assert_eq!(parse_etag_version("\"42\""), Some(42));
    assert_eq!(parse_etag_version("W/\"7\""), Some(7));
    assert_eq!(
        retry_after_seconds(&serde_json::json!({"retry_after_seconds": 9})),
        Some(9)
    );
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
fn apply_replaces_an_unavailable_synced_local_model() {
    let mut local = Config::default();
    let remote = serde_json::json!({ "local_model": "retired-model" });
    assert!(apply_synced_to_config(&mut local, &remote));
    assert_eq!(local.local_model, "cohere-q5");
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

/// Guard against the exact drift that motivated [`NEVER_SYNCED`]: a
/// portable `Config` field gets added but nobody remembers to add it to
/// [`SYNCED_KEYS`], so it silently never syncs. Serializes a real
/// `Config::default()` and checks every JSON key is filed into exactly
/// one of the two lists, so a new field breaks this test (naming itself)
/// until someone decides where it belongs.
#[test]
fn every_config_field_is_synced_or_never_synced() {
    let value = serde_json::to_value(Config::default()).expect("Config serializes");
    let obj = value.as_object().expect("Config serializes to an object");

    for key in obj.keys() {
        let synced = SYNCED_KEYS.contains(&key.as_str());
        let never = NEVER_SYNCED.contains(&key.as_str());
        assert!(
            synced || never,
            "Config field \"{key}\" is in neither SYNCED_KEYS nor NEVER_SYNCED — \
             decide whether it should sync and add it to one of them"
        );
        assert!(
            !(synced && never),
            "Config field \"{key}\" is listed in BOTH SYNCED_KEYS and NEVER_SYNCED"
        );
    }

    // Catch the reverse mistake too: a stale or misspelled name sitting in
    // one of the lists that no longer (or never did) match a real field.
    for key in SYNCED_KEYS.iter().chain(NEVER_SYNCED.iter()) {
        assert!(
            obj.contains_key(*key),
            "\"{key}\" is listed in SYNCED_KEYS/NEVER_SYNCED but is not a Config field"
        );
    }
}
