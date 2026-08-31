//! Tests for session feeding, the spare-chunk pool, and the resampler.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::*;

fn test_entry(id: u64, tx: mpsc::Sender<Vec<i16>>) -> SessionEntry {
    SessionEntry {
        id,
        tx,
        inner: Mutex::new(SessionResampler {
            resampler: LinearResampler::new(1.0, 1),
            pending: Vec::new(),
            target_rate: 16_000,
            built_rate: 16_000,
            built_channels: 1,
            spare_chunks: Vec::new(),
        }),
        queue_full_reported: AtomicBool::new(false),
    }
}

/// Device-format atomics matching what `test_entry` was built for, so a
/// plain `feed_sessions` call in a test does not itself trigger a rebuild.
fn native_format() -> (AtomicU32, AtomicUsize) {
    (AtomicU32::new(16_000), AtomicUsize::new(1))
}

#[test]
fn feed_sessions_processes_each_input_buffer_once() {
    let (tx, mut rx) = mpsc::channel(4);
    let sessions = parking_lot::RwLock::new(vec![test_entry(1, tx)]);
    let (device_rate, channels) = native_format();
    let input = vec![123_i16; CHUNK_SAMPLES + 1];

    feed_sessions(&sessions, &device_rate, &channels, &input);

    let chunk = rx.try_recv().expect("one complete audio chunk");
    assert_eq!(chunk.len(), CHUNK_SAMPLES);
    assert!(rx.try_recv().is_err(), "input buffer was duplicated");
    assert_eq!(sessions.read()[0].inner.lock().pending.len(), 1);
}

#[test]
fn feed_sessions_rebuilds_resampler_on_device_format_change() {
    // Session subscribed at 16 kHz target; device starts at 16 kHz mono,
    // so step 1.0 is a passthrough. Simulate a reopen at double the rate
    // mid-stream, as `run_global_capture` does after an unplug, mic swap,
    // or sleep resume, and confirm the session's resampler rebuilds for
    // the new step instead of continuing to pace input as if the device
    // were still at the old rate.
    let (tx, mut rx) = mpsc::channel(4);
    let sessions = parking_lot::RwLock::new(vec![test_entry(1, tx)]);
    let (device_rate, channels) = native_format();

    let warm = vec![1_i16; CHUNK_SAMPLES];
    feed_sessions(&sessions, &device_rate, &channels, &warm);
    let first = rx.try_recv().expect("chunk at the original rate");
    assert_eq!(first.len(), CHUNK_SAMPLES);

    // Reopen at double the rate; channel count is unchanged.
    device_rate.store(32_000, Ordering::Release);

    // At the new 32 kHz -> 16 kHz step (2.0), the same input length now
    // yields half as many output samples, so it alone cannot complete a
    // chunk unless the resampler picked up the new step.
    let after = vec![2_i16; CHUNK_SAMPLES];
    feed_sessions(&sessions, &device_rate, &channels, &after);
    assert!(
        rx.try_recv().is_err(),
        "resampler kept the stale step instead of rebuilding for the new rate"
    );
    assert_eq!(
        sessions.read()[0].inner.lock().pending.len(),
        CHUNK_SAMPLES / 2,
        "rebuilt resampler should downsample 2:1 at the new device rate"
    );
}

#[test]
fn feed_sessions_drain_chunking_preserves_exact_sample_order() {
    // Regression guard for the split_off -> drain change: feed input
    // across several callback-sized slices that do not line up with
    // CHUNK_SAMPLES boundaries, and confirm every emitted chunk plus the
    // final pending tail reconstructs the exact original sample
    // sequence, with no gaps, duplicates, or off-by-one shifts from
    // reusing pooled buffers.
    let (tx, mut rx) = mpsc::channel(32);
    let sessions = parking_lot::RwLock::new(vec![test_entry(1, tx)]);
    let (device_rate, channels) = native_format();

    let total = CHUNK_SAMPLES * 3 + 250;
    let all: Vec<i16> = (0..total as i32).map(|v| v as i16).collect();

    let mut received = Vec::new();
    // Odd-sized slices so chunk boundaries land mid-callback.
    for slice in all.chunks(777) {
        feed_sessions(&sessions, &device_rate, &channels, slice);
        while let Ok(chunk) = rx.try_recv() {
            received.extend(chunk);
        }
    }
    received.extend(sessions.read()[0].inner.lock().pending.clone());

    assert_eq!(received, all);
}

#[test]
fn session_flusher_targets_and_unregisters_by_stable_id() {
    let (tx1, mut rx1) = mpsc::channel(4);
    let (tx2, mut rx2) = mpsc::channel(4);
    let mut first = test_entry(11, tx1);
    first.inner.get_mut().pending.push(1);
    let mut second = test_entry(22, tx2);
    second.inner.get_mut().pending.push(2);
    let sessions = Arc::new(parking_lot::RwLock::new(vec![first, second]));
    let flusher = SessionFlusher {
        sessions: Arc::clone(&sessions),
        session_id: 22,
    };

    flusher.finish();
    assert!(rx1.try_recv().is_err());
    assert_eq!(rx2.try_recv().expect("target session tail"), vec![2]);

    let remaining: Vec<u64> = sessions.read().iter().map(|entry| entry.id).collect();
    assert_eq!(remaining, vec![11]);
}

#[test]
fn finish_flushes_before_unregistering_and_closing_the_receiver() {
    let (tx, mut rx) = mpsc::channel(4);
    let mut entry = test_entry(33, tx);
    entry.inner.get_mut().pending.extend([7, 8, 9]);
    let sessions = Arc::new(parking_lot::RwLock::new(vec![entry]));
    let flusher = SessionFlusher {
        sessions: Arc::clone(&sessions),
        session_id: 33,
    };

    flusher.finish();

    assert!(sessions.read().is_empty());
    assert_eq!(
        rx.try_recv().expect("final pending fragment"),
        vec![7, 8, 9]
    );
}

#[test]
fn the_microphone_preference_round_trips_and_trims() {
    // Empty means "follow the Windows default", which is the shipped
    // behaviour and must survive being set explicitly.
    set_preferred_input("  ");
    assert_eq!(PREFERRED_INPUT.load().as_str(), "");
    set_preferred_input("  Yeti  ");
    assert_eq!(PREFERRED_INPUT.load().as_str(), "Yeti");
    set_preferred_input("");
    assert_eq!(PREFERRED_INPUT.load().as_str(), "");
}
