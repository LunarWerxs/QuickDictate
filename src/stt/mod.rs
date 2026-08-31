//! Provider-agnostic speech-to-text session runner.
//!
//! This is the original `stt.rs` machinery with every ElevenLabs-specific bit
//! moved behind [`provider::SttProvider`]. What stays here is generic: the
//! retry shell (key rotation, rounds), the 4-phase send loop (live → dynamic
//! tail → drain → commit/close), the hybrid paste policy, live word-count
//! updates, epoch bookkeeping, and timeout/deadline handling. Each provider is
//! a small adapter in its own file.

mod assemblyai;
mod dashscope;
mod deepgram;
mod elevenlabs;
mod google;
mod local;
mod openai;
pub mod provider;

mod connect;
mod dispatch;
mod finalize;
mod heuristics;
#[cfg(test)]
mod live_test;
#[cfg(test)]
mod mock;
mod recv_task;
mod send_task;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::keys::{FailKind, KeyPool};
use crate::polish;
use crate::state::{App, Status};

use connect::{audio_capture_unhealthy, establish_connected_session, ConnectedSession};
use finalize::{
    abort_for_early_key_failure, enter_release_phase, finish_session_outcome, join_recv_task,
    join_send_task, promote_tail_transcript, SessionFinalizeCtx,
};
use recv_task::{run_recv_task, RecvTaskState, SessionAccumulators};
use send_task::{run_send_task, SendTaskState};

pub use dispatch::{spawn_key_test, spawn_prewarm};

const TAIL_MIN: Duration = Duration::from_millis(250);

/// After TAIL_MIN, we keep listening as long as we hear audio above
/// SILENCE_RMS. The tail ends once we observe this much continuous silence --
/// the "keep listening after you stop" window, now user-configurable via
/// `Config::listen_tail_ms` (Settings → Dictation). Read per session below.
///
/// Head-room added on top of that quiet window to form the hard upper bound
/// on the dynamic tail, so a runaway session (background music, fan) can't
/// outlive the user. With the default 800 ms quiet window this reproduces the
/// historical 1800 ms cap.
const TAIL_MAX_HEADROOM: Duration = Duration::from_millis(1000);

/// i16 RMS threshold separating "speech" from "silence/ambient noise." Speech
/// at normal volume is well over 2000; high-gain mics idle as high as ~1100
/// from room hum. 1500 sits above that ambient floor.
const SILENCE_RMS: i32 = 1500;

/// During the post-release tail we hold back trailing silence (see
/// [`TailSilenceGate`](send_task::TailSilenceGate)), so on a long quiet tail no real audio frame goes out
/// for a while. If we stay silent this long, send a provider keepalive so an
/// idle server doesn't close the session mid-tail. Well under any realistic WS
/// idle timeout, and far longer than the default ~1.8 s tail, so for normal use
/// it never fires at all -- it only matters for deliberately long tails.
const TAIL_KEEPALIVE_AFTER: Duration = Duration::from_secs(5);

/// Keys we try per "round" of attempts before pausing to let a refresh land.
const MAX_KEY_ATTEMPTS: u32 = 3;

/// After a full round of MAX_KEY_ATTEMPTS bad keys, pause this long before
/// trying another round (only helps if a key on a short cooldown recovered).
const POOL_REFRESH_WAIT: Duration = Duration::from_secs(4);
const ERROR_PIP_VISIBLE: Duration = Duration::from_secs(2);
/// Hard cap on a single provider `connect()` during a real dictation session.
/// probe_key already bounds its prewarm connect at 6 s; the live path had no
/// timeout at all, so a stalled handshake (black-holed network, provider outage
/// mid-handshake) could hang the user's hotkey press until the OS TCP timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard cap on ONE `send_audio` call. Chunks are ~100 ms of audio, so a
/// healthy socket completes this in single-digit milliseconds; anything near
/// this bound means the transport is gone. Generous enough not to trip on a
/// brief stall, short enough that a dead network surfaces while the user is
/// still talking rather than after they release.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard cap on rounds (3 keys × 2 rounds = up to 6 attempts per press).
const MAX_RETRY_ROUNDS: u32 = 2;

/// Sentinel error asking the retry shell to pick a different key. Stays out of
/// band of normal errors so a real failure (network, mic) still bubbles up.
const EXHAUSTED_SIGNAL: &str = "__quickdictate_key_exhausted__";

