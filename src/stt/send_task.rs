//! The session's outbound-audio task.
//!
//! Four phases in order: live, dynamic tail, drain, then commit + close. The
//! tail and drain phases run every chunk past [`TailSilenceGate`], so trailing
//! silence never reaches a model that would finalize it into a hallucination.
//! The live phase also runs the stall watchdog ([`StallWatch`]): a server
//! that stops answering while speech is going out is replaced mid-press.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::connect::Reconnector;
use super::heuristics::{rms_i16, stall_tripped};
use super::provider::{ProviderSink, ProviderStream};
use super::{
    SentAudio, MAX_STALL_RECONNECTS, REPLAY_CAP_CHUNKS, REPLAY_CHUNK_PACE, SEND_TIMEOUT,
    SILENCE_RMS, TAIL_KEEPALIVE_AFTER, TAIL_MIN,
};

/// Send one PCM chunk through the provider sink. Mirrors the original `ship()`:
/// once a send errors the socket is dead, so we log only the first failure and
/// skip every subsequent send.
async fn ship(sink: &mut Box<dyn ProviderSink>, chunk: &[i16], dead: &mut bool) -> bool {
    if *dead {
        return false;
    }
    // Bounded: `connect()` had CONNECT_TIMEOUT and the post-release flush had
    // `send_deadline`, but the LIVE phase awaited send_audio with no limit at
    // all. A blackholed network while the user is holding the hotkey would
    // hang the whole session with no partials and no error until they let go.
    match tokio::time::timeout(SEND_TIMEOUT, sink.send_audio(chunk)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::debug!("provider send error (subsequent sends will be skipped): {e}");
            *dead = true;
            false
        }
        Err(_) => {
            tracing::warn!(
                "provider send stalled for {SEND_TIMEOUT:?}; treating the socket as dead"
            );
            *dead = true;
            false
        }
    }
}

/// Ship a batch of chunks in order, stopping early (and leaving `dead` set) if
/// the socket dies mid-batch. Returns how many were actually sent. Used by the
/// tail phases below, which forward held-back audio in a burst the moment
/// speech resumes (see [`TailSilenceGate`]).
async fn ship_all(sink: &mut Box<dyn ProviderSink>, chunks: &[Vec<i16>], dead: &mut bool) -> usize {
    let mut n = 0;
    for chunk in chunks {
        if !ship(sink, chunk, dead).await {
            break;
        }
        n += 1;
    }
    n
}

/// Trims the trailing run of silence from the audio forwarded to the provider
/// during the post-release tail.
///
/// A streaming STT model (notably ElevenLabs Scribe) will "finalize" a stretch
/// of dead room-tone into a hallucinated short answer -- ask a question, stop,
/// and it appends "Yes." -- because its language-model prior completes your
/// sentence out of the silence. QuickDictate then pastes that as if you'd said
/// it. The cure is to never send it the trailing silence in the first place.
///
/// Silent chunks are buffered rather than sent; the instant real speech resumes
/// the whole held run is flushed in order (so a genuine mid-utterance pause is
/// preserved verbatim and words after it still reach the provider), and only the
/// final silence that is *never* followed by more speech is dropped. This lets a
/// user keep an arbitrarily long "keep listening" tail without inviting
/// hallucinations -- we trim by content, not by clamping the tail's length.
#[derive(Default)]
pub(super) struct TailSilenceGate {
    /// Silent chunks captured since the last speech chunk, awaiting either a
    /// flush (speech resumed) or a discard (tail ended still-silent).
    pending: Vec<Vec<i16>>,
}

impl TailSilenceGate {
    /// Offer one captured chunk with the caller's speech/silence verdict (RMS
    /// vs the silence floor). Returns the chunks to forward to the provider
    /// *now*, in order: empty while we're inside a silent stretch, or the held
    /// pause followed by this chunk the moment speech resumes.
    pub(super) fn offer(&mut self, chunk: Vec<i16>, is_speech: bool) -> Vec<Vec<i16>> {
        if is_speech {
            let mut out = std::mem::take(&mut self.pending);
            out.push(chunk);
            out
        } else {
            self.pending.push(chunk);
            Vec::new()
        }
    }

