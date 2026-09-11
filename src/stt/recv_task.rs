//! The session's inbound-event task.
//!
//! Normalizes provider events into the shared accumulators the finalize phase
//! reads back, and owns the two policies that decide what a commit becomes:
//! the phantom-finalization guard and the hybrid hold/live paste flow.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::keys::FailKind;
use crate::polish;
use crate::state::App;

use super::deliver_transcript;
use super::heuristics::{is_phantom_finalization, looks_like_short_answer, transcripts_equivalent};
use super::provider::{ProviderStream, RecvError, SttEvent};

/// The accumulators [`run_recv_task`] fills in and the finalize phase reads
/// back through [`SessionFinalizeCtx`](super::finalize::SessionFinalizeCtx) once the send task's release phase
/// finishes. Created once, cheaply cloned (every field is an `Arc`) into the
/// recv task's own [`RecvTaskState`], so a lost/aborted recv task still
/// leaves whatever it managed to process behind in the originals.
#[derive(Clone)]
pub(super) struct SessionAccumulators {
    pub(super) chunks_buf: Arc<parking_lot::Mutex<Vec<String>>>,
    pub(super) last_partial_buf: Arc<parking_lot::Mutex<String>>,
    pub(super) dropped_phantom_buf: Arc<parking_lot::Mutex<Option<String>>>,
    pub(super) committed_flag: Arc<AtomicBool>,
    /// Text of the most recent KEPT commit, so the end-of-session fallback
    /// can tell a genuinely-unfinalized trailing partial from a partial
    /// that merely repeats what was already committed.
    pub(super) last_commit_text: Arc<parking_lot::Mutex<String>>,
    pub(super) transcribed_words: Arc<AtomicU64>,
    pub(super) key_fail_kind: Arc<parking_lot::Mutex<Option<FailKind>>>,
    pub(super) provider_failure: Arc<parking_lot::Mutex<Option<String>>>,
}

impl SessionAccumulators {
    pub(super) fn new() -> Self {
        Self {
            chunks_buf: Arc::new(parking_lot::Mutex::new(Vec::new())),
            last_partial_buf: Arc::new(parking_lot::Mutex::new(String::new())),
            dropped_phantom_buf: Arc::new(parking_lot::Mutex::new(None)),
            committed_flag: Arc::new(AtomicBool::new(false)),
            last_commit_text: Arc::new(parking_lot::Mutex::new(String::new())),
            transcribed_words: Arc::new(AtomicU64::new(0)),
            key_fail_kind: Arc::new(parking_lot::Mutex::new(None)),
            provider_failure: Arc::new(parking_lot::Mutex::new(None)),
        }
    }
}

/// State [`run_recv_task`] owns for a session's inbound events: the event
/// stream itself, the per-session flags/settings the Committed handler
/// branches on, and the shared accumulators it fills in. Bundled into one
/// struct for the same reason [`SendTaskState`](super::send_task::SendTaskState) is -- the task's match arms
/// (Committed especially) close over most of it.
pub(super) struct RecvTaskState {
    pub(super) stream: Box<dyn ProviderStream>,
    pub(super) recv_app: Arc<App>,
    pub(super) epoch: u64,
    pub(super) provider_id: &'static str,
    pub(super) log_transcripts: bool,
    pub(super) delay_until_release: bool,
    pub(super) suppress_phantom: bool,
    pub(super) polish_settings: Option<polish::PolishSettings>,
    pub(super) speculate_polish: bool,
    pub(super) release_pending: Arc<AtomicBool>,
    pub(super) speech_shipped: Arc<AtomicU64>,
    pub(super) acc: SessionAccumulators,
    /// Words an earlier attempt of this press already earned (carried held
    /// text), so the pip's count starts there instead of at zero.
    pub(super) seeded_words: u32,
    /// Bumped on every partial and commit, for the send task's stall watchdog.
    pub(super) server_activity: Arc<AtomicU64>,
    /// Bumped on every KEPT commit, so the send task's replay buffer can
    /// start over at each durable segment boundary.
    pub(super) commits_seen: Arc<AtomicU64>,
    /// Raised when the inbound half ends before release, so the send task
    /// reconnects at once instead of waiting out the stall timer.
    pub(super) stream_dead: Arc<AtomicBool>,
    /// Replacement inbound halves from the send task's stall recovery. The
    /// sender lives in the send task, so the channel closes when it is done
    /// and no replacement can come any more.
    pub(super) stream_swap: tokio::sync::mpsc::Receiver<Box<dyn ProviderStream>>,
}

