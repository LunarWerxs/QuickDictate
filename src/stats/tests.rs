//! Tests for the usage counters, their merge, and the store.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use super::*;

fn temp_stats_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "quickdictate-{name}-{}-{nonce}.json",
        std::process::id()
    ))
}

#[test]
fn aggregation_tracks_totals_longest_and_providers() {
    let mut stats = UsageStats::default();
    stats.record("ElevenLabs", 12, 5_000, 100);
    stats.record("local", 30, 20_000, 200);
    stats.record("local", 8, 40_000, 300);

    assert_eq!(stats.total_words, 50);
    assert_eq!(stats.total_audio_ms, 65_000);
    assert_eq!(stats.total_dictations, 3);
    assert_eq!(stats.longest_dictation_words, 30);
    assert_eq!(stats.longest_dictation_audio_ms, 40_000);
    assert_eq!(stats.tracking_started_unix_secs, 100);
    assert_eq!(stats.last_dictation_unix_secs, 300);
    assert_eq!(
        stats.providers["local"],
        ProviderStats {
            words: 38,
            audio_ms: 60_000,
            dictations: 2,
        }
    );
}

#[test]
fn zero_word_attempt_is_not_a_dictation() {
    let mut stats = UsageStats::default();
    stats.record("local", 0, 99_000, 100);
    assert_eq!(stats, UsageStats::default());
}

#[test]
fn old_json_with_missing_fields_loads_via_defaults() {
    let stats: UsageStats = serde_json::from_str(r#"{"total_words":42}"#).unwrap();
    assert_eq!(stats.total_words, 42);
    assert_eq!(stats.total_dictations, 0);
    assert!(stats.providers.is_empty());
}

#[test]
fn store_round_trips_and_recovers_from_corrupt_json() {
    let path = temp_stats_path("stats-round-trip");
    let store = Arc::new(StatsStore::load_from(path.clone()));
    store.record_dictation("openai", 7, 2_500);
    let reloaded = StatsStore::load_from(path.clone()).snapshot();
    assert_eq!(reloaded.total_words, 7);
    assert_eq!(reloaded.total_audio_ms, 2_500);

    fs::write(&path, b"{not json").unwrap();
    let recovered = StatsStore::load_from(path.clone()).snapshot();
    assert_eq!(recovered.total_dictations, 0);
    assert_eq!(recovered.total_words, 0);
    assert!(recovered.devices.is_empty());
    assert!(!recovered.local_device_id.is_empty());
    assert!(path.with_extension("json.bad").exists());

    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("json.bad"));
}

#[test]
fn store_recovers_backup_left_by_an_interrupted_save() {
    let path = temp_stats_path("stats-backup-recovery");
    let backup = path.with_extension("json.bak");
    let mut stats = UsageStats::default();
    stats.record("local", 11, 4_000, 100);
    fs::write(&backup, serde_json::to_vec(&stats).unwrap()).unwrap();

    let recovered = StatsStore::load_from(path.clone()).snapshot();
    assert_eq!(recovered.total_words, 11);
    assert_eq!(recovered.total_audio_ms, 4_000);

    let _ = fs::remove_file(path);
    let _ = fs::remove_file(backup);
}

#[test]
fn flush_hands_complete_totals_to_a_replacement_store() {
    let path = temp_stats_path("stats-relaunch-flush");
    let old = Arc::new(StatsStore::load_from(path.clone()));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    runtime.block_on(async {
        old.record_dictation("local", 9, 3_000);
    });
    old.flush();

    // Mirrors the replacement process: it is only launched after flush,
    // loads the durable total, then records its own later dictation.
    let replacement = Arc::new(StatsStore::load_from(path.clone()));
    replacement.record_dictation("local", 4, 1_000);
    let final_stats = StatsStore::load_from(path.clone()).snapshot();
    assert_eq!(final_stats.total_words, 13);
    assert_eq!(final_stats.total_audio_ms, 4_000);
    assert_eq!(final_stats.total_dictations, 2);

    drop(runtime);
    let _ = fs::remove_file(path);
}

