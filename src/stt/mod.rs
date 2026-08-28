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

#[cfg(test)]
mod live_test;
#[cfg(test)]
mod mock;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::time::Instant;

use crate::config::Config;
use crate::keys::{FailKind, KeyPool};
use crate::polish;
use crate::state::{App, Status};
use provider::{ProviderSession, ProviderSink, SttEvent, SttProvider, SttSessionOpts};

/// Minimum tail we always capture after hotkey release -- gives WASAPI's
/// ~10-20 ms hardware buffer and the resampler's pending fragment time to
/// reach us. Under this we don't even check audio energy.
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
/// [`TailSilenceGate`]), so on a long quiet tail no real audio frame goes out
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

/// Build the provider selected in settings.json. Unknown ids fall back to
/// ElevenLabs (the baseline) with a warning. Providers are cheap unit structs,
/// rebuilt per session so a settings edit + restart cleanly switches backend.
fn make_provider(cfg: &Config) -> Box<dyn SttProvider> {
    make_provider_id(&cfg.stt_provider, cfg)
}

/// Build a provider by EXPLICIT id, so a Per-App Profile can select one that
/// differs from `cfg.stt_provider` (see `Config::provider_for_exe`).
fn make_provider_id(id: &str, cfg: &Config) -> Box<dyn SttProvider> {
    match id.trim().to_ascii_lowercase().as_str() {
        "elevenlabs" => Box::new(elevenlabs::ElevenLabsProvider),
        "deepgram" => Box::new(deepgram::DeepgramProvider),
        "assemblyai" => Box::new(assemblyai::AssemblyAiProvider),
        "dashscope" => Box::new(dashscope::DashScopeProvider {
            intl: cfg.dashscope_intl,
        }),
        "openai" => Box::new(openai::OpenAiProvider),
        "google" => Box::new(google::GoogleProvider),
        "local" => Box::new(local::LocalProvider {
            model_id: cfg.local_model.clone(),
        }),
        other => {
            tracing::warn!("unknown stt_provider '{other}', falling back to elevenlabs");
            Box::new(elevenlabs::ElevenLabsProvider)
        }
    }
}

/// Startup key prewarm (§owner request, 2026-07-04): probe every key of the
/// active provider in config order, mark dead/limited ones failed (so the
/// session's `acquire` never wastes a press on them), and queue the first
/// validated key as ready-to-go. Runs in the background; dictation stays fully
/// usable while it works — a session started mid-probe just uses the pool as
/// probed so far.
pub fn spawn_prewarm(app: Arc<App>, keys: Arc<KeyPool>) {
    app.rt.clone().spawn(async move {
        let cfg = app.config.load_full();
        let provider = make_provider(&cfg);
        if !provider.requires_api_key() {
            return;
        }
        let provider_id = provider.id();
        let fmt = provider.required_audio_format();
        let opts = SttSessionOpts {
            language: provider.language_for(&cfg.language),
            sample_rate: fmt.sample_rate,
            model: cfg.stt_model.clone(),
            // A probe only proves the credential works; biasing terms would
            // just make the handshake bigger for no benefit.
            custom_vocabulary: Vec::new(),
        };
        let list = keys.all_keys();
        if list.is_empty() {
            return;
        }
        tracing::info!("prewarm: probing {} {provider_id} key(s)", list.len());
        for key in list {
            let verdict = probe_key(provider.as_ref(), &key, &opts).await;
            match verdict {
                Ok(()) => keys.mark_alive_probe(&key),
                Err(kind) => keys.mark_failed(&key, kind),
            }
        }
        tracing::info!("prewarm: done — {}", keys.summary());
    });
}

/// Settings-window "Test keys": probe `keys_to_test` against `cfg`'s selected
/// provider, all keys **in parallel**, invoking `on_result(key, ok)` as each
/// verdict lands. Purely diagnostic — does not touch the live KeyPool.
pub fn spawn_key_test(
    app: &App,
    cfg: Config,
    keys_to_test: Vec<String>,
    on_result: std::sync::Arc<dyn Fn(String, bool) + Send + Sync>,
) {
    let cfg = Arc::new(cfg);
    for key in keys_to_test {
        let cfg = Arc::clone(&cfg);
        let on_result = Arc::clone(&on_result);
        app.rt.spawn(async move {
            // Each probe builds its own provider (cheap unit structs) so the
            // probes are fully independent and run concurrently.
            let provider = make_provider(&cfg);
            let opts = SttSessionOpts {
                language: provider.language_for(&cfg.language),
                sample_rate: provider.required_audio_format().sample_rate,
                model: cfg.stt_model.clone(),
                custom_vocabulary: Vec::new(),
            };
            let ok = probe_key(provider.as_ref(), &key, &opts).await.is_ok();
            on_result(key, ok);
        });
    }
}