/// What the recv loop's select settled on: a replacement stream (or the news
/// that none can come), or the next event off the current one. Split out so
/// the stream can be swapped after the select's borrows are released.
enum Next {
    Swap(Option<Box<dyn ProviderStream>>),
    Event(Result<Option<SttEvent>, RecvError>),
}

/// The session's inbound-event task: normalize provider events into
/// [`RecvTaskState::acc`]. Runs on its own `tokio::spawn` from
/// [`run_session`](super::run_session), mirroring [`run_send_task`](super::send_task::run_send_task) on the outbound side.
///
/// If the stream ends before release while a replacement could still arrive
/// (the send task's stall recovery), the task PARKS on the swap channel
/// instead of finishing, and carries on with the replacement when it comes.
pub(super) async fn run_recv_task(mut state: RecvTaskState) {
    let epoch = state.epoch;
    let provider_id = state.provider_id;
    let mut events: usize = 0;
    let mut committed_words: u32 = state.seeded_words;
    // Snapshot of `speech_shipped` taken at the last *kept* commit. Compared
    // against the live count at each new commit to spot a phantom (equal =>
    // no speech shipped in between). Starts at 0, so the very first real
    // commit -- always backed by shipped speech -- is never mistaken for one.
    let mut last_commit_speech: u64 = 0;
    // Whether a replacement stream can still arrive.
    let mut swap_open = true;
    // The current stream ended early and we are waiting for a replacement.
    let mut parked = false;
    loop {
        let next = tokio::select! {
            biased;
            swapped = state.stream_swap.recv(), if swap_open => Next::Swap(swapped),
            r = state.stream.recv_event(), if !parked => Next::Event(r),
            else => break,
        };
        let received = match next {
            Next::Swap(Some(stream)) => {
                state.stream = stream;
                parked = false;
                // Whatever the quiet connection last said about the open
                // segment is superseded: the replacement transcribes the
                // replayed audio afresh, so the pip falls back to the
                // committed count until its partials arrive.
                state.acc.last_partial_buf.lock().clear();
                state
                    .recv_app
                    .word_count
                    .store(committed_words, Ordering::Release);
                if state.acc.provider_failure.lock().take().is_some() {
                    tracing::info!(
                        "session[{epoch}] clearing the transport failure: the press carries on over a replacement connection"
                    );
                }
                tracing::info!(
                    "session[{epoch}] {provider_id} listening on the replacement connection"
                );
                continue;
            }
            Next::Swap(None) => {
                swap_open = false;
                if parked {
                    break;
                }
                continue;
            }
            Next::Event(r) => r,
        };
        // Whether an early end of this stream should be waited out rather
        // than ending the task: only before release, and only while the send
        // task can still produce a replacement.
        let can_park = swap_open && !state.release_pending.load(Ordering::Acquire);
        let ev = match received {
            Ok(Some(ev)) => ev,
            Ok(None) => {
                if can_park {
                    tracing::warn!(
                        "session[{epoch}] inbound half ended before release; waiting for a replacement connection"
                    );
                    state.stream_dead.store(true, Ordering::Release);
                    parked = true;
                    continue;
                }
                break;
            }
            Err(e) => {
                // A read error mid-utterance is NOT a clean end of stream.
                // Recording it in `provider_failure` is what makes
                // run_session return Err, so the retry shell can rotate or
                // the pip can show an error. Without this a dropped socket
                // was indistinguishable from the provider finishing
                // normally: no retry, no error, and any uncommitted speech
                // silently gone while the app reported success.
                //
                // Recorded unconditionally here, but only SURFACED at the
                // end of run_session when the session delivered no words.
                // ElevenLabs routinely resets the socket without a closing
                // handshake once it has sent the final transcript, and
                // erroring on that flashed the pip after a dictation the
                // user watched succeed. A successful replacement clears it.
                tracing::warn!("session[{epoch}] recv error: {e}");
                let mut slot = state.acc.provider_failure.lock();
                if slot.is_none() {
                    *slot = Some(format!("transport failed mid-session: {e}"));
                }
                drop(slot);
                if can_park {
                    tracing::warn!(
                        "session[{epoch}] transport failed before release; waiting for a replacement connection"
                    );
                    state.stream_dead.store(true, Ordering::Release);
                    parked = true;
                    continue;
                }
                break;
            }
        };
        events += 1;
        match ev {
            SttEvent::SessionStarted => {
                tracing::info!("session[{epoch}] {provider_id} session_started");
            }
            SttEvent::Partial(t) => {
                state.server_activity.fetch_add(1, Ordering::AcqRel);
                if state.log_transcripts {
                    tracing::debug!("session[{epoch}] partial: {t}");
                } else {
                    tracing::debug!("session[{epoch}] partial: {} char(s)", t.chars().count());
                }
                let partial_words = t.split_whitespace().count() as u32;
                state
                    .recv_app
                    .word_count
                    .store(committed_words + partial_words, Ordering::Release);
                *state.acc.last_partial_buf.lock() = t;
            }
            SttEvent::Committed(final_text) => {
                state.server_activity.fetch_add(1, Ordering::AcqRel);
                handle_committed_event(
                    &mut state,
                    &mut committed_words,
                    &mut last_commit_speech,
                    final_text,
                );
            }
            SttEvent::KeyFailure(kind) => {
                tracing::warn!("session[{epoch}] provider signaled key failure ({kind:?})");
                *state.acc.key_fail_kind.lock() = Some(kind);
                // Don't break: the outer wait loop observes key_fail_kind and
                // tears the session down / rotates keys.
            }
            SttEvent::ProviderFailure(message) => {
                tracing::error!("session[{epoch}] {provider_id} failed: {message}");
                *state.acc.provider_failure.lock() = Some(message);
            }
            SttEvent::Closed(reason) => {
                match reason {
                    Some(r) => {
                        tracing::warn!("session[{epoch}] transport closed by server ({r})")
                    }
                    None => tracing::info!("session[{epoch}] transport closed by server"),
                }
                if can_park {
                    tracing::warn!(
                        "session[{epoch}] server closed the session before release; waiting for a replacement connection"
                    );
                    state.stream_dead.store(true, Ordering::Release);
                    parked = true;
                    continue;
                }
                break;
            }
        }
    }
    tracing::info!("session[{epoch}] recv_task ended (events={events})");
}