    /// How many trailing silent chunks are currently held back (and, once the
    /// tail ends still-silent, discarded). For the log lines only.
    pub(super) fn held(&self) -> usize {
        self.pending.len()
    }
}

/// State [`run_send_task`] owns for a session's outbound audio: the
/// resampled-audio receiver, the provider sink, and the counters the recv
/// task and the end-of-session gate read back. Bundled into one struct
/// because the send task's phases (live / dynamic tail / drain) all close
/// over every field.
pub(super) struct SendTaskState {
    pub(super) samples_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
    pub(super) sink: Box<dyn ProviderSink>,
    /// Consumed (not just borrowed) by [`send_task_drain_phase`], so it's an
    /// `Option` rather than a plain field: `SessionFlusher::finish` takes
    /// `self` by value, which a field behind `&mut SendTaskState` can't hand
    /// out directly.
    pub(super) flusher: Option<crate::audio::SessionFlusher>,
    pub(super) release_pending: Arc<AtomicBool>,
    pub(super) speech_shipped: Arc<AtomicU64>,
    pub(super) sent_progress: Arc<parking_lot::Mutex<SentAudio>>,
    pub(super) tail_quiet: Duration,
    pub(super) tail_max: Duration,
    /// Bumped by the recv task on every partial and commit. The stall
    /// watchdog reads it: unchanged while speech goes out means the server
    /// side of the session is gone.
    pub(super) server_activity: Arc<AtomicU64>,
    /// Bumped by the recv task on every KEPT commit. A change means the
    /// replay buffer can start over: that segment is durable now.
    pub(super) commits_seen: Arc<AtomicU64>,
    /// Set by the recv task when the inbound half ended before release (the
    /// server closed or reset the socket mid-press). The watchdog trips on
    /// it at once rather than waiting out [`super::STALL_AFTER`].
    pub(super) stream_dead: Arc<AtomicBool>,
    /// How to recover from a stall, or `None` for providers that opt out
    /// (see [`super::provider::SttProvider::supports_stall_recovery`]).
    pub(super) recovery: Option<StallRecovery>,
}

/// Everything the send task needs to replace a stalled connection: the
/// reconnector, and the channel that hands the replacement's inbound half to
/// the recv task (which parks on it whenever its own stream dies early).
pub(super) struct StallRecovery {
    pub(super) reconnect: Reconnector,
    pub(super) stream_tx: tokio::sync::mpsc::Sender<Box<dyn ProviderStream>>,
    pub(super) epoch: u64,
}

/// The live phase's stall watchdog and replay buffer.
///
/// A streaming provider answers speech with partials every second or so.
/// This tracks how long the server has said nothing while speech-bearing
/// audio kept going out; past [`super::STALL_AFTER`] (with at least
/// [`super::STALL_MIN_SPEECH_CHUNKS`] of speech in that window) the session
/// is presumed dead on the server side, even though the socket still accepts
/// bytes -- which is exactly how ElevenLabs was caught failing: one commit,
/// then nothing for the rest of the press. Recovery opens a replacement
/// connection on the same key and replays the current segment (everything
/// shipped since the last kept commit) into it, so the words spoken into the
/// dead connection are transcribed after all. The segment is what gets
/// replayed, not just the last few seconds, because the stalled server's
/// partial for it is discarded: the replacement transcribes it whole.
pub(super) struct StallWatch {
    activity_seen: u64,
    commits_seen: u64,
    quiet_since: tokio::time::Instant,
    speech_since_activity: u64,
    reconnects: u32,
    segment: VecDeque<Vec<i16>>,
}

impl StallWatch {
    fn new(state: &SendTaskState) -> Self {
        Self {
            activity_seen: state.server_activity.load(Ordering::Acquire),
            commits_seen: state.commits_seen.load(Ordering::Acquire),
            quiet_since: tokio::time::Instant::now(),
            speech_since_activity: 0,
            reconnects: 0,
            segment: VecDeque::new(),
        }
    }