#[test]
fn recent_ranges_and_chart_use_hourly_history() {
    let now = 1_000 * HOUR_SECS;
    let mut stats = UsageStats::default();
    stats.record("local", 5, 1_000, now - 25 * HOUR_SECS);
    stats.record("local", 7, 2_000, now - 3 * HOUR_SECS);
    stats.record("openai", 11, 3_000, now);

    let day = stats.view(StatsRange::Last24Hours, now);
    assert_eq!(day.totals.words, 18);
    assert_eq!(day.totals.dictations, 2);
    assert_eq!(day.chart.iter().sum::<u64>(), 2);

    let week = stats.view(StatsRange::Last7Days, now);
    assert_eq!(week.totals.words, 23);
    assert_eq!(week.totals.dictations, 3);
    assert_eq!(week.chart.iter().sum::<u64>(), 3);

    let all = stats.view(StatsRange::AllTime, now);
    assert_eq!(all.totals.words, 23);
    assert_eq!(all.totals.dictations, 3);
}

#[test]
fn synced_device_counters_merge_idempotently() {
    let now = 1_000 * HOUR_SECS;
    let mut first = UsageStats {
        local_device_id: "first-device".into(),
        ..UsageStats::default()
    };
    first.record("local", 5, 1_000, now);
    let mut second = UsageStats {
        local_device_id: "second-device".into(),
        ..UsageStats::default()
    };
    second.record("openai", 7, 2_000, now);

    let merged = UsageStats::merge_synced_values(&first.synced_value(), &second.synced_value());
    let mut received = UsageStats::default();
    assert!(received.merge_synced_value(&merged));
    assert_eq!(received.total_words, 12);
    assert_eq!(received.total_dictations, 2);
    assert!(!received.merge_synced_value(&merged));
    assert_eq!(received.total_words, 12);
}

#[test]
fn reset_is_durable_and_rejects_a_stale_synced_generation() {
    let path = temp_stats_path("stats-reset");
    let store = Arc::new(StatsStore::load_from(path.clone()));
    store.record_dictation("local", 9, 3_000);
    store.flush();
    let stale = store.snapshot().synced_value();

    store.reset();
    store.flush();
    assert_eq!(store.snapshot().total_dictations, 0);
    assert!(!store.apply_synced(&stale));
    assert_eq!(store.snapshot().total_dictations, 0);
    assert_eq!(
        StatsStore::load_from(path.clone())
            .snapshot()
            .total_dictations,
        0
    );

    let _ = fs::remove_file(path);
}

#[test]
fn shutdown_barrier_waits_for_the_session_guard() {
    let path = temp_stats_path("stats-session-barrier");
    let store = Arc::new(StatsStore::load_from(path.clone()));
    let guard = store.session_guard();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let barrier_store = Arc::clone(&store);
    let join = std::thread::spawn(move || {
        entered_tx.send(()).unwrap();
        barrier_store.finish_sessions_and_flush();
        done_tx.send(()).unwrap();
    });

    entered_rx.recv().unwrap();
    assert!(done_rx.try_recv().is_err());
    drop(guard);
    done_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("barrier should finish after the session guard drops");
    join.join().unwrap();

    let _ = fs::remove_file(path);
}

// The eviction logic used to live in `UsageStats::normalize` and these
// tests drove it through `.normalize()`. It now lives in
// `SyncedUsageStats::prune` (called after devices from both sides of a
// merge are unioned, never on a lone local snapshot), so the tests
// below exercise `prune` directly.

#[test]
fn stale_device_is_evicted_after_the_cutoff() {
    let now = 2_000_000_000u64;
    let mut synced = SyncedUsageStats::default();
    synced.devices.insert(
        "current".into(),
        DeviceStats {
            last_dictation_unix_secs: now,
            ..Default::default()
        },
    );
    synced.devices.insert(
        "retired-laptop".into(),
        DeviceStats {
            tracking_started_unix_secs: now - STALE_DEVICE_SECS - HOUR_SECS,
            last_dictation_unix_secs: now - STALE_DEVICE_SECS - HOUR_SECS,
            totals: PeriodStats {
                words: 30,
                audio_ms: 9_000,
                dictations: 3,
                ..Default::default()
            },
            hours: BTreeMap::new(),
        },
    );

    synced.prune(now);

    assert!(!synced.devices.contains_key("retired-laptop"));
    assert!(synced.devices.contains_key("current"));
}