pub struct SttHandle {
    pub stop: Arc<AtomicBool>,
    /// Set true when the session task exits (clean or errored). Main uses this
    /// to tell whether the active handle is still doing work.
    pub done: Arc<AtomicBool>,
}

impl SttHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

pub fn start_session(app: Arc<App>, keys: Arc<KeyPool>) -> SttHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let stop_ret = Arc::clone(&stop);
    let done_ret = Arc::clone(&done);
    let epoch = app.next_session_epoch();
    let app2 = Arc::clone(&app);
    let stats_session_guard = app.stats.session_guard();
    app.rt.spawn(async move {
        let _stats_session_guard = stats_session_guard;
        let session_usage = Arc::new(parking_lot::Mutex::new(SessionUsage::default()));
        // The retry-with-key-rotation shell and the post-session outcome
        // handling are both split into named async fns below, purely to
        // keep this spawned block's cognitive load down -- see their doc
        // comments. Behavior is unchanged.
        let final_res = run_session_with_retries(
            app2.clone(),
            Arc::clone(&keys),
            Arc::clone(&stop),
            epoch,
            Arc::clone(&session_usage),
        )
        .await;
        finish_session(app2, keys, epoch, session_usage, final_res).await;
        done.store(true, Ordering::Release);
    });
    SttHandle {
        stop: stop_ret,
        done: done_ret,
    }
}

/// Retry shell for one dictation session: a session may fail fast with
/// EXHAUSTED_SIGNAL, in which case we rotate to the next of the user's keys.
/// After a round of MAX_KEY_ATTEMPTS failures we pause briefly
/// (POOL_REFRESH_WAIT) in case a short cooldown lapses, then try another
/// round. Split out of `start_session`'s spawned block purely to keep its
/// cognitive load down.
async fn run_session_with_retries(
    app2: Arc<App>,
    keys: Arc<KeyPool>,
    stop: Arc<AtomicBool>,
    epoch: u64,
    session_usage: Arc<parking_lot::Mutex<SessionUsage>>,
) -> Result<()> {
    let mut final_res: Result<()> = Ok(());
    let mut attempts_in_round: u32 = 0;
    let mut rounds_done: u32 = 0;
    let mut total_attempts: u32 = 0;
    let user_aborted = || stop.load(Ordering::Acquire) || app2.current_session_epoch() != epoch;
    loop {
        if user_aborted() {
            break;
        }
        attempts_in_round += 1;
        total_attempts += 1;
        let attempt_res = run_session(
            app2.clone(),
            Arc::clone(&keys),
            Arc::clone(&stop),
            epoch,
            Arc::clone(&session_usage),
        )
        .await;
        let is_exhausted = matches!(&attempt_res, Err(e) if e.to_string() == EXHAUSTED_SIGNAL);
        if !is_exhausted {
            final_res = attempt_res;
            break;
        }
        if attempts_in_round < MAX_KEY_ATTEMPTS {
            tracing::warn!(
                "session[{epoch}] attempt {total_attempts} (round {round}, key {attempts_in_round}/{MAX_KEY_ATTEMPTS}) hit a bad key; rotating",
                round = rounds_done + 1
            );
            continue;
        }
        rounds_done += 1;
        if rounds_done >= MAX_RETRY_ROUNDS {
            tracing::error!(
                "session[{epoch}] {total_attempts} attempts across {MAX_RETRY_ROUNDS} rounds all failed; giving up"
            );
            final_res = attempt_res;
            break;
        }
        tracing::warn!(
            "session[{epoch}] round {rounds_done}/{MAX_RETRY_ROUNDS} exhausted; waiting up to {POOL_REFRESH_WAIT:?} for pool refresh"
        );
        let refreshed = keys.schedule_refresh_and_wait(POOL_REFRESH_WAIT).await;
        if user_aborted() {
            break;
        }
        tracing::info!(
            "session[{epoch}] refresh completed={refreshed}; starting round {round} of {MAX_RETRY_ROUNDS}",
            round = rounds_done + 1
        );
        attempts_in_round = 0;
    }
    final_res
}