    /// Note one shipped live chunk. Returns `true` when the watchdog trips.
    fn observe(&mut self, state: &SendTaskState, chunk: Vec<i16>, is_speech: bool) -> bool {
        let activity = state.server_activity.load(Ordering::Acquire);
        if activity != self.activity_seen {
            self.activity_seen = activity;
            self.rearm();
        }
        let commits = state.commits_seen.load(Ordering::Acquire);
        if commits != self.commits_seen {
            self.commits_seen = commits;
            self.segment.clear();
        }
        self.segment.push_back(chunk);
        if self.segment.len() > REPLAY_CAP_CHUNKS {
            self.segment.pop_front();
        }
        if is_speech {
            self.speech_since_activity += 1;
        }
        if state.stream_dead.swap(false, Ordering::AcqRel) {
            // The recv task watched the socket end mid-press. No need to
            // wait for the timer to prove what is already known.
            return self.reconnects < MAX_STALL_RECONNECTS;
        }
        stall_tripped(
            self.quiet_since.elapsed(),
            self.speech_since_activity,
            self.reconnects,
        )
    }

    /// Start the quiet timer over (server activity seen, or a recovery just
    /// ran and must not immediately re-trip).
    fn rearm(&mut self) {
        self.quiet_since = tokio::time::Instant::now();
        self.speech_since_activity = 0;
    }
}

/// The watchdog tripped: open a replacement connection, hand its inbound half
/// to the recv task, switch the sink, and replay the current segment. Any
/// failure along the way leaves the session on the connection it has; the
/// end-of-session path then reports whatever that connection managed.
async fn recover_from_stall(state: &mut SendTaskState, watch: &mut StallWatch, ws_dead: &mut bool) {
    let Some(recovery) = state.recovery.as_ref() else {
        return;
    };
    let epoch = recovery.epoch;
    watch.reconnects += 1;
    tracing::warn!(
        "session[{epoch}] provider went quiet: no transcript for {:.1} s while {} speech chunk(s) went out; opening a replacement connection ({} of {MAX_STALL_RECONNECTS})",
        watch.quiet_since.elapsed().as_secs_f64(),
        watch.speech_since_activity,
        watch.reconnects
    );
    let replacement = match recovery.reconnect.connect().await {
        Ok(session) => session,
        Err(e) => {
            tracing::warn!(
                "session[{epoch}] replacement connection failed ({e}); staying on the quiet one"
            );
            watch.rearm();
            return;
        }
    };
    // Never block here: the recv task drains this one-slot channel the moment
    // anything lands in it, so a full or closed channel means that task is
    // stuck or gone, and a replacement it would never read is no use.
    if recovery.stream_tx.try_send(replacement.stream).is_err() {
        tracing::warn!(
            "session[{epoch}] recv task did not take the replacement; staying on the quiet connection"
        );
        watch.rearm();
        return;
    }
    state.sink = replacement.sink;
    let replay: Vec<Vec<i16>> = watch.segment.iter().cloned().collect();
    let samples: usize = replay.iter().map(Vec::len).sum();
    tracing::info!(
        "session[{epoch}] replacement connected; replaying the current segment ({} chunk(s), {samples} samples) into it",
        replay.len()
    );
    // Replayed audio is not new audio: `sent` and `speech_shipped` already
    // count it, and the recv task drops the stalled partial for it.
    let mut dead = false;
    for chunk in &replay {
        if !ship(&mut state.sink, chunk, &mut dead).await {
            break;
        }
        tokio::time::sleep(REPLAY_CHUNK_PACE).await;
    }
    if dead {
        tracing::warn!("session[{epoch}] the replacement connection died during the replay");
        *ws_dead = true;
    }
    watch.rearm();
}