/// Handle one `SttEvent::Committed`: the phantom-finalization guard, the
/// hybrid hold/live paste policy, and the speculative-polish kick-off. Split
/// out of [`run_recv_task`]'s match on its own -- of the six event arms,
/// this is the one with real nesting (phantom guard -> hybrid paste ->
/// speculation).
fn handle_committed_event(
    state: &mut RecvTaskState,
    committed_words: &mut u32,
    last_commit_speech: &mut u64,
    final_text: String,
) {
    let epoch = state.epoch;
    // Drop the chunk entirely only if the user threw this dictation away. A
    // commit that lands while the NEXT press is already live is still this
    // press's words, and in held mode it is swept up at finalize.
    if state.recv_app.session_discarded(epoch) {
        tracing::debug!("session[{epoch}] dropping late commit (dictation discarded)");
        return;
    }

    let released = state.release_pending.load(Ordering::Acquire);
    let speech_now = state.speech_shipped.load(Ordering::Acquire);

    // Phantom-finalization guard (ElevenLabs Scribe). A commit
    // that lands AFTER release with no speech-bearing audio shipped
    // since the previous commit -- AND whose text is a short
    // answer-shaped interjection -- is the model's LM prior
    // "answering" the question out of dead air ("Yes.", "No."),
    // not anything the user said. A genuinely-spoken trailing word
    // ships speech first, bumping `speech_now`, so it survives;
    // pre-release VAD commits have `released == false` and survive
    // too. The short-text gate bounds a residual race: `speech_now`
    // counts chunks shipped, not chunks attributable to *this*
    // commit, so a slow VAD commit that delivers a REAL segment
    // post-release (after the counter already advanced past it)
    // could look phantom -- but we then only ever risk dropping a
    // plausible answer, never a full sentence. See
    // `is_phantom_finalization`, `looks_like_short_answer`, and
    // the phantom-finalization regression tests below.
    if state.suppress_phantom
        && is_phantom_finalization(released, speech_now, *last_commit_speech)
        && looks_like_short_answer(&final_text)
    {
        *state.acc.dropped_phantom_buf.lock() = Some(final_text.clone());
        let mut partial = state.acc.last_partial_buf.lock();
        if transcripts_equivalent(&partial, &final_text) {
            partial.clear();
        }
        if state.log_transcripts {
            tracing::info!(
                "session[{epoch}] dropped phantom finalization (no speech since last commit): {final_text}"
            );
        } else {
            tracing::info!(
                "session[{epoch}] dropped phantom finalization (no speech since last commit): {} char(s)",
                final_text.chars().count()
            );
        }
        return;
    }

    // A transcript we're keeping. Mark that we have durable
    // committed text (disarms the last-partial fallback) and
    // advance the speech baseline for the next phantom check.
    // Set ONLY for kept commits: a dropped phantom must not trip
    // this, or a session whose only real content arrived as a
    // partial would lose its promotion fallback.
    state.acc.committed_flag.store(true, Ordering::Release);
    *last_commit_speech = speech_now;
    state.commits_seen.fetch_add(1, Ordering::AcqRel);

    // This commit supersedes every partial up to this point, so
    // clear the buffer. What lands in it AFTER this is speech
    // from a LATER segment, and that segment deserves the
    // last-partial fallback even though an earlier commit
    // already succeeded. The old session-wide `!got_committed`
    // gate disabled the fallback for the rest of the session
    // after the first commit, so a final segment whose
    // finalization timed out was discarded outright.
    state.acc.last_partial_buf.lock().clear();
    *state.acc.last_commit_text.lock() = final_text.clone();

    let chunk_words = final_text.split_whitespace().count() as u32;
    *committed_words = committed_words.saturating_add(chunk_words);
    state
        .acc
        .transcribed_words
        .fetch_add(chunk_words as u64, Ordering::AcqRel);
    state
        .recv_app
        .word_count
        .store(*committed_words, Ordering::Release);

    // Hybrid paste flow:
    //   before release              -> HOLD (accumulate)
    //   after release               -> LIVE (paste each chunk)
    //   delay_until_release = false -> LIVE throughout
    if state.delay_until_release && !released {
        if state.log_transcripts {
            tracing::info!("session[{epoch}] committed (held until release): {final_text}");
        } else {
            tracing::info!(
                "session[{epoch}] committed (held until release): {} char(s)",
                final_text.chars().count()
            );
        }
        let prefix = {
            let mut held = state.acc.chunks_buf.lock();
            held.push(final_text);
            // Same join the release flush will do, so a hit is
            // an exact-text hit rather than a near miss.
            state.speculate_polish.then(|| held.join(" "))
        };
        // Free time: the user is still talking and none of
        // this is on screen yet, so run the cleanup pass over
        // everything committed so far. If they release while
        // it is still thinking, the deadline race takes over
        // and nothing here has cost them anything.
        if let Some(prefix) = prefix {
            if let Some(settings) = state.polish_settings.as_ref() {
                state.recv_app.polish.speculate(settings, &prefix);
            }
        }
    } else {
        if state.log_transcripts {
            tracing::info!("session[{epoch}] committed (live, append): {final_text}");
        } else {
            tracing::info!(
                "session[{epoch}] committed (live, append): {} char(s)",
                final_text.chars().count()
            );
        }
        deliver_transcript(&state.recv_app.transcript_tx, final_text);
    }
}
