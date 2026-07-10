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
#[cfg(feature = "google")]
mod google;
mod openai;
pub mod provider;

#[cfg(test)]
mod live_test;
#[cfg(test)]
mod mock;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::time::Instant;

use crate::config::Config;
use crate::keys::{FailKind, KeyPool};
use crate::state::{App, Status};
use provider::{ProviderSession, ProviderSink, SttEvent, SttProvider, SttSessionOpts};

/// How long we keep the recv task alive after the send task has wrapped up so
/// we don't miss the provider's last committed transcript. 1.5 s comfortably
/// covers typical post-commit processing and protects against the "occasional
/// missing tail word" symptom on slower connections.
const FINAL_TRANSCRIPT_WAIT: Duration = Duration::from_millis(1500);

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

/// Hard cap on rounds (3 keys × 2 rounds = up to 6 attempts per press).
const MAX_RETRY_ROUNDS: u32 = 2;

/// Sentinel error asking the retry shell to pick a different key. Stays out of
/// band of normal errors so a real failure (network, mic) still bubbles up.
const EXHAUSTED_SIGNAL: &str = "__quickdictate_key_exhausted__";

/// Build the provider selected in settings.json. Unknown ids fall back to
/// ElevenLabs (the baseline) with a warning. Providers are cheap unit structs,
/// rebuilt per session so a settings edit + restart cleanly switches backend.
fn make_provider(cfg: &Config) -> Box<dyn SttProvider> {
    match cfg.stt_provider.trim().to_ascii_lowercase().as_str() {
        "elevenlabs" => Box::new(elevenlabs::ElevenLabsProvider),
        "deepgram" => Box::new(deepgram::DeepgramProvider),
        "assemblyai" => Box::new(assemblyai::AssemblyAiProvider),
        "dashscope" => Box::new(dashscope::DashScopeProvider {
            intl: cfg.dashscope_intl,
        }),
        "openai" => Box::new(openai::OpenAiProvider),
        "google" => {
            #[cfg(feature = "google")]
            {
                Box::new(google::GoogleProvider)
            }
            #[cfg(not(feature = "google"))]
            {
                tracing::error!(
                    "stt_provider=google requires a build with --features google; \
                     falling back to elevenlabs"
                );
                Box::new(elevenlabs::ElevenLabsProvider)
            }
        }
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
        let provider_id = provider.id();
        let fmt = provider.required_audio_format();
        let opts = SttSessionOpts {
            language: provider.language_for(&cfg.language),
            sample_rate: fmt.sample_rate,
            model: cfg.stt_model.clone(),
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
    app.rt.spawn(async move {
        // Retry shell: a session may fail fast with EXHAUSTED_SIGNAL, in which
        // case we rotate to the next of the user's keys. After a round of
        // MAX_KEY_ATTEMPTS failures we pause briefly (POOL_REFRESH_WAIT) in
        // case a short cooldown lapses, then try another round.
        let mut final_res: Result<()> = Ok(());
        let mut attempts_in_round: u32 = 0;
        let mut rounds_done: u32 = 0;
        let mut total_attempts: u32 = 0;
        let user_aborted =
            || stop.load(Ordering::Acquire) || app2.current_session_epoch() != epoch;
        loop {
            if user_aborted() {
                break;
            }
            attempts_in_round += 1;
            total_attempts += 1;
            let attempt_res =
                run_session(app2.clone(), Arc::clone(&keys), Arc::clone(&stop), epoch).await;
            let is_exhausted =
                matches!(&attempt_res, Err(e) if e.to_string() == EXHAUSTED_SIGNAL);
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
        if let Err(e) = final_res {
            if e.to_string() == EXHAUSTED_SIGNAL {
                tracing::error!(
                    "session[{epoch}] tried {MAX_KEY_ATTEMPTS} keys, none worked -- check provider credit / pool health"
                );
            } else {
                tracing::error!("session error: {e:#}");
            }
            if app2.current_session_epoch() == epoch {
                app2.set_status(Status::Error);
                let app_for_clear = Arc::clone(&app2);
                app2.rt.spawn(async move {
                    tokio::time::sleep(ERROR_PIP_VISIBLE).await;
                    if app_for_clear.current_session_epoch() == epoch {
                        app_for_clear.clear_status_if(Status::Error, Status::Idle);
                    }
                });
            }
        }
        done.store(true, Ordering::Release);
    });
    SttHandle {
        stop: stop_ret,
        done: done_ret,
    }
}

/// Send one PCM chunk through the provider sink. Mirrors the original `ship()`:
/// once a send errors the socket is dead, so we log only the first failure and
/// skip every subsequent send.
async fn ship(sink: &mut Box<dyn ProviderSink>, chunk: &[i16], dead: &mut bool) -> bool {
    if *dead {
        return false;
    }
    match sink.send_audio(chunk).await {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!("provider send error (subsequent sends will be skipped): {e}");
            *dead = true;
            false
        }
    }
}

async fn run_session(
    app: Arc<App>,
    keys: Arc<KeyPool>,
    stop: Arc<AtomicBool>,
    epoch: u64,
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
            app.set_status(Status::Error);
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
    let provider = make_provider(&cfg);
    let provider_id = provider.id();
    let finalize_timeout = provider.finalize_timeout();

    let key = match keys.acquire() {
        Some(k) => k,
        None => {
            tracing::info!("session[{epoch}] pool empty; waiting up to 1.5 s for refresh");
            if !keys.wait_until_ready(Duration::from_millis(1500)).await {
                anyhow::bail!("no API key available");
            }
            keys.acquire().ok_or_else(|| anyhow!("no API key"))?
        }
    };
    let key_suffix: String = key
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    tracing::info!("session[{epoch}] provider={provider_id} using key ...{key_suffix}");
    *app.current_key.lock() = Some(key.clone());

    let fmt = provider.required_audio_format();
    let opts = SttSessionOpts {
        language: provider.language_for(&cfg.language),
        sample_rate: fmt.sample_rate,
        model: cfg.stt_model.clone(),
    };

    // Subscribe to the pre-warmed global audio pipeline BEFORE connecting so a
    // connect failure still drops the flusher and unregisters cleanly. The
    // provider's required rate (16 kHz streaming, 24 kHz OpenAI) drives the
    // per-session resampler.
    let (mut samples_rx, flusher) = app.audio.subscribe(fmt.sample_rate);

    let connect_start = Instant::now();
    let ProviderSession { sink, mut stream } =
        match tokio::time::timeout(CONNECT_TIMEOUT, provider.connect(&key, &opts)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
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
    let send_task: tokio::task::JoinHandle<usize> = tokio::spawn(async move {
        let mut sink = sink;
        let mut chunks_sent: usize = 0;
        let mut ws_dead = false;

        // === Phase 1: live ===
        loop {
            if release_pending_send.load(Ordering::Acquire) || ws_dead {
                break;
            }
            let chunk_opt = tokio::select! {
                v = samples_rx.recv() => v,
                _ = tokio::time::sleep(Duration::from_millis(30)) => continue,
            };
            match chunk_opt {
                Some(chunk) => {
                    if !ship(&mut sink, &chunk, &mut ws_dead).await {
                        break;
                    }
                    chunks_sent += 1;
                }
                None => break,
            }
        }

        // === Phase 2: dynamic tail ===
        let tail_start = tokio::time::Instant::now();
        let mut last_speech = tail_start;
        let mut tail_chunks: usize = 0;
        let mut peak_rms: i32 = 0;
        while !ws_dead {
            let elapsed = tail_start.elapsed();
            if elapsed >= tail_max {
                tracing::info!(
                    "session tail: hit tail_max ({:.0} ms) after {:.0} ms (peak_rms={peak_rms})",
                    tail_max.as_secs_f64() * 1000.0,
                    elapsed.as_secs_f64() * 1000.0
                );
                break;
            }
            let chunk_opt = tokio::select! {
                v = samples_rx.recv() => v,
                _ = tokio::time::sleep(Duration::from_millis(20)) => None,
            };
            if let Some(chunk) = chunk_opt {
                let rms = rms_i16(&chunk);
                if rms > peak_rms {
                    peak_rms = rms;
                }
                if rms >= SILENCE_RMS {
                    last_speech = tokio::time::Instant::now();
                }
                if !ship(&mut sink, &chunk, &mut ws_dead).await {
                    break;
                }
                chunks_sent += 1;
                tail_chunks += 1;
            }
            if elapsed >= TAIL_MIN && last_speech.elapsed() >= tail_quiet {
                tracing::info!(
                    "session tail: ended after {:.0} ms ({} tail chunks, peak_rms={peak_rms}, quiet ={:.0} ms)",
                    elapsed.as_secs_f64() * 1000.0,
                    tail_chunks,
                    last_speech.elapsed().as_secs_f64() * 1000.0
                );
                break;
            }
        }

        // === Phase 3: flush the session's resampler tail, then drain ===
        flusher_send.flush_tail();
        let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        while !ws_dead {
            let chunk_opt = tokio::select! {
                v = samples_rx.recv() => v,
                _ = tokio::time::sleep_until(drain_deadline) => None,
            };
            match chunk_opt {
                Some(chunk) => {
                    if !ship(&mut sink, &chunk, &mut ws_dead).await {
                        break;
                    }
                    chunks_sent += 1;
                }
                None => break,
            }
        }

        // === Phase 4: commit + close (only if the socket is still alive) ===
        if !ws_dead {
            let _ = sink.commit().await;
            let _ = sink.close().await;
        }
        chunks_sent
    });

    let recv_app = Arc::clone(&app);
    let delay_until_release = cfg.delay_output_till_release;
    // Default: never write recognized text to disk, even with `enable_logging`
    // on -- only a char-count/context summary. `log_transcripts` is a separate
    // opt-in for deep debugging that restores full-text logging at these
    // sites (and see `output.rs` for the paste-side log lines it also gates).
    let log_transcripts = cfg.log_transcripts;

    // Shared accumulators that survive even if we drop the recv JoinHandle on
    // timeout, so any chunks/partials the task already processed stay readable.
    let chunks_buf: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let last_partial_buf: Arc<parking_lot::Mutex<String>> =
        Arc::new(parking_lot::Mutex::new(String::new()));
    let committed_flag = Arc::new(AtomicBool::new(false));
    let key_fail_kind: Arc<parking_lot::Mutex<Option<FailKind>>> =
        Arc::new(parking_lot::Mutex::new(None));
    let chunks_for_task = Arc::clone(&chunks_buf);
    let last_partial_for_task = Arc::clone(&last_partial_buf);
    let committed_for_task = Arc::clone(&committed_flag);
    let key_fail_for_task = Arc::clone(&key_fail_kind);
    let release_pending_recv = Arc::clone(&release_pending);

    // Reset the live word counter at the start of every session.
    app.word_count.store(0, Ordering::Release);
    let recv_task = tokio::spawn(async move {
        let mut events: usize = 0;
        let mut committed_words: u32 = 0;
        #[allow(unused_assignments)]
        let mut partial_words = 0u32;
        loop {
            let ev = match stream.recv_event().await {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("session[{epoch}] recv error: {e}");
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
                    partial_words = t.split_whitespace().count() as u32;
                    recv_app
                        .word_count
                        .store(committed_words + partial_words, Ordering::Release);
                    *last_partial_for_task.lock() = t;
                }
                SttEvent::Committed(final_text) => {
                    committed_for_task.store(true, Ordering::Release);

                    // Drop the chunk entirely if a NEWER session has taken over.
                    if recv_app.current_session_epoch() != epoch {
                        tracing::debug!(
                            "session[{epoch}] dropping late commit (newer session active)"
                        );
                        continue;
                    }

                    let chunk_words = final_text.split_whitespace().count() as u32;
                    committed_words = committed_words.saturating_add(chunk_words);
                    recv_app
                        .word_count
                        .store(committed_words, Ordering::Release);

                    // Hybrid paste flow:
                    //   before release              -> HOLD (accumulate)
                    //   after release               -> LIVE (paste each chunk)
                    //   delay_until_release = false -> LIVE throughout
                    let released = release_pending_recv.load(Ordering::Acquire);
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
                        chunks_for_task.lock().push(final_text);
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
                        let _ = recv_app.transcript_tx.send(final_text);
                    }
                }
                SttEvent::KeyFailure(kind) => {
                    tracing::warn!("session[{epoch}] provider signaled key failure ({kind:?})");
                    *key_fail_for_task.lock() = Some(kind);
                    // Don't break: the outer wait loop observes key_fail_kind and
                    // tears the session down / rotates keys.
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
    if let Some(kind) = *key_fail_kind.lock() {
        tracing::warn!(
            "session[{epoch}] aborting finalize early -- key ...{key_suffix} failed ({kind:?})"
        );
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
            let _ = app.transcript_tx.send(joined);
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
    let chunks_sent = match tokio::time::timeout(send_deadline, send_task).await {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => {
            tracing::warn!("session[{epoch}] send_task join error: {e}");
            0
        }
        Err(_) => {
            tracing::warn!("session[{epoch}] send_task did not finish in {send_deadline:?}");
            0
        }
    };
    tracing::info!(
        "session[{epoch}] audio chunks sent = {chunks_sent} (~{} ms of audio)",
        chunks_sent * 100
    );

    // Wait up to FINAL_TRANSCRIPT_WAIT for recv to drain. If it doesn't finish
    // we abandon the JoinHandle, but the SHARED accumulators stay alive on this
    // stack, so anything already processed is still readable.
    let recv_finished = tokio::time::timeout(FINAL_TRANSCRIPT_WAIT, recv_task)
        .await
        .is_ok();
    if !recv_finished {
        tracing::warn!(
            "session[{epoch}] recv_task did not finish within {:?}; draining shared buffers anyway",
            FINAL_TRANSCRIPT_WAIT
        );
    }

    let got_committed = committed_flag.load(Ordering::Acquire);
    // Sweep once more in case recv pushed a chunk between us flipping
    // release_pending and taking the buffer.
    let held_chunks = std::mem::take(&mut *chunks_buf.lock());
    let last_partial = std::mem::take(&mut *last_partial_buf.lock());

    if !held_chunks.is_empty() {
        let joined = held_chunks.join(" ");
        if app.current_session_epoch() == epoch {
            tracing::info!(
                "session[{epoch}] flushing {} held commit chunk(s), {} chars total",
                held_chunks.len(),
                joined.chars().count()
            );
            let _ = app.transcript_tx.send(joined);
        } else {
            tracing::info!(
                "session[{epoch}] skipping held commit flush because a newer action superseded it"
            );
        }
    }

    let had_partial = !last_partial.is_empty();
    if !got_committed && had_partial && app.current_session_epoch() == epoch {
        if log_transcripts {
            tracing::info!("session[{epoch}] promoting last partial: {last_partial}");
        } else {
            tracing::info!(
                "session[{epoch}] promoting last partial: {} char(s)",
                last_partial.chars().count()
            );
        }
        let _ = app.transcript_tx.send(last_partial);
    } else if !got_committed && had_partial {
        tracing::info!(
            "session[{epoch}] skipping last partial because a newer action superseded it"
        );
    }
    if !got_committed && !had_partial && chunks_sent == 0 {
        tracing::warn!("session[{epoch}] produced no transcript (zero audio chunks sent -- session ended before mic was warm)");
    }

    // Happy path only reaches here (fast-fail returned above on failure).
    let key_failure = *key_fail_kind.lock();
    if let Some(kind) = key_failure {
        keys.mark_failed(&key, kind);
        tracing::warn!("session[{epoch}] ended with FAILED key ({kind:?}); pool will rotate");
    } else {
        keys.mark_success(&key, chunks_sent.saturating_mul(100) as u64);
    }
    crate::sound::play_stop(cfg.enable_sound);
    tracing::info!("session[{epoch}] ended");
    if key_failure.is_some() {
        return Err(anyhow!(EXHAUSTED_SIGNAL));
    }
    Ok(())
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
