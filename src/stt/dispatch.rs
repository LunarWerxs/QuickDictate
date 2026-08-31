//! Choosing a provider, and proving a key works before a session needs it.
//!
//! Provider construction from settings (including a Per-App Profile's
//! override), the startup key prewarm, and the Settings window's "Test keys".

use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::keys::{FailKind, KeyPool};
use crate::state::App;

use super::provider::{ProviderSession, SttEvent, SttProvider, SttSessionOpts};
use super::{assemblyai, dashscope, deepgram, elevenlabs, google, local, openai};

/// Build the provider selected in settings.json. Unknown ids fall back to
/// ElevenLabs (the baseline) with a warning. Providers are cheap unit structs,
/// rebuilt per session so a settings edit + restart cleanly switches backend.
fn make_provider(cfg: &Config) -> Box<dyn SttProvider> {
    make_provider_id(&cfg.stt_provider, cfg)
}

/// Build a provider by EXPLICIT id, so a Per-App Profile can select one that
/// differs from `cfg.stt_provider` (see `Config::provider_for_exe`).
pub(super) fn make_provider_id(id: &str, cfg: &Config) -> Box<dyn SttProvider> {
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