/// Phase 1 of [`run_send_task`]: forward mic audio to the provider as fast as
/// it arrives, until the hotkey is released or the socket dies. Runs the
/// stall watchdog for providers that support recovery.
async fn send_task_live_phase(state: &mut SendTaskState, sent: &mut SentAudio, ws_dead: &mut bool) {
    let mut watch = state.recovery.is_some().then(|| StallWatch::new(state));
    loop {
        if state.release_pending.load(Ordering::Acquire) || *ws_dead {
            break;
        }
        let chunk_opt = tokio::select! {
            v = state.samples_rx.recv() => v,
            _ = tokio::time::sleep(Duration::from_millis(30)) => continue,
        };
        match chunk_opt {
            Some(chunk) => {
                // Classify before shipping so the phantom-finalization guard
                // (recv task) can tell a commit backed by real speech from one
                // conjured out of the trailing silence the live phase also
                // forwards. Only speech advances `speech_shipped`.
                let is_speech = rms_i16(&chunk) >= SILENCE_RMS;
                if !ship(&mut state.sink, &chunk, ws_dead).await {
                    break;
                }
                sent.record_chunk(&chunk);
                *state.sent_progress.lock() = *sent;
                if is_speech {
                    state.speech_shipped.fetch_add(1, Ordering::Release);
                }
                if let Some(watch) = watch.as_mut() {
                    if watch.observe(state, chunk, is_speech) {
                        recover_from_stall(state, watch, ws_dead).await;
                    }
                }
            }
            None => break,
        }
    }
}

/// Phase 2 of [`run_send_task`]: keep listening through the user-configured
/// tail, but do NOT forward its trailing silence to the provider -- a
/// streaming model would hallucinate a short answer out of that dead air
/// (see [`TailSilenceGate`]). The gate holds silent chunks back and flushes
/// them only when speech resumes, so a real mid-utterance pause is preserved
/// and only the final never-followed-by-speech silence is dropped.
/// Endpointing (peak_rms / last_speech / the quiet window) still sees every
/// chunk; the gate only decides what actually goes on the wire.
async fn send_task_tail_phase(
    state: &mut SendTaskState,
    sent: &mut SentAudio,
    ws_dead: &mut bool,
) -> TailSilenceGate {
    let mut gate = TailSilenceGate::default();
    let tail_start = tokio::time::Instant::now();
    let mut last_speech = tail_start;
    // Last time a real audio frame (or a keepalive) actually went out. While
    // we're trimming a long silent stretch nothing ships, so this drives the
    // keepalive that stops an idle server from closing the session mid-tail.
    let mut last_send = tail_start;
    let mut tail_chunks: usize = 0;
    let mut peak_rms: i32 = 0;
    while !*ws_dead {
        let elapsed = tail_start.elapsed();
        if elapsed >= state.tail_max {
            tracing::info!(
                "session tail: hit tail_max ({:.0} ms) after {:.0} ms (peak_rms={peak_rms}, {} silent chunk(s) trimmed)",
                state.tail_max.as_secs_f64() * 1000.0,
                elapsed.as_secs_f64() * 1000.0,
                gate.held(),
            );
            break;
        }
        let chunk_opt = tokio::select! {
            v = state.samples_rx.recv() => match v {
                Some(chunk) => Some(chunk),
                // The capture side is gone (device lost mid-tail). Nothing
                // more can arrive, and a closed receiver answers instantly,
                // so waiting out the tail here would be a hot spin.
                None => {
                    tracing::warn!("session tail: audio source closed; ending the tail early");
                    break;
                }
            },
            _ = tokio::time::sleep(Duration::from_millis(20)) => None,
        };
        if let Some(chunk) = chunk_opt {
            let rms = rms_i16(&chunk);
            if rms > peak_rms {
                peak_rms = rms;
            }
            let is_speech = rms >= SILENCE_RMS;
            if is_speech {
                last_speech = tokio::time::Instant::now();
            }
            // Ship speech now (flushing any held pause first); buffer silence.
            let outgoing = gate.offer(chunk, is_speech);
            let n = ship_all(&mut state.sink, &outgoing, ws_dead).await;
            sent.record_prefix(&outgoing, n);
            *state.sent_progress.lock() = *sent;
            tail_chunks += n;
            if n > 0 {
                last_send = tokio::time::Instant::now();
                // A speech-bearing tail chunk went out: a genuinely-spoken
                // trailing word. Count it so its commit isn't mistaken for a
                // phantom (this is what preserves a real trailing "Yes.").
                if is_speech {
                    state.speech_shipped.fetch_add(1, Ordering::Release);
                }
            }
            if *ws_dead {
                break;
            }
        }
        // Long quiet tail: no audio has gone out for a while (we're trimming
        // silence). Send a content-free keepalive so the server keeps the
        // session open. Never fires on a normal-length tail.
        if last_send.elapsed() >= TAIL_KEEPALIVE_AFTER {
            if let Err(e) = state.sink.keepalive().await {
                tracing::debug!("session tail: keepalive failed (socket likely dead): {e}");
                *ws_dead = true;
                break;
            }
            last_send = tokio::time::Instant::now();
            tracing::debug!("session tail: sent keepalive during long silent tail");
        }
        if elapsed >= TAIL_MIN && last_speech.elapsed() >= state.tail_quiet {
            tracing::info!(
                "session tail: ended after {:.0} ms ({} tail chunk(s) shipped, {} silent chunk(s) trimmed, peak_rms={peak_rms}, quiet ={:.0} ms)",
                elapsed.as_secs_f64() * 1000.0,
                tail_chunks,
                gate.held(),
                last_speech.elapsed().as_secs_f64() * 1000.0
            );
            break;
        }
    }
    gate
}