/// Record usage, then report or clear the session's outcome. Split out of
/// `start_session`'s spawned block purely to keep its cognitive load down.
async fn finish_session(
    app2: Arc<App>,
    keys: Arc<KeyPool>,
    epoch: u64,
    session_usage: Arc<parking_lot::Mutex<SessionUsage>>,
    final_res: Result<()>,
) {
    let usage = session_usage.lock().clone();
    if usage.words > 0 {
        app2.stats
            .record_dictation(&usage.provider, usage.words, usage.audio_ms);
        crate::sync::schedule_stats_push(Arc::clone(&app2));
    }
    if let Err(e) = final_res {
        let key_shaped = e.to_string() == EXHAUSTED_SIGNAL;
        if key_shaped {
            tracing::error!(
                "session[{epoch}] tried {MAX_KEY_ATTEMPTS} keys, none worked -- check provider credit / pool health"
            );
        } else {
            tracing::error!("session error: {e:#}");
        }
        if app2.current_session_epoch() == epoch {
            // Name the actual cause, but only when the failure was actually
            // key-shaped. The pool's `last_failure` outlives the session
            // (prewarm marks quota-limited keys at startup), so consulting
            // it for a NON-key failure misattributes: a mic or network error
            // on a machine whose spare keys sat at Quota would show "out of
            // credit" for a dictation that never touched those keys.
            let kind = if key_shaped {
                error_kind_for(&keys)
            } else {
                crate::state::ErrorKind::Generic
            };
            app2.raise_error(kind);
            let app_for_clear = Arc::clone(&app2);
            app2.rt.spawn(async move {
                tokio::time::sleep(ERROR_PIP_VISIBLE).await;
                if app_for_clear.current_session_epoch() == epoch {
                    app_for_clear.clear_status_if(Status::Error, Status::Idle);
                }
            });
        }
    } else if app2.current_session_epoch() == epoch {
        app2.clear_status_if(Status::Processing, Status::Idle);
    }
}

/// Map what the key pool observed this run onto the pip/tooltip cause.
/// `all_dead` (every key rejected as invalid) stays the strongest signal;
/// below that the most recent [`FailKind`] is the honest answer.
fn error_kind_for(keys: &KeyPool) -> crate::state::ErrorKind {
    use crate::state::ErrorKind;
    if keys.all_dead() {
        return ErrorKind::DeadKeys;
    }
    match keys.last_failure() {
        Some(FailKind::Exhausted) => ErrorKind::Quota,
        Some(FailKind::RateLimit) => ErrorKind::RateLimited,
        Some(FailKind::Transient) => ErrorKind::Network,
        Some(FailKind::Invalid) => ErrorKind::DeadKeys,
        None => ErrorKind::Generic,
    }
}

