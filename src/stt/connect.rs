//! Getting a session to the point where it can start streaming.
//!
//! Resolves the Per-App Profile overrides, picks the key pool, acquires a key,
//! subscribes to the audio pipeline, and connects the provider -- rotating
//! keys on the failures that are the key's fault.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::time::Instant;

use crate::config::Config;
use crate::keys::{FailKind, KeyPool};
use crate::state::{App, Status};

use super::dispatch::make_provider_id;
use super::provider::{AudioFormat, ProviderSession, ProviderSink, ProviderStream, SttSessionOpts};
use super::{CONNECT_TIMEOUT, ERROR_PIP_VISIBLE, EXHAUSTED_SIGNAL};

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

/// Everything [`establish_connected_session`] resolved and connected before
/// [`run_session`](super::run_session) can spawn the send/recv tasks: the config snapshot, the
/// (possibly per-app-profile overridden) provider id and key pool, the
/// acquired key, and the split provider connection. Bundled into one struct
/// because it's all produced together by one phase and consumed together by
/// the next.
pub(super) struct ConnectedSession {
    pub(super) cfg: Arc<Config>,
    pub(super) keys: Arc<KeyPool>,
    pub(super) key: String,
    pub(super) key_suffix: String,
    pub(super) requires_api_key: bool,
    pub(super) finalize_timeout: Duration,
    pub(super) final_transcript_timeout: Duration,
    pub(super) suppress_phantom: bool,
    pub(super) provider_id: &'static str,
    pub(super) fmt: AudioFormat,
    pub(super) samples_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
    pub(super) flusher: crate::audio::SessionFlusher,
    pub(super) sink: Box<dyn ProviderSink>,
    pub(super) stream: Box<dyn ProviderStream>,
}

/// If the global capture stream has died (mic unplugged, driver error), this
/// press would silently record nothing while hotkeys/tray/UI still look
/// alive. Surface the visible error pip and abort instead of pretending to
/// listen; the audio thread keeps retrying the device, so a later press
/// recovers. Returns `true` when it did (the caller must return `Ok(())`
/// without going any further).
pub(super) fn audio_capture_unhealthy(app: &Arc<App>, epoch: u64) -> bool {
    if app.audio.is_healthy() {
        return false;
    }
    tracing::error!(
        "session[{epoch}] aborted: audio capture is not running (microphone lost?) — device reopen is retried automatically"
    );
    if app.current_session_epoch() == epoch {
        // A lost mic is a generic error (the "!" pip), not a key problem.
        app.raise_error(crate::state::ErrorKind::Generic);
        let app_for_clear = Arc::clone(app);
        app.rt.spawn(async move {
            tokio::time::sleep(ERROR_PIP_VISIBLE).await;
            if app_for_clear.current_session_epoch() == epoch {
                app_for_clear.clear_status_if(Status::Error, Status::Idle);
            }
        });
    }
    true
}

/// Resolve config/per-app-profile overrides, acquire a key, and connect.
/// `Ok(None)` means the connection succeeded but the press it belongs to is
/// already superseded (a newer epoch started, or `stop` fired) -- the caller
/// returns `Ok(())` without spawning anything. Every `Err` arm already
/// rotates/marks the key exactly as the pre-extraction code did before
/// returning `EXHAUSTED_SIGNAL` (or, for a no-key-required provider, the raw
/// error).
pub(super) async fn establish_connected_session(
    app: &Arc<App>,
    keys: Arc<KeyPool>,
    stop: &Arc<AtomicBool>,
    epoch: u64,
) -> Result<Option<ConnectedSession>> {
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
    let ProviderSession { sink, stream } = match tokio::time::timeout(
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
        return Ok(None);
    }

    Ok(Some(ConnectedSession {
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
    }))
}