#[test]
fn evicted_device_totals_are_folded_into_the_archived_bucket() {
    let now = 2_000_000_000u64;
    let mut synced = SyncedUsageStats::default();
    synced.devices.insert(
        "current".into(),
        DeviceStats {
            last_dictation_unix_secs: now,
            ..Default::default()
        },
    );
    synced.devices.insert(
        "retired-laptop".into(),
        DeviceStats {
            tracking_started_unix_secs: now - STALE_DEVICE_SECS - HOUR_SECS,
            last_dictation_unix_secs: now - STALE_DEVICE_SECS - HOUR_SECS,
            totals: PeriodStats {
                words: 30,
                audio_ms: 9_000,
                dictations: 3,
                ..Default::default()
            },
            hours: BTreeMap::new(),
        },
    );

    synced.prune(now);

    let archived = synced
        .devices
        .get(ARCHIVED_DEVICE_ID)
        .expect("evicted totals should be folded into the archived bucket");
    assert_eq!(archived.totals.words, 30);
    assert_eq!(archived.totals.audio_ms, 9_000);
    assert_eq!(archived.totals.dictations, 3);
    assert!(synced.archived_device_ids.contains("retired-laptop"));
}

#[test]
fn a_device_with_a_recent_dictation_is_never_evicted_even_if_every_other_device_is_stale() {
    // There is no longer a "current device" exemption inside `prune` —
    // it has no notion of which device is local (that concept only
    // exists on `UsageStats`, and both sides of a merge must prune
    // identically). A device survives purely because its own timestamp
    // is recent, which is what actually protects the device performing
    // a sync in practice: recording a dictation refreshes its
    // `last_dictation_unix_secs` to "now" before any merge runs.
    let now = 2_000_000_000u64;
    let mut synced = SyncedUsageStats::default();
    synced.devices.insert(
        "active".into(),
        DeviceStats {
            tracking_started_unix_secs: now - STALE_DEVICE_SECS - HOUR_SECS,
            last_dictation_unix_secs: now,
            totals: PeriodStats {
                words: 4,
                audio_ms: 500,
                dictations: 1,
                ..Default::default()
            },
            hours: BTreeMap::new(),
        },
    );

    synced.prune(now);

    assert!(synced.devices.contains_key("active"));
    assert_eq!(synced.devices["active"].totals.words, 4);
}

#[test]
fn pruning_a_synced_document_twice_is_a_no_op() {
    let now = 2_000_000_000u64;
    let mut synced = SyncedUsageStats::default();
    synced.devices.insert(
        "current".into(),
        DeviceStats {
            last_dictation_unix_secs: now,
            ..Default::default()
        },
    );
    synced.devices.insert(
        "retired-laptop".into(),
        DeviceStats {
            tracking_started_unix_secs: now - STALE_DEVICE_SECS - HOUR_SECS,
            last_dictation_unix_secs: now - STALE_DEVICE_SECS - HOUR_SECS,
            totals: PeriodStats {
                words: 30,
                audio_ms: 9_000,
                dictations: 3,
                ..Default::default()
            },
            hours: BTreeMap::new(),
        },
    );

    synced.prune(now);
    let once = synced.clone();
    synced.prune(now);

    assert_eq!(
        synced, once,
        "pruning an already-pruned document must be a no-op"
    );
}

#[test]
fn a_reappearing_raw_row_for_an_already_archived_device_is_not_folded_again() {
    // Simulates the actual bug: a stale device's raw row keeps arriving
    // from a peer that never itself pruned it, on every sync. The first
    // prune folds it in; a later prune that sees the same raw row again
    // must drop it without summing it into the archive a second time.
    let now = 2_000_000_000u64;
    let stale_device = DeviceStats {
        tracking_started_unix_secs: now - STALE_DEVICE_SECS - HOUR_SECS,
        last_dictation_unix_secs: now - STALE_DEVICE_SECS - HOUR_SECS,
        totals: PeriodStats {
            words: 30,
            audio_ms: 9_000,
            dictations: 3,
            ..Default::default()
        },
        hours: BTreeMap::new(),
    };
    let mut synced = SyncedUsageStats::default();
    synced
        .devices
        .insert("retired-laptop".into(), stale_device.clone());
    synced.prune(now);
    assert_eq!(synced.devices[ARCHIVED_DEVICE_ID].totals.words, 30);

    // The same raw row reappears, as it would after unioning in a fresh
    // remote payload that carried it unpruned.
    synced.devices.insert("retired-laptop".into(), stale_device);
    synced.prune(now);

    assert!(!synced.devices.contains_key("retired-laptop"));
    assert_eq!(synced.devices[ARCHIVED_DEVICE_ID].totals.words, 30);
    assert_eq!(synced.devices[ARCHIVED_DEVICE_ID].totals.dictations, 3);
}
