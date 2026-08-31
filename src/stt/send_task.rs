//! The session's outbound-audio task.
//!
//! Four phases in order: live, dynamic tail, drain, then commit + close. The
//! tail and drain phases run every chunk past [`TailSilenceGate`], so trailing
//! silence never reaches a model that would finalize it into a hallucination.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::heuristics::rms_i16;
use super::provider::ProviderSink;
use super::{SentAudio, SEND_TIMEOUT, SILENCE_RMS, TAIL_KEEPALIVE_AFTER, TAIL_MIN};

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
}

/// Phase 1 of [`run_send_task`]: forward mic audio to the provider as fast as
/// it arrives, until the hotkey is released or the socket dies.
async fn send_task_live_phase(state: &mut SendTaskState, sent: &mut SentAudio, ws_dead: &mut bool) {
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
            v = state.samples_rx.recv() => v,
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