/// Probe one key: connect, push ~0.1 s of silence, (batch providers: commit so
/// the HTTP round-trip actually runs), then listen briefly for an auth/quota
/// failure event. No event inside the window = the provider accepted us.
async fn probe_key(
    provider: &dyn SttProvider,
    key: &str,
    opts: &SttSessionOpts,
) -> Result<(), FailKind> {
    let connect = tokio::time::timeout(Duration::from_secs(6), provider.connect(key, opts));
    let ProviderSession {
        mut sink,
        mut stream,
    } = match connect.await {
        Err(_) => return Err(FailKind::Transient), // timed out — network, not the key
        Ok(Err(e)) => return Err(provider.classify_connect_error(&e)),
        Ok(Ok(s)) => s,
    };
    // ~0.1 s of silence: harmless for streaming providers (no VAD trigger),
    // and gives batch providers a body to submit.
    let silence = vec![0i16; (opts.sample_rate / 10) as usize];
    let _ = sink.send_audio(&silence).await;
    if provider.id() == "google" {
        // Batch: the key is only exercised by the recognize POST in commit().
        let _ = sink.commit().await;
    }
    let listen = tokio::time::timeout(Duration::from_millis(1500), async {
        loop {
            match stream.recv_event().await {
                Ok(Some(SttEvent::KeyFailure(kind))) => return Some(kind),
                Ok(Some(_)) => continue, // SessionStarted / partials — fine
                Ok(None) | Err(_) => return None,
            }
        }
    });
    match listen.await {
        Ok(Some(kind)) => Err(kind),
        // Timeout (quiet stream) or clean close: the provider accepted the key.
        _ => {
            let _ = sink.close().await;
            Ok(())
        }
    }
}

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

