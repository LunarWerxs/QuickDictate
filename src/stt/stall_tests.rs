//! The stall watchdog, end to end at the send-task level: a mock provider
//! that never answers, speech going out, and the reconnect + replay that
//! must follow. Runs on paused tokio time, so the 5 s watchdog costs nothing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::connect::Reconnector;
use super::heuristics::stall_tripped;
use super::mock::MockProvider;
use super::provider::{ProviderSession, SttProvider, SttSessionOpts};
use super::send_task::{run_send_task, SendTaskState, StallRecovery};
use super::{SentAudio, MAX_STALL_RECONNECTS, STALL_AFTER, STALL_MIN_SPEECH_CHUNKS};

#[test]
fn watchdog_needs_both_silence_from_the_server_and_speech_from_the_user() {
    let long = STALL_AFTER + Duration::from_millis(1);
    let short = STALL_AFTER - Duration::from_millis(1);
    // The observed failure: seconds of speech out, nothing back.
    assert!(stall_tripped(long, STALL_MIN_SPEECH_CHUNKS, 0));
    // A user who paused: the server is rightly quiet.
    assert!(!stall_tripped(long, 0, 0));
    assert!(!stall_tripped(long, STALL_MIN_SPEECH_CHUNKS - 1, 0));
    // A slow first partial is not a stall yet.
    assert!(!stall_tripped(short, 1_000, 0));
    // Out of reconnects for this press.
    assert!(!stall_tripped(long, 1_000, MAX_STALL_RECONNECTS));
}

fn opts() -> SttSessionOpts {
    SttSessionOpts {
        language: "en".into(),
        sample_rate: 16_000,
        model: None,
        custom_vocabulary: Vec::new(),
    }
}

struct Harness {
    provider: Arc<MockProvider>,
    samples_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    stream_rx: tokio::sync::mpsc::Receiver<Box<dyn super::provider::ProviderStream>>,
    release_pending: Arc<AtomicBool>,
    server_activity: Arc<AtomicU64>,
    stream_dead: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<SentAudio>,
}