/// Hand a finished transcript to the output thread.
///
/// The channel is bounded (64) and the plain `send()` this replaces BLOCKS
/// when it is full. run_session runs on one of only two tokio worker threads, so a
/// stalled paste (SendInput into a hung foreground window) could wedge both
/// workers and freeze every session and timer in the app. A bounded wait keeps
/// the transcript in the normal case and gives up loudly rather than deadlocking.
fn deliver_transcript(tx: &crossbeam_channel::Sender<String>, text: String) {
    const DELIVER_TIMEOUT: Duration = Duration::from_secs(30);
    let chars = text.chars().count();
    if let Err(e) = tx.send_timeout(text, DELIVER_TIMEOUT) {
        tracing::error!(
            "output queue did not accept a {chars}-char transcript within \
             {DELIVER_TIMEOUT:?} ({e}); the paste pipeline is stalled and this \
             transcript is lost"
        );
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SentAudio {
    chunks: usize,
    samples: u64,
    /// The send half watched the socket die (a failed `ship` or keepalive)
    /// before it was done. Chunk counts alone cannot tell "the user said
    /// nothing" apart from "we were cut off before they got a word in", and
    /// only the second one is worth an error pip. See
    /// [`transport_failure_lost_speech`](heuristics::transport_failure_lost_speech).
    socket_died: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SessionUsage {
    provider: String,
    words: u64,
    audio_ms: u64,
}

impl SessionUsage {
    fn add_fragment(&mut self, provider: &str, words: u64, audio_ms: u64) {
        if words == 0 {
            return;
        }
        if self.provider.is_empty() {
            self.provider = provider.to_string();
        } else if self.provider != provider {
            // A config edit during a retry is extraordinarily rare, but keep
            // the aggregate honest rather than attributing it to either side.
            self.provider = "mixed".to_string();
        }
        self.words = self.words.saturating_add(words);
        self.audio_ms = self.audio_ms.saturating_add(audio_ms);
    }
}

impl SentAudio {
    fn record_chunk(&mut self, chunk: &[i16]) {
        self.chunks = self.chunks.saturating_add(1);
        self.samples = self.samples.saturating_add(chunk.len() as u64);
    }

    fn record_prefix(&mut self, chunks: &[Vec<i16>], count: usize) {
        for chunk in chunks.iter().take(count) {
            self.record_chunk(chunk);
        }
    }
}

fn audio_duration_ms(samples: u64, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    samples.saturating_mul(1_000) / sample_rate as u64
}

async fn run_session(
    app: Arc<App>,
    keys: Arc<KeyPool>,
    stop: Arc<AtomicBool>,
    epoch: u64,
    session_usage: Arc<parking_lot::Mutex<SessionUsage>>,
) -> Result<()> {
    tracing::info!("session[{epoch}] starting");

    if audio_capture_unhealthy(&app, epoch) {
        return Ok(());
    }

    let connected = match establish_connected_session(&app, keys, &stop, epoch).await? {
        Some(connected) => connected,
        None => return Ok(()),
    };
    let ConnectedSession {
        cfg,
        keys,
        key,
        key_suffix,
        requires_api_key,
        finalize_timeout,
        final_transcript_timeout,
        suppress_phantom,
        provider_id,
        fmt,
        samples_rx,
        flusher,
        sink,
        stream,
    } = connected;

    if app.promote_starting_to_listening() {
        tracing::info!("session[{epoch}] visible state: Starting -> Listening");
    }
    crate::sound::play_start(cfg.enable_sound);

    // The "keep listening after you stop" window (Settings → Dictation). It
    // sets the dynamic-tail silence timeout; the hard cap is that plus a fixed
    // head-room. Read from the per-session config snapshot, so a Save applies
    // on the next utterance without a restart. Durations are `Copy`, so the
    // `move` send task below captures copies and we can still use them after.
    let tail_quiet = Duration::from_millis(cfg.listen_tail_ms);
    let tail_max = tail_quiet + TAIL_MAX_HEADROOM;

    // `release_pending` is set the moment the user lets go of the hotkey; the
    // send task uses it as the trigger to enter the dynamic-tail phase.
    let release_pending = Arc::new(AtomicBool::new(false));
    let flusher_send = flusher.clone();

    // Running count of speech-bearing chunks (RMS >= SILENCE_RMS) actually
    // shipped to the provider this session, for the phantom-finalization guard.
    // The send task bumps it per speech chunk across all phases; the recv task
    // snapshots it at each commit. A post-release commit whose snapshot equals
    // the previous commit's snapshot carried NO new speech -- that is Scribe
    // finalizing dead air into a hallucinated "answer" (see the Committed arm
    // and `is_phantom_finalization`). Counting only speech (not the inter-word
    // silence the live phase also ships) is what makes the equality meaningful.
    let speech_shipped = Arc::new(AtomicU64::new(0));
    let sent_progress = Arc::new(parking_lot::Mutex::new(SentAudio::default()));
    let send_task: tokio::task::JoinHandle<SentAudio> =
        tokio::spawn(run_send_task(SendTaskState {
            samples_rx,
            sink,
            flusher: Some(flusher_send),
            release_pending: Arc::clone(&release_pending),
            speech_shipped: Arc::clone(&speech_shipped),
            sent_progress: Arc::clone(&sent_progress),
            tail_quiet,
            tail_max,
        }));

    let recv_app = Arc::clone(&app);
    let delay_until_release = cfg.delay_output_till_release;
    // Default: never write recognized text to disk, even with `enable_logging`
    // on -- only a char-count/context summary. `log_transcripts` is a separate
    // opt-in for deep debugging that restores full-text logging at these
    // sites (and see `output.rs` for the paste-side log lines it also gates).
    let log_transcripts = cfg.log_transcripts;

    // Cleanup-pass settings for the SPECULATIVE passes below. Which app the
    // text lands in isn't known until paste time (the user may alt-tab
    // mid-dictation), so speculation uses the globals and `output.rs` makes
    // the authoritative per-app call. A speculated answer for an app that
    // turns the pass off is simply never collected.
    let polish_settings = cfg.polish_possible().then(|| polish::PolishSettings {
        endpoint: cfg.polish_endpoint.clone(),
        model: cfg.polish_model.clone(),
        // `polish_possible` already established this is non-empty.
        keys: cfg.polish_key_pool(),
        deadline: Duration::from_millis(cfg.polish_deadline_ms),
    });
    // Only worth speculating when commits actually pile up unpasted. With
    // `delay_output_till_release` off every commit is pasted the moment it
    // lands, so there is no held prefix to work ahead on.
    let speculate_polish = polish_settings.is_some() && delay_until_release;

    let acc = SessionAccumulators::new();
    let ctx = SessionFinalizeCtx {
        epoch,
        provider_id,
        keys,
        key,
        key_suffix,
        requires_api_key,
        delay_until_release,
        log_transcripts,
        enable_sound: cfg.enable_sound,
        fmt,
        sent_progress,
        acc: acc.clone(),
        session_usage,
    };

    // Reset the live word counter at the start of every session.
    app.word_count.store(0, Ordering::Release);
    // Drop any answer speculated for the PREVIOUS press. It is keyed by exact
    // text so it could not be misapplied anyway, but a new dictation should
    // not be racing against a stale in-flight request either.
    app.polish.reset();
    let recv_task = tokio::spawn(run_recv_task(RecvTaskState {
        stream,
        recv_app,
        epoch,
        provider_id,
        log_transcripts,
        delay_until_release,
        suppress_phantom,
        polish_settings,
        speculate_polish,
        release_pending: Arc::clone(&release_pending),
        speech_shipped: Arc::clone(&speech_shipped),
        acc,
    }));

    while !stop.load(Ordering::Acquire) {
        if app.current_session_epoch() != epoch {
            break;
        }
        // Break the moment we know the session is unusable so the retry shell
        // sees the failure without waiting for the user to press again.
        if ctx.acc.key_fail_kind.lock().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Fast-fail: if the provider already told us the key is dead, skip the
    // entire finalize and hand back to the retry shell to rotate keys.
    let early_key_failure = *ctx.acc.key_fail_kind.lock();
    if let Some(kind) = early_key_failure {
        return abort_for_early_key_failure(&ctx, kind, send_task, recv_task).await;
    }

    enter_release_phase(&app, &ctx, tail_quiet, tail_max, &release_pending);

    // Bound the wait so we never get stuck if something goes wrong. Streaming
    // providers finish within the tail window; batch providers (Google) need
    // longer for their in-`commit()` POST, hence the provider-supplied value.
    // Keep the budget above the (now user-configurable) tail as well, so a
    // long "keep listening" window can't get cut off before commit()/close()
    // and drop the final transcript. The provider's own timeout stays the
    // floor (Google's 45 s dwarfs any tail).
    let send_deadline = finalize_timeout.max(tail_max + Duration::from_millis(600));
    let sent = join_send_task(send_task, &ctx, send_deadline).await;
    let audio_ms = audio_duration_ms(sent.samples, fmt.sample_rate);
    // `speech` is the end-of-session gate's evidence that the user actually
    // said something (see `transport_failure_lost_speech`), so log it here
    // rather than only at the gate: a press that pips reveals in one line
    // whether the mic really heard speech or just crossed the floor on noise.
    let speech_chunks = speech_shipped.load(Ordering::Acquire);
    tracing::info!(
        "session[{epoch}] audio chunks sent = {} ({} speech-bearing, {} samples @ {} Hz, ~{} ms of audio)",
        sent.chunks,
        speech_chunks,
        sent.samples,
        fmt.sample_rate,
        audio_ms
    );

    // Wait for recv to drain. If it doesn't finish, cancel it before inspecting
    // the shared accumulators so it cannot emit a second, late final after we
    // promote the last partial. OpenAI uses a longer provider-specific grace
    // because its complete result can arrive several seconds after commit.
    join_recv_task(recv_task, &ctx, final_transcript_timeout).await;

    let got_committed = ctx.acc.committed_flag.load(Ordering::Acquire);
    let had_partial = promote_tail_transcript(&app, &ctx);
    if !got_committed && !had_partial && sent.chunks == 0 {
        tracing::warn!("session[{epoch}] produced no transcript (zero audio chunks sent -- session ended before mic was warm)");
    }

    finish_session_outcome(&ctx, audio_ms, speech_chunks, &sent)
}
