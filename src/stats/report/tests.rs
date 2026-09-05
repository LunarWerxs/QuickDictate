//! Would fail if `anonymized_payload` regressed to serializing `UsageStats`
//! wholesale (its actual field list, not a tautology): the struct carries
//! `local_device_id` / `devices` / `archived_device_ids` for the sync-merge
//! machinery, none of which this report may ever send.

use super::*;
use crate::stats::DeviceStats;

fn stats_with_device_identity() -> UsageStats {
    let mut stats = UsageStats {
        local_device_id: "should-never-leave-this-machine".into(),
        total_words: 42,
        total_audio_ms: 9_000,
        total_dictations: 3,
        longest_dictation_words: 30,
        longest_dictation_audio_ms: 5_000,
        ..Default::default()
    };
    stats
        .devices
        .insert("dev-should-not-leak".into(), DeviceStats::default());
    stats
        .archived_device_ids
        .insert("archived-should-not-leak".into());
    stats
        .providers
        .insert("elevenlabs".into(), Default::default());
    stats
}

#[test]
fn payload_carries_only_the_allowlisted_aggregate_fields() {
    let stats = stats_with_device_identity();
    let payload = anonymized_payload("anon-install-id", &stats);
    let obj = payload.as_object().expect("payload is a JSON object");

    let allowed = [
        "install_id",
        "app_version",
        "total_words",
        "total_audio_ms",
        "total_dictations",
        "longest_dictation_words",
        "longest_dictation_audio_ms",
        "providers",
    ];
    for key in obj.keys() {
        assert!(
            allowed.contains(&key.as_str()),
            "unexpected field leaked into the usage report: {key}"
        );
    }

    assert_eq!(payload["install_id"], "anon-install-id");
    assert_eq!(payload["app_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(payload["total_words"], 42);
    assert_eq!(payload["total_audio_ms"], 9_000);
    assert_eq!(payload["total_dictations"], 3);
}

#[test]
fn payload_never_contains_device_or_sync_identifiers() {
    let stats = stats_with_device_identity();
    let payload = anonymized_payload("anon-install-id", &stats);
    let text = payload.to_string();

    // The real regression this guards: a future `serde_json::to_value(stats)`
    // shortcut would put these substrings right back into the wire body.
    assert!(!text.contains("should-never-leave-this-machine"));
    assert!(!text.contains("dev-should-not-leak"));
    assert!(!text.contains("archived-should-not-leak"));
    assert!(!text.contains("local_device_id"));
    assert!(!text.contains("archived_device_ids"));
    // `devices` as a JSON *key* specifically -- "providers" legitimately
    // rides along, so this checks the quoted field name, not a bare substring.
    assert!(!text.contains("\"devices\""));
}

#[test]
fn payload_provider_breakdown_carries_only_counts() {
    let mut stats = UsageStats::default();
    stats
        .providers
        .insert("deepgram".into(), Default::default());
    let payload = anonymized_payload("id", &stats);
    let providers = payload["providers"]["deepgram"]
        .as_object()
        .expect("per-provider entry is an object");
    let allowed = ["words", "audio_ms", "dictations"];
    for key in providers.keys() {
        assert!(allowed.contains(&key.as_str()), "unexpected key: {key}");
    }
}

#[test]
fn share_usage_stats_defaults_to_off() {
    // Would fail if the opt-in default were ever flipped to true, since this
    // is a network call to LunarWerx and consent must be affirmative.
    assert!(!crate::config::Config::default().share_usage_stats);
}