/// A send task on a mock provider whose stream never says anything.
async fn spawn_send_task(with_recovery: bool) -> Harness {
    let provider = Arc::new(MockProvider::default());
    let ProviderSession { sink, stream: _ } = provider.connect("k", &opts()).await.unwrap();
    let (samples_tx, samples_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(64);
    let (stream_tx, stream_rx) = tokio::sync::mpsc::channel(1);
    let release_pending = Arc::new(AtomicBool::new(false));
    let server_activity = Arc::new(AtomicU64::new(0));
    let stream_dead = Arc::new(AtomicBool::new(false));
    let recovery = with_recovery.then(|| StallRecovery {
        reconnect: Reconnector::new(
            Arc::clone(&provider) as Arc<dyn SttProvider>,
            "k".into(),
            opts(),
        ),
        stream_tx,
        epoch: 7,
    });
    let state = SendTaskState {
        samples_rx,
        sink,
        flusher: None,
        release_pending: Arc::clone(&release_pending),
        speech_shipped: Arc::new(AtomicU64::new(0)),
        sent_progress: Arc::new(parking_lot::Mutex::new(SentAudio::default())),
        tail_quiet: Duration::from_millis(800),
        tail_max: Duration::from_millis(1800),
        server_activity: Arc::clone(&server_activity),
        commits_seen: Arc::new(AtomicU64::new(0)),
        stream_dead: Arc::clone(&stream_dead),
        recovery,
    };
    let task = tokio::spawn(run_send_task(state));
    Harness {
        provider,
        samples_tx,
        stream_rx,
        release_pending,
        server_activity,
        stream_dead,
        task,
    }
}

/// Feed `n` chunks of loud speech at the mic's real cadence (100 ms).
async fn speak(h: &Harness, n: usize) {
    let loud = vec![8_000i16; 1_600];
    for _ in 0..n {
        h.samples_tx.send(loud.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn finish(h: Harness) -> SentAudio {
    h.release_pending.store(true, Ordering::Release);
    // The mic stays subscribed through the tail and drain, as in the app;
    // the send task drops its own receiver before commit.
    let sent = h.task.await.unwrap();
    drop(h.samples_tx);
    sent
}

#[tokio::test(start_paused = true)]
async fn a_quiet_server_is_replaced_and_the_segment_replayed() {
    let mut h = spawn_send_task(true).await;
    // Six seconds of speech, nothing ever comes back.
    speak(&h, 60).await;

    assert_eq!(
        h.provider.connects.load(Ordering::Acquire),
        2,
        "one reconnect after the watchdog tripped"
    );
    assert!(
        h.stream_rx.try_recv().is_ok(),
        "the replacement's inbound half must reach the recv task"
    );
    let shipped = h.provider.sent_chunks.load(Ordering::Acquire);
    assert!(
        shipped > 60,
        "the segment must be replayed into the replacement (sent {shipped})"
    );

    let sent = finish(h).await;
    assert_eq!(
        sent.chunks, 60,
        "replayed audio is not counted as new audio"
    );
    assert!(!sent.socket_died);
}

#[tokio::test(start_paused = true)]
async fn a_server_that_keeps_answering_is_left_alone() {
    let h = spawn_send_task(true).await;
    // Ten seconds of speech with a partial every second.
    for _ in 0..10 {
        speak(&h, 10).await;
        h.server_activity.fetch_add(1, Ordering::AcqRel);
    }
    assert_eq!(h.provider.connects.load(Ordering::Acquire), 1);
    let sent = finish(h).await;
    assert_eq!(sent.chunks, 100);
}

#[tokio::test(start_paused = true)]
async fn a_dead_inbound_half_reconnects_at_once() {
    let mut h = spawn_send_task(true).await;
    speak(&h, 5).await;
    // The recv task saw the server close the socket mid-press.
    h.stream_dead.store(true, Ordering::Release);
    speak(&h, 2).await;
    assert_eq!(
        h.provider.connects.load(Ordering::Acquire),
        2,
        "no need to wait out the stall timer"
    );
    assert!(h.stream_rx.try_recv().is_ok());
    finish(h).await;
}

#[tokio::test(start_paused = true)]
async fn reconnects_are_capped_per_press() {
    let h = spawn_send_task(true).await;
    // Long enough for the watchdog to trip many times over if uncapped.
    speak(&h, 60 * 4).await;
    assert_eq!(
        h.provider.connects.load(Ordering::Acquire),
        1 + MAX_STALL_RECONNECTS as usize
    );
    finish(h).await;
}

/// The other half of "never lose the sentences before a failure": what an
/// aborted attempt hands to the next one.
#[test]
fn an_aborted_attempt_hands_over_held_commits_and_a_real_trailing_partial() {
    use super::finalize::stash_unpasted;
    use super::recv_task::SessionAccumulators;

    let acc = SessionAccumulators::new();
    acc.chunks_buf.lock().push("First sentence.".into());
    acc.chunks_buf.lock().push("Second one.".into());
    *acc.last_commit_text.lock() = "Second one.".into();
    *acc.last_partial_buf.lock() = "and a third that never".into();
    let mut carry = Vec::new();
    assert_eq!(stash_unpasted(&acc, &mut carry), 3);
    assert_eq!(
        carry,
        vec![
            "First sentence.".to_string(),
            "Second one.".into(),
            "and a third that never".into()
        ]
    );
    assert!(acc.chunks_buf.lock().is_empty(), "moved, not copied");
    assert!(acc.last_partial_buf.lock().is_empty());

    // A partial that merely repeats the last commit is not a fourth chunk.
    *acc.last_partial_buf.lock() = "second one".into();
    assert_eq!(stash_unpasted(&acc, &mut carry), 0);
    assert_eq!(carry.len(), 3);
}

#[tokio::test(start_paused = true)]
async fn providers_without_recovery_never_reconnect() {
    let h = spawn_send_task(false).await;
    speak(&h, 120).await;
    assert_eq!(h.provider.connects.load(Ordering::Acquire), 1);
    let sent = finish(h).await;
    assert_eq!(sent.chunks, 120);
}