/// Trim, drop blanks, and de-duplicate the user's biasing terms before they go
/// on the wire. Case-insensitive de-dup keeping first-seen order, so a list
/// hand-edited in settings.json cannot send the same term three times and eat
/// a provider's term budget.
fn normalize_vocabulary(terms: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for term in terms {
        let t = term.trim();
        if t.is_empty() {
            continue;
        }
        let folded = t.to_ascii_lowercase();
        if seen.contains(&folded) {
            continue;
        }
        seen.push(folded);
        out.push(t.to_string());
    }
    out
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SentAudio {
    chunks: usize,
    samples: u64,
    /// The send half watched the socket die (a failed `ship` or keepalive)
    /// before it was done. Chunk counts alone cannot tell "the user said
    /// nothing" apart from "we were cut off before they got a word in", and
    /// only the second one is worth an error pip. See
    /// [`transport_failure_lost_speech`].
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
struct TailSilenceGate {
    /// Silent chunks captured since the last speech chunk, awaiting either a
    /// flush (speech resumed) or a discard (tail ended still-silent).
    pending: Vec<Vec<i16>>,
}

impl TailSilenceGate {
    /// Offer one captured chunk with the caller's speech/silence verdict (RMS
    /// vs the silence floor). Returns the chunks to forward to the provider
    /// *now*, in order: empty while we're inside a silent stretch, or the held
    /// pause followed by this chunk the moment speech resumes.
    fn offer(&mut self, chunk: Vec<i16>, is_speech: bool) -> Vec<Vec<i16>> {
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
    fn held(&self) -> usize {
        self.pending.len()
    }
}

/// State [`run_send_task`] owns for a session's outbound audio: the
/// resampled-audio receiver, the provider sink, and the counters the recv
/// task and the end-of-session gate read back. Bundled into one struct
/// because the send task's phases (live / dynamic tail / drain) all close
/// over every field.
struct SendTaskState {
    samples_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
    sink: Box<dyn ProviderSink>,
    /// Consumed (not just borrowed) by [`send_task_drain_phase`], so it's an
    /// `Option` rather than a plain field: `SessionFlusher::finish` takes
    /// `self` by value, which a field behind `&mut SendTaskState` can't hand
    /// out directly.
    flusher: Option<crate::audio::SessionFlusher>,
    release_pending: Arc<AtomicBool>,
    speech_shipped: Arc<AtomicU64>,
    sent_progress: Arc<parking_lot::Mutex<SentAudio>>,
    tail_quiet: Duration,
    tail_max: Duration,
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
/// commit + close. Runs on its own `tokio::spawn` from [`run_session`].
async fn run_send_task(mut state: SendTaskState) -> SentAudio {
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

async fn run_session(
    app: Arc<App>,
    keys: Arc<KeyPool>,
    stop: Arc<AtomicBool>,
    epoch: u64,
    session_usage: Arc<parking_lot::Mutex<SessionUsage>>,
) -> Result<()> {
    tracing::info!("session[{epoch}] starting");

    // If the global capture stream has died (mic unplugged, driver error), this
    // press would silently record nothing while hotkeys/tray/UI still look alive.
    // Surface the visible error pip and abort instead of pretending to listen;
    // the audio thread keeps retrying the device, so a later press recovers.
    // (Mirrors the session-error flash below.)
    if !app.audio.is_healthy() {
        tracing::error!(
            "session[{epoch}] aborted: audio capture is not running (microphone lost?) — device reopen is retried automatically"
        );
        if app.current_session_epoch() == epoch {
            // A lost mic is a generic error (the "!" pip), not a key problem.
            app.raise_error(crate::state::ErrorKind::Generic);
            let app_for_clear = Arc::clone(&app);
            app.rt.spawn(async move {
                tokio::time::sleep(ERROR_PIP_VISIBLE).await;
                if app_for_clear.current_session_epoch() == epoch {
                    app_for_clear.clear_status_if(Status::Error, Status::Idle);
                }
            });
        }
        return Ok(());
    }

    let cfg = app.config.load_full();
    // Resolve Per-App Profile overrides ONCE, at session start. The profile
    // that matters for provider/language is the one for the window the user
    // was in when they pressed the hotkey; the text-processing profile is
    // resolved separately at commit time in output.rs, because by then focus
    // may legitimately have moved.
    let exe_at_start = crate::focus::foreground_exe_name();
    let effective = cfg.effective_settings(exe_at_start.as_deref());
    let resolved_provider = cfg.provider_for_exe(exe_at_start.as_deref());
    // A profile that names a DIFFERENT provider needs that provider's keys,
    // so the session runs on its own pool. Everything else keeps using the
    // shared pool the main loop maintains, byte-identically to before.
    let keys = match resolved_provider.as_deref() {
        Some(want) if want != keys.provider_id() => {
            tracing::info!(
                "session[{epoch}] profile for {:?} overrides the provider: {} -> {want}",
                exe_at_start.as_deref().unwrap_or("<unknown>"),
                keys.provider_id()
            );
            crate::keys::KeyPool::for_provider(&cfg, want)
        }
        _ => keys,
    };
    let provider = make_provider_id(
        resolved_provider.as_deref().unwrap_or(&cfg.stt_provider),
        &cfg,
    );
    let provider_id = provider.id();
    let requires_api_key = provider.requires_api_key();
    let finalize_timeout = provider.finalize_timeout();
    let final_transcript_timeout = provider.final_transcript_timeout();
    // Whether this provider needs the phantom-finalization guard (ElevenLabs
    // Scribe completes a question into a hallucinated short "answer" at
    // end-of-stream). Read once here so the recv task captures a plain bool.
    let suppress_phantom = provider.suppress_phantom_finalization();

    let key = if requires_api_key {
        match keys.acquire() {
            Some(k) => k,
            None => {
                tracing::info!("session[{epoch}] pool empty; waiting up to 1.5 s for refresh");
                if !keys.wait_until_ready(Duration::from_millis(1500)).await {
                    anyhow::bail!("no API key available");
                }
                keys.acquire().ok_or_else(|| anyhow!("no API key"))?
            }
        }
    } else {
        String::new()
    };
    // Positional label, never a slice of the credential: log files end up
    // attached to bug reports.
    let key_suffix = keys.label(&key);
    if requires_api_key {
        tracing::info!("session[{epoch}] provider={provider_id} using key {key_suffix}");
        *app.current_key.lock() = Some(key.clone());
    } else {
        tracing::info!("session[{epoch}] provider={provider_id} (no API key)");
        *app.current_key.lock() = None;
    }

    let fmt = provider.required_audio_format();
    let opts = SttSessionOpts {
        language: provider.language_for(&effective.language),
        sample_rate: fmt.sample_rate,
        model: cfg.stt_model.clone(),
        custom_vocabulary: normalize_vocabulary(&effective.custom_vocabulary),
    };

    // Subscribe to the pre-warmed global audio pipeline BEFORE connecting so a
    // connect failure still drops the flusher and unregisters cleanly. The
    // provider's required rate (16 kHz streaming, 24 kHz OpenAI) drives the
    // per-session resampler.
    let (samples_rx, flusher) = app.audio.subscribe(fmt.sample_rate);

    let connect_start = Instant::now();
    let ProviderSession { sink, mut stream } = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        provider.connect(&key, &opts),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            if !requires_api_key {
                return Err(anyhow!("{provider_id} connect failed: {e}"));
            }
            // A connect-stage failure is (almost always) a per-key problem —
            // bad credential, arrears, quota — so signal the retry shell to
            // rotate to the next key instead of giving up on the whole press.
            // (This was the DashScope red-"!" bug: its arrears error surfaces
            // at connect, and a plain error here killed the session outright.)
            keys.mark_failed(&key, provider.classify_connect_error(&e));
            tracing::warn!(
                "session[{epoch}] {provider_id} connect failed with key ...{key_suffix}: {e}"
            );
            return Err(anyhow!(EXHAUSTED_SIGNAL));
        }
        Err(_) => {
            if !requires_api_key {
                return Err(anyhow!(
                    "{provider_id} connect timed out after {CONNECT_TIMEOUT:?}"
                ));
            }
            // Exceeded CONNECT_TIMEOUT: a stalled handshake, not a bad key.
            // Treat it as transient and rotate rather than hang the press.
            keys.mark_failed(&key, FailKind::Transient);
            tracing::warn!(
                    "session[{epoch}] {provider_id} connect timed out after {CONNECT_TIMEOUT:?} with key ...{key_suffix}"
                );
            return Err(anyhow!(EXHAUSTED_SIGNAL));
        }
    };
    tracing::info!(
        "session[{epoch}] {provider_id} connected in {:?}",
        connect_start.elapsed()
    );

    if app.current_session_epoch() != epoch || stop.load(Ordering::Acquire) {
        return Ok(());
    }

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
    let release_pending_send = Arc::clone(&release_pending);
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
    let speech_shipped_send = Arc::clone(&speech_shipped);
    let speech_shipped_recv = Arc::clone(&speech_shipped);
    let sent_progress = Arc::new(parking_lot::Mutex::new(SentAudio::default()));
    let sent_progress_send = Arc::clone(&sent_progress);
    let mut send_task: tokio::task::JoinHandle<SentAudio> =
        tokio::spawn(run_send_task(SendTaskState {
            samples_rx,
            sink,
            flusher: Some(flusher_send),
            release_pending: release_pending_send,
            speech_shipped: speech_shipped_send,
            sent_progress: sent_progress_send,
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

    // Shared accumulators that survive even if we drop the recv JoinHandle on
    // timeout, so any chunks/partials the task already processed stay readable.
    let chunks_buf: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let last_partial_buf: Arc<parking_lot::Mutex<String>> =
        Arc::new(parking_lot::Mutex::new(String::new()));
    let dropped_phantom_buf: Arc<parking_lot::Mutex<Option<String>>> =
        Arc::new(parking_lot::Mutex::new(None));
    let committed_flag = Arc::new(AtomicBool::new(false));
    // Text of the most recent KEPT commit, so the end-of-session fallback can
    // tell a genuinely-unfinalized trailing partial from a partial that
    // merely repeats what was already committed.
    let last_commit_text: Arc<parking_lot::Mutex<String>> =
        Arc::new(parking_lot::Mutex::new(String::new()));
    let transcribed_words = Arc::new(AtomicU64::new(0));
    let key_fail_kind: Arc<parking_lot::Mutex<Option<FailKind>>> =
        Arc::new(parking_lot::Mutex::new(None));
    let provider_failure: Arc<parking_lot::Mutex<Option<String>>> =
        Arc::new(parking_lot::Mutex::new(None));
    let chunks_for_task = Arc::clone(&chunks_buf);
    let last_partial_for_task = Arc::clone(&last_partial_buf);
    let dropped_phantom_for_task = Arc::clone(&dropped_phantom_buf);
    let committed_for_task = Arc::clone(&committed_flag);
    let last_commit_text_for_task = Arc::clone(&last_commit_text);
    let transcribed_words_for_task = Arc::clone(&transcribed_words);
    let key_fail_for_task = Arc::clone(&key_fail_kind);
    let provider_failure_for_task = Arc::clone(&provider_failure);
    let release_pending_recv = Arc::clone(&release_pending);

    // Reset the live word counter at the start of every session.
    app.word_count.store(0, Ordering::Release);
    // Drop any answer speculated for the PREVIOUS press. It is keyed by exact
    // text so it could not be misapplied anyway, but a new dictation should
    // not be racing against a stale in-flight request either.
    app.polish.reset();
    let mut recv_task = tokio::spawn(async move {
        let mut events: usize = 0;
        let mut committed_words: u32 = 0;
        // Snapshot of `speech_shipped` taken at the last *kept* commit. Compared
        // against the live count at each new commit to spot a phantom (equal =>
        // no speech shipped in between). Starts at 0, so the very first real
        // commit -- always backed by shipped speech -- is never mistaken for one.
        let mut last_commit_speech: u64 = 0;
        loop {
            let ev = match stream.recv_event().await {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
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
                    // user watched succeed.
                    tracing::warn!("session[{epoch}] recv error: {e}");
                    let mut slot = provider_failure_for_task.lock();
                    if slot.is_none() {
                        *slot = Some(format!("transport failed mid-session: {e}"));
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
                    if log_transcripts {
                        tracing::debug!("session[{epoch}] partial: {t}");
                    } else {
                        tracing::debug!("session[{epoch}] partial: {} char(s)", t.chars().count());
                    }
                    let partial_words = t.split_whitespace().count() as u32;
                    recv_app
                        .word_count
                        .store(committed_words + partial_words, Ordering::Release);
                    *last_partial_for_task.lock() = t;
                }
                SttEvent::Committed(final_text) => {
                    // Drop the chunk entirely if a NEWER session has taken over.
                    if recv_app.current_session_epoch() != epoch {
                        tracing::debug!(
                            "session[{epoch}] dropping late commit (newer session active)"
                        );
                        continue;
                    }

                    let released = release_pending_recv.load(Ordering::Acquire);
                    let speech_now = speech_shipped_recv.load(Ordering::Acquire);

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
                    if suppress_phantom
                        && is_phantom_finalization(released, speech_now, last_commit_speech)
                        && looks_like_short_answer(&final_text)
                    {
                        *dropped_phantom_for_task.lock() = Some(final_text.clone());
                        let mut partial = last_partial_for_task.lock();
                        if transcripts_equivalent(&partial, &final_text) {
                            partial.clear();
                        }
                        if log_transcripts {
                            tracing::info!(
                                "session[{epoch}] dropped phantom finalization (no speech since last commit): {final_text}"
                            );
                        } else {
                            tracing::info!(
                                "session[{epoch}] dropped phantom finalization (no speech since last commit): {} char(s)",
                                final_text.chars().count()
                            );
                        }
                        continue;
                    }

                    // A transcript we're keeping. Mark that we have durable
                    // committed text (disarms the last-partial fallback) and
                    // advance the speech baseline for the next phantom check.
                    // Set ONLY for kept commits: a dropped phantom must not trip
                    // this, or a session whose only real content arrived as a
                    // partial would lose its promotion fallback.
                    committed_for_task.store(true, Ordering::Release);
                    last_commit_speech = speech_now;

                    // This commit supersedes every partial up to this point, so
                    // clear the buffer. What lands in it AFTER this is speech
                    // from a LATER segment, and that segment deserves the
                    // last-partial fallback even though an earlier commit
                    // already succeeded. The old session-wide `!got_committed`
                    // gate disabled the fallback for the rest of the session
                    // after the first commit, so a final segment whose
                    // finalization timed out was discarded outright.
                    last_partial_for_task.lock().clear();
                    *last_commit_text_for_task.lock() = final_text.clone();

                    let chunk_words = final_text.split_whitespace().count() as u32;
                    committed_words = committed_words.saturating_add(chunk_words);
                    transcribed_words_for_task.fetch_add(chunk_words as u64, Ordering::AcqRel);
                    recv_app
                        .word_count
                        .store(committed_words, Ordering::Release);

                    // Hybrid paste flow:
                    //   before release              -> HOLD (accumulate)
                    //   after release               -> LIVE (paste each chunk)
                    //   delay_until_release = false -> LIVE throughout
                    if delay_until_release && !released {
                        if log_transcripts {
                            tracing::info!(
                                "session[{epoch}] committed (held until release): {final_text}"
                            );
                        } else {
                            tracing::info!(
                                "session[{epoch}] committed (held until release): {} char(s)",
                                final_text.chars().count()
                            );
                        }
                        let prefix = {
                            let mut held = chunks_for_task.lock();
                            held.push(final_text);
                            // Same join the release flush will do, so a hit is
                            // an exact-text hit rather than a near miss.
                            speculate_polish.then(|| held.join(" "))
                        };
                        // Free time: the user is still talking and none of
                        // this is on screen yet, so run the cleanup pass over
                        // everything committed so far. If they release while
                        // it is still thinking, the deadline race takes over
                        // and nothing here has cost them anything.
                        if let Some(prefix) = prefix {
                            if let Some(settings) = polish_settings.as_ref() {
                                recv_app.polish.speculate(settings, &prefix);
                            }
                        }
                    } else {
                        if log_transcripts {
                            tracing::info!(
                                "session[{epoch}] committed (live, append): {final_text}"
                            );
                        } else {
                            tracing::info!(
                                "session[{epoch}] committed (live, append): {} char(s)",
                                final_text.chars().count()
                            );
                        }
                        deliver_transcript(&recv_app.transcript_tx, final_text);
                    }
                }
                SttEvent::KeyFailure(kind) => {
                    tracing::warn!("session[{epoch}] provider signaled key failure ({kind:?})");
                    *key_fail_for_task.lock() = Some(kind);
                    // Don't break: the outer wait loop observes key_fail_kind and
                    // tears the session down / rotates keys.
                }
                SttEvent::ProviderFailure(message) => {
                    tracing::error!("session[{epoch}] {provider_id} failed: {message}");
                    *provider_failure_for_task.lock() = Some(message);
                }
                SttEvent::Closed(reason) => {
                    match reason {
                        Some(r) => {
                            tracing::warn!("session[{epoch}] transport closed by server ({r})")
                        }
                        None => tracing::info!("session[{epoch}] transport closed by server"),
                    }
                    break;
                }
            }
        }
        tracing::info!("session[{epoch}] recv_task ended (events={events})");
    });

    while !stop.load(Ordering::Acquire) {
        if app.current_session_epoch() != epoch {
            break;
        }
        // Break the moment we know the session is unusable so the retry shell
        // sees the failure without waiting for the user to press again.
        if key_fail_kind.lock().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Fast-fail: if the provider already told us the key is dead, skip the
    // entire finalize and hand back to the retry shell to rotate keys.
    let early_key_failure = *key_fail_kind.lock();
    if let Some(kind) = early_key_failure {
        tracing::warn!(
            "session[{epoch}] aborting finalize early -- key ...{key_suffix} failed ({kind:?})"
        );
        // Do not detach either half while the retry shell rotates keys. Besides
        // retaining an obsolete audio subscription, a late receiver could paste
        // into the replacement attempt. Preserve any words that were already
        // live-pasted (delay=false) and the audio known to have been shipped.
        send_task.abort();
        recv_task.abort();
        let _ = send_task.await;
        let _ = recv_task.await;
        if !delay_until_release {
            let words = transcribed_words.load(Ordering::Acquire);
            if words > 0 {
                let sent = *sent_progress.lock();
                session_usage.lock().add_fragment(
                    provider_id,
                    words,
                    audio_duration_ms(sent.samples, fmt.sample_rate),
                );
            }
        }
        keys.mark_failed(&key, kind);
        return Err(anyhow!(EXHAUSTED_SIGNAL));
    }

    tracing::info!(
        "session[{epoch}] release pending; entering dynamic tail (min={:?}, quiet={:?}, max={:?})",
        TAIL_MIN,
        tail_quiet,
        tail_max
    );
    // Flip the release flag FIRST so recv switches to live-paste mode for any
    // chunks the server sends from this point on.
    release_pending.store(true, Ordering::Release);

    // Then flush anything held during the session so release feels snappy.
    let release_flush: Vec<String> = std::mem::take(&mut *chunks_buf.lock());
    if !release_flush.is_empty() {
        let joined = release_flush.join(" ");
        if app.current_session_epoch() == epoch {
            tracing::info!(
                "session[{epoch}] release flush: {} chunk(s), {} chars",
                release_flush.len(),
                joined.chars().count()
            );
            deliver_transcript(&app.transcript_tx, joined);
        } else {
            tracing::info!(
                "session[{epoch}] skipping release flush because a newer action superseded it"
            );
        }
    }

    // Bound the wait so we never get stuck if something goes wrong. Streaming
    // providers finish within the tail window; batch providers (Google) need
    // longer for their in-`commit()` POST, hence the provider-supplied value.
    // Keep the budget above the (now user-configurable) tail as well, so a
    // long "keep listening" window can't get cut off before commit()/close()
    // and drop the final transcript. The provider's own timeout stays the
    // floor (Google's 45 s dwarfs any tail).
    let send_deadline = finalize_timeout.max(tail_max + Duration::from_millis(600));
    let sent = match tokio::time::timeout(send_deadline, &mut send_task).await {
        Ok(Ok(sent)) => sent,
        Ok(Err(e)) => {
            tracing::warn!("session[{epoch}] send_task join error: {e}");
            *sent_progress.lock()
        }
        Err(_) => {
            tracing::warn!(
                "session[{epoch}] send_task did not finish in {send_deadline:?}; cancelling it"
            );
            send_task.abort();
            let _ = send_task.await;
            *sent_progress.lock()
        }
    };
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
    let recv_finished = tokio::time::timeout(final_transcript_timeout, &mut recv_task)
        .await
        .is_ok();
    if !recv_finished {
        tracing::warn!(
            "session[{epoch}] recv_task did not finish within {:?}; cancelling it before promoting any partial",
            final_transcript_timeout
        );
        recv_task.abort();
        let _ = recv_task.await;
    }

    let got_committed = committed_flag.load(Ordering::Acquire);
    // Sweep once more in case recv pushed a chunk between us flipping
    // release_pending and taking the buffer.
    let held_chunks = std::mem::take(&mut *chunks_buf.lock());
    let last_partial = std::mem::take(&mut *last_partial_buf.lock());
    let dropped_phantom = dropped_phantom_buf.lock().take();

    if !held_chunks.is_empty() {
        let joined = held_chunks.join(" ");
        if app.current_session_epoch() == epoch {
            tracing::info!(
                "session[{epoch}] flushing {} held commit chunk(s), {} chars total",
                held_chunks.len(),
                joined.chars().count()
            );
            deliver_transcript(&app.transcript_tx, joined);
        } else {
            tracing::info!(
                "session[{epoch}] skipping held commit flush because a newer action superseded it"
            );
        }
    }

    // The last-partial fallback is now per SEGMENT, not per session: a kept
    // commit clears the partial buffer, so anything left here is speech that
    // arrived after the last commit and never got finalized (the provider hit
    // `final_transcript_timeout`). Gating it on `!got_committed` used to throw
    // that trailing segment away for the rest of the session as soon as one
    // earlier sentence committed. `got_committed` still guards the
    // "no transcript at all" diagnostic below, which is genuinely per session.
    let had_partial = !last_partial.is_empty();
    let partial_was_dropped_phantom = dropped_phantom
        .as_deref()
        .is_some_and(|phantom| transcripts_equivalent(phantom, &last_partial));
    // Belt and braces: if a provider re-emits the committed text as a trailing
    // partial, promoting it would paste the same words twice.
    let partial_repeats_last_commit = {
        let last = last_commit_text.lock();
        !last.is_empty() && transcripts_equivalent(&last, &last_partial)
    };
    if had_partial && partial_was_dropped_phantom {
        tracing::info!(
            "session[{epoch}] suppressing last partial because it matches a dropped phantom finalization"
        );
    } else if had_partial && partial_repeats_last_commit {
        tracing::info!(
            "session[{epoch}] suppressing last partial because it repeats the last commit"
        );
    } else if had_partial && app.current_session_epoch() == epoch {
        transcribed_words.fetch_add(
            last_partial.split_whitespace().count() as u64,
            Ordering::AcqRel,
        );
        if log_transcripts {
            tracing::info!("session[{epoch}] promoting last partial: {last_partial}");
        } else {
            tracing::info!(
                "session[{epoch}] promoting last partial: {} char(s)",
                last_partial.chars().count()
            );
        }
        deliver_transcript(&app.transcript_tx, last_partial);
    } else if had_partial {
        tracing::info!(
            "session[{epoch}] skipping last partial because a newer action superseded it"
        );
    }
    if !got_committed && !had_partial && sent.chunks == 0 {
        tracing::warn!("session[{epoch}] produced no transcript (zero audio chunks sent -- session ended before mic was warm)");
    }

    // Happy path only reaches here (fast-fail returned above on failure).
    let key_failure = *key_fail_kind.lock();
    if let Some(kind) = key_failure {
        keys.mark_failed(&key, kind);
        tracing::warn!("session[{epoch}] ended with FAILED key ({kind:?}); pool will rotate");
    } else {
        if requires_api_key {
            keys.mark_success(&key, audio_ms);
        }
    }
    let words = transcribed_words.load(Ordering::Acquire);
    if words > 0 {
        session_usage
            .lock()
            .add_fragment(provider_id, words, audio_ms);
    }
    crate::sound::play_stop(cfg.enable_sound);
    tracing::info!("session[{epoch}] ended");
    if key_failure.is_some() {
        return Err(anyhow!(EXHAUSTED_SIGNAL));
    }
    if let Some(message) = provider_failure.lock().take() {
        // A transport that died without costing the user anything is a
        // teardown, not a failure. ElevenLabs in particular often drops the TCP
        // connection without a closing handshake, so `recv_event` reports
        // "Connection reset without closing handshake" on sessions that lost
        // nothing at all. Raising the error pip for those is a lie. The point
        // of recording a mid-session transport error is the case where speech
        // was LOST, so gate on exactly that (see
        // `transport_failure_lost_speech`).
        if !transport_failure_lost_speech(words, sent.socket_died) {
            if words > 0 {
                tracing::info!(
                    "session[{epoch}] transport dropped during teardown after delivering \
                     {words} word(s); not surfacing an error ({message})"
                );
            } else {
                tracing::info!(
                    "session[{epoch}] transport dropped on an empty dictation -- the provider \
                     returned no words at all ({speech_chunks} chunk(s) were above our silence \
                     floor), so there is no transcript to lose; not surfacing an error \
                     ({message})"
                );
            }
            return Ok(());
        }
        return Err(anyhow!(message));
    }
    Ok(())
}

/// True when a committed transcript is a hallucinated end-of-stream
/// finalization rather than something the user actually said.
///
/// ElevenLabs Scribe (`scribe_v2_realtime`, `commit_strategy=vad`) will, when an
/// utterance is finalized, occasionally emit its language-model prior "answer"
/// to the preceding question as a fresh `committed_transcript` -- ask "should we
/// do X?", stop, and it commits "Yes." -- even when we send it no trailing audio
/// at all. The tell is that **no speech-bearing audio was shipped to the
/// provider between the previous commit and this one**: `speech_now` (the running
/// speech-chunk count) still equals `speech_at_last_commit` (its value at the
/// last kept commit).
///
/// We only judge this **after release** (`released`): before release, mid-
/// utterance VAD commits are held/accumulated and must always be kept. And
/// because a genuinely-spoken trailing word ships speech first (advancing
/// `speech_now`), a real "Yes." is never mistaken for the phantom -- only a
/// commit conjured out of silence is dropped.
#[inline]
fn is_phantom_finalization(released: bool, speech_now: u64, speech_at_last_commit: u64) -> bool {
    released && speech_now == speech_at_last_commit
}

/// Did a mid-session transport failure actually cost the user any speech?
/// Only when it did is the red "!" pip honest. Three cases:
///
/// * `words > 0` -- the transcript already landed and the user watched it get
///   typed. The reset is ElevenLabs hanging up after a job well done.
/// * `words == 0` and the send half never saw the socket die -- the press
///   produced nothing this app judged worth typing. Either the provider never
///   emitted a single word, or everything it emitted was discarded as a
///   phantom finalization. Both are EMPTY presses: start a dictation, say
///   nothing, stop. There is no transcript to lose, so the reset costs the
///   user nothing and a red "!" is complaining about a press they already
///   know was empty.
/// * `words == 0` and the socket died under the send half -- we were cut off
///   mid-press, so anything said from that moment on never even reached the
///   provider. That is a real failure and keeps the pip.
///
/// Note what is deliberately NOT consulted: our own count of speech-bearing
/// (RMS >= [`SILENCE_RMS`]) chunks. Measured over ten consecutive silent
/// presses, three of them shipped 3-17 "speech-bearing" chunks of room noise
/// and ElevenLabs returned not one partial for any of them -- so a bare RMS
/// floor is a far worse judge of "did a human say something" than the
/// provider's own verdict, and gating the pip on it just moved the false
/// alarms around. The count is still logged next to the chunk totals, because
/// it is exactly what you want when diagnosing a press after the fact.
#[inline]
fn transport_failure_lost_speech(words: u64, socket_died: bool) -> bool {
    words == 0 && socket_died
}

fn transcripts_equivalent(left: &str, right: &str) -> bool {
    fn normalized(text: &str) -> String {
        text.chars()
            .filter(|ch| ch.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    }

    let left = normalized(left);
    !left.is_empty() && left == normalized(right)
}

/// Upper bounds on what the phantom guard is willing to drop. The hallucinated
/// "answer" is always a tiny interjection ("Yes.", "No.", "Okay.", "Like...",
/// "Absolutely."); real dictation flushed at finalize is a fuller clause.
const PHANTOM_MAX_WORDS: usize = 4;
const PHANTOM_MAX_CHARS: usize = 24;

/// Secondary gate on the phantom drop (the primary being "no speech shipped
/// since the last commit"): is `text` short enough to *be* a phantom answer
/// rather than real dictated content? This bounds the count/commit attribution
/// race (a slow VAD commit delivering a real segment post-release could look
/// phantom by count alone) so the guard can never silently eat a real sentence
/// -- only ever a plausible answer. See the Committed arm.
#[inline]
fn looks_like_short_answer(text: &str) -> bool {
    let t = text.trim();
    t.chars().count() <= PHANTOM_MAX_CHARS && t.split_whitespace().count() <= PHANTOM_MAX_WORDS
}

/// Root-mean-square amplitude of an i16 buffer. Cheap (one pass, integer math
/// + one sqrt). Distinguishes "still talking" from "ambient noise" in the tail.
#[inline]
fn rms_i16(samples: &[i16]) -> i32 {
    if samples.is_empty() {
        return 0;
    }
    let mut sum: i64 = 0;
    for &s in samples {
        let v = s as i64;
        sum += v * v;
    }
    let mean = sum / samples.len() as i64;
    (mean as f64).sqrt() as i32
}

#[cfg(test)]
mod tests {
    use super::{
        audio_duration_ms, is_phantom_finalization, looks_like_short_answer,
        transcripts_equivalent, transport_failure_lost_speech, SentAudio, SessionUsage,
        TailSilenceGate,
    };

    #[test]
    fn short_answer_detector_matches_observed_phantoms() {
        // Every phantom shape observed during the original Scribe investigation.
        for p in [
            "Yes.",
            "No.",
            "Yeah.",
            "Okay.",
            "Sure.",
            "Like...",
            "Absolutely.",
            "I think so.",
        ] {
            assert!(
                looks_like_short_answer(p),
                "{p:?} should read as a phantom answer"
            );
        }
    }

    #[test]
    fn short_answer_detector_spares_real_sentences() {
        // A real trailing clause a slow VAD commit might deliver post-release
        // must never be eaten, even if the count-based check misfires.
        assert!(!looks_like_short_answer(
            "Can we make them properly sized instead of super wide?"
        ));
        assert!(!looks_like_short_answer(
            "please refactor this whole function"
        ));
    }

    #[test]
    fn phantom_guard_drops_post_release_commit_with_no_new_speech() {
        // The bug: question committed pre-release at speech=15; release; the tail
        // ships nothing; Scribe finalizes "Yes." while the count is still 15.
        assert!(is_phantom_finalization(true, 15, 15));
    }

    #[test]
    fn phantom_guard_keeps_a_genuinely_spoken_trailing_word() {
        // A real trailing "Yes." ships at least one speech chunk first (16 > 15),
        // so it must NOT be dropped.
        assert!(!is_phantom_finalization(true, 16, 15));
    }

    #[test]
    fn phantom_guard_never_touches_pre_release_commits() {
        // Before release, mid-utterance VAD commits are held and always kept,
        // regardless of the speech counts.
        assert!(!is_phantom_finalization(false, 15, 15));
        assert!(!is_phantom_finalization(false, 0, 0));
    }

    #[test]
    fn phantom_guard_keeps_words_flushed_by_a_mid_sentence_release() {
        // Released mid-sentence: the final real words shipped in the live phase
        // (speech=20) but VAD never committed them (last commit still at 0). The
        // manual commit flushes them post-release; new speech since the last
        // commit means this is real, not a phantom.
        assert!(!is_phantom_finalization(true, 20, 0));
    }

    #[test]
    fn an_empty_dictation_never_raises_the_error_pip() {
        // The reported bug: start a dictation, say nothing, stop. ElevenLabs
        // resets the socket without a closing handshake, but the provider
        // returned no words, so there is no transcript to lose and no "!".
        assert!(!transport_failure_lost_speech(0, false));
    }

    #[test]
    fn room_noise_on_an_empty_press_is_still_an_empty_press() {
        // Same call, stated separately because it is the case that made the
        // first fix wrong: a silent press in a room with a TV on ships plenty
        // of chunks above SILENCE_RMS, and ElevenLabs still returns nothing.
        // Zero words back means an empty press however loud the room was.
        assert!(!transport_failure_lost_speech(0, false));
    }

    #[test]
    fn a_socket_that_died_under_us_still_raises_the_pip() {
        // Cut off mid-press: whatever was said from that point never reached
        // the provider, so the user really did lose speech and should see it.
        assert!(transport_failure_lost_speech(0, true));
    }

    #[test]
    fn a_reset_after_the_words_landed_is_teardown_not_failure() {
        // The pre-existing guard, unchanged: the user watched their sentence
        // get typed, so how the socket closed afterwards is not their problem.
        assert!(!transport_failure_lost_speech(27, false));
        assert!(!transport_failure_lost_speech(27, true));
    }

    #[test]
    fn dropped_phantom_matches_its_partial_fallback() {
        assert!(transcripts_equivalent(" 100%   Go. ", "100% go."));
        assert!(transcripts_equivalent("Okay…", "OKAY!"));
        assert!(!transcripts_equivalent("100% Go.", "100% Go. now"));
        assert!(!transcripts_equivalent("...", ""));
    }

    #[test]
    fn audio_duration_uses_samples_not_assumed_chunk_length() {
        assert_eq!(audio_duration_ms(16_000, 16_000), 1_000);
        assert_eq!(audio_duration_ms(24_000, 24_000), 1_000);
        assert_eq!(audio_duration_ms(1_200, 24_000), 50);
        assert_eq!(audio_duration_ms(1_200, 0), 0);

        let mut sent = SentAudio::default();
        sent.record_prefix(&[vec![0; 1_600], vec![0; 800]], 2);
        assert_eq!(sent.chunks, 2);
        assert_eq!(sent.samples, 2_400);
        assert_eq!(audio_duration_ms(sent.samples, 16_000), 150);
    }

    #[test]
    fn retry_fragments_form_one_physical_dictation_total() {
        let mut usage = SessionUsage::default();
        usage.add_fragment("openai", 40, 12_000);
        usage.add_fragment("openai", 15, 4_000);
        assert_eq!(
            usage,
            SessionUsage {
                provider: "openai".into(),
                words: 55,
                audio_ms: 16_000,
            }
        );
    }

    #[test]
    fn speech_with_no_held_pause_ships_immediately_and_alone() {
        let mut g = TailSilenceGate::default();
        let out = g.offer(vec![9000; 4], true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][0], 9000);
        assert_eq!(g.held(), 0);
    }

    #[test]
    fn silence_is_held_back_not_shipped() {
        let mut g = TailSilenceGate::default();
        assert!(g.offer(vec![0; 4], false).is_empty());
        assert!(g.offer(vec![1; 4], false).is_empty());
        assert_eq!(g.held(), 2);
    }

    #[test]
    fn resumed_speech_flushes_the_held_pause_in_order_then_the_speech() {
        // A genuine mid-utterance pause must reach the provider verbatim so the
        // words after it aren't spliced onto the words before it.
        let mut g = TailSilenceGate::default();
        g.offer(vec![10; 1], false); // pause chunk A
        g.offer(vec![20; 1], false); // pause chunk B
        let out = g.offer(vec![9000; 1], true); // speech resumes
        assert_eq!(out.len(), 3);
        assert_eq!(out[0][0], 10); // A first
        assert_eq!(out[1][0], 20); // then B
        assert_eq!(out[2][0], 9000); // then the speech chunk
        assert_eq!(g.held(), 0); // buffer drained on flush
    }

    #[test]
    fn trailing_silence_never_followed_by_speech_is_never_emitted() {
        // This is the whole point: the run of silence after the last real word
        // stays held, so the caller discards it and the model never sees dead
        // air to finalize into a hallucinated "Yes."
        let mut g = TailSilenceGate::default();
        assert_eq!(g.offer(vec![9000; 1], true).len(), 1); // last real word ships
        assert!(g.offer(vec![0; 1], false).is_empty());
        assert!(g.offer(vec![0; 1], false).is_empty());
        assert!(g.offer(vec![0; 1], false).is_empty());
        assert_eq!(g.held(), 3); // all held; caller drops them, none sent
    }

    #[test]
    fn alternating_speech_resets_the_held_run_each_time() {
        let mut g = TailSilenceGate::default();
        g.offer(vec![9000; 1], true); // speech -> ships, nothing held
        assert_eq!(g.held(), 0);
        g.offer(vec![0; 1], false); // 1 held
        assert_eq!(g.held(), 1);
        let out = g.offer(vec![9000; 1], true); // speech again -> flush 1 + speech
        assert_eq!(out.len(), 2);
        assert_eq!(g.held(), 0);
    }
}