/// Phase 3 of [`run_send_task`]: flush the session's resampler tail, then
/// drain it -- same silence gate as the tail, so the flushed fragment and
/// any last mic chunks are forwarded only if they carry speech. Stops the
/// capture subscription first, atomically flushing its last resampler
/// fragment while `samples_rx` is still alive, then drains that fragment;
/// reversing these drops can clip it and log a false queue warning during
/// slow local inference.
async fn send_task_drain_phase(
    state: &mut SendTaskState,
    sent: &mut SentAudio,
    ws_dead: &mut bool,
    gate: &mut TailSilenceGate,
) {
    if let Some(flusher) = state.flusher.take() {
        flusher.finish();
    }
    let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while !*ws_dead {
        let chunk_opt = tokio::select! {
            v = state.samples_rx.recv() => v,
            _ = tokio::time::sleep_until(drain_deadline) => None,
        };
        match chunk_opt {
            Some(chunk) => {
                let is_speech = rms_i16(&chunk) >= SILENCE_RMS;
                let outgoing = gate.offer(chunk, is_speech);
                let n = ship_all(&mut state.sink, &outgoing, ws_dead).await;
                sent.record_prefix(&outgoing, n);
                *state.sent_progress.lock() = *sent;
                if is_speech && n > 0 {
                    state.speech_shipped.fetch_add(1, Ordering::Release);
                }
                if *ws_dead {
                    break;
                }
            }
            None => break,
        }
    }
    if gate.held() > 0 {
        tracing::debug!(
            "session tail: dropped {} trailing silent chunk(s) before commit -- never sent, so the model can't finalize silence into a hallucinated answer",
            gate.held(),
        );
    }
}

/// The session's outbound-audio task: live phase, dynamic tail, drain, then
/// commit + close. Runs on its own `tokio::spawn` from [`run_session`](super::run_session).
pub(super) async fn run_send_task(mut state: SendTaskState) -> SentAudio {
    let mut sent = SentAudio::default();
    let mut ws_dead = false;

    send_task_live_phase(&mut state, &mut sent, &mut ws_dead).await;
    let mut gate = send_task_tail_phase(&mut state, &mut sent, &mut ws_dead).await;
    send_task_drain_phase(&mut state, &mut sent, &mut ws_dead, &mut gate).await;

    // Batch/local commit can spend seconds or minutes in inference. Stop
    // subscribing before awaiting it so the bounded audio queue does not
    // fill with frames nobody will ever consume.
    drop(state.samples_rx);

    // Commit + close (only if the socket is still alive).
    if !ws_dead {
        let _ = state.sink.commit().await;
        let _ = state.sink.close().await;
    }
    // Carry the socket's fate back with the byte counts: the end-of-session
    // gate needs it to tell an empty press from one that was cut off.
    sent.socket_died = ws_dead;
    *state.sent_progress.lock() = sent;
    sent
}
