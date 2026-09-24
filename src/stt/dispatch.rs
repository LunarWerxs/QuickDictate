//! Choosing a provider, and proving a key works before a session needs it.
//!
//! Provider construction from settings (including a Per-App Profile's
//! override), the startup key prewarm, and the Settings window's "Test keys".

use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::key_checks;
use crate::keys::{FailKind, KeyPool};
use crate::state::App;

use super::provider::{ProviderSession, ProviderStream, SttEvent, SttProvider, SttSessionOpts};
use super::{assemblyai, dashscope, deepgram, elevenlabs, google, local, openai, CONNECT_TIMEOUT};

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

/// Whether the provider `cfg` selects streams interim transcripts while the
/// user talks (see [`SttProvider::streams_interim_text`]). The pip asks this
/// to choose between its live word count and the spinner; asking the provider
/// is what keeps that choice from drifting as adapters are added, which a
/// hardcoded `== "local"` in the UI had already done -- Google and OpenAI
/// both sat on a frozen "0".
pub fn provider_streams_interim_text(cfg: &Config) -> bool {
    make_provider(cfg).streams_interim_text()
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
        let mut alive = Vec::new();
        for key in list {
            let verdict = probe_key(provider.as_ref(), &key, &opts).await;
            match verdict {
                Ok(()) => {
                    keys.mark_alive_probe(&key);
                    alive.push(key);
                }
                Err(kind) => keys.mark_failed(&key, kind),
            }
        }
        check_new_accounts(provider.as_ref(), &keys, &alive, &opts).await;
        tracing::info!("prewarm: done — {}", keys.summary());
    });
}

/// Prewarm half of the account check: run it on every key the connect probe
/// just passed that has never passed it before, all at once, and bench the
/// ones whose account the provider rejects. A key found this way is marked
/// failed exactly as if a press had been cut off by it, so it is never handed
/// to a press, where it would have cost everything said after ten seconds.
async fn check_new_accounts(
    provider: &dyn SttProvider,
    keys: &KeyPool,
    alive: &[String],
    opts: &SttSessionOpts,
) {
    let Some(audio) = provider.account_check_audio() else {
        return;
    };
    let provider_id = provider.id();
    let due: Vec<&String> = alive
        .iter()
        .filter(|key| !key_checks::has_passed(provider_id, key))
        .collect();
    if due.is_empty() {
        return;
    }
    tracing::info!(
        "prewarm: account check on {} {provider_id} key(s) not checked before",
        due.len()
    );
    let verdicts = futures_util::future::join_all(
        due.iter()
            .map(|key| check_account(provider, key, opts, audio)),
    )
    .await;
    for (key, verdict) in due.into_iter().zip(verdicts) {
        let label = keys.label(key);
        match verdict {
            AccountVerdict::Passed => {
                key_checks::record_pass(provider_id, key);
                tracing::info!("key {label} passed the account check");
            }
            AccountVerdict::Failed(kind) => {
                tracing::warn!(
                    "key {label} failed the account check ({kind:?}); benched so no press uses it"
                );
                keys.mark_failed(key, kind);
            }
            AccountVerdict::Inconclusive(why) => {
                tracing::info!(
                    "key {label}: account check inconclusive ({why}); next launch tries again"
                );
            }
        }
    }
}

/// What [`check_account`] concluded about a key's account.
#[derive(Debug, PartialEq)]
pub(super) enum AccountVerdict {
    /// The provider took the whole stream without a word against the key.
    Passed,
    /// The provider rejected the key (for ElevenLabs: `unaccepted_terms`).
    Failed(FailKind),
    /// Nothing either way: a network hiccup or a close that blamed nothing.
    Inconclusive(&'static str),
}

/// Chunks of [`account_chunk`] go out this far apart: 100 ms of audio every
/// 10 ms, ten times real time. ElevenLabs counts audio, not wall-clock time,
/// so a rejected account is closed about a second in rather than ten.
const ACCOUNT_CHECK_PACE: Duration = Duration::from_millis(10);

/// How long to keep listening once the last chunk is out. The rejection
/// trails the tenth second of audio by well under a second (measured).
const ACCOUNT_CHECK_LISTEN: Duration = Duration::from_secs(3);

/// 100 ms of faint noise, about -40 dBFS: the shape the check was measured
/// with against ElevenLabs (good keys answered it with nothing at all).
fn account_chunk(sample_rate: u32) -> Vec<i16> {
    (0..sample_rate / 10)
        .map(|i| ((i * 7919) % 601) as i16 - 300)
        .collect()
}

/// Stream `audio` worth of [`account_chunk`]s into one session and listen for
/// the provider rejecting the key's account on the way. See
/// [`SttProvider::account_check_audio`] for which providers need this, and
/// `key_checks` for why a pass is remembered.
pub(super) async fn check_account(
    provider: &dyn SttProvider,
    key: &str,
    opts: &SttSessionOpts,
    audio: Duration,
) -> AccountVerdict {
    let ProviderSession {
        mut sink,
        mut stream,
    } = match connect_probe(provider, key, opts).await {
        Ok(session) => session,
        Err(None) => return AccountVerdict::Inconclusive("connect timed out"),
        Err(Some(FailKind::Transient)) => return AccountVerdict::Inconclusive("connect failed"),
        Err(Some(kind)) => return AccountVerdict::Failed(kind),
    };
    let chunk = account_chunk(opts.sample_rate);
    let chunks = audio.as_millis() / 100;
    // `Some(outcome)` if the stream settled it first; `None` if the whole
    // stream went out and the listening window passed without a word.
    let heard = tokio::select! {
        heard = first_key_failure(stream.as_mut()) => Some(heard),
        () = async {
            for _ in 0..chunks {
                // A refused send means the server closed the socket; the
                // stream arm reports why.
                if sink.send_audio(&chunk).await.is_err() {
                    break;
                }
                tokio::time::sleep(ACCOUNT_CHECK_PACE).await;
            }
            tokio::time::sleep(ACCOUNT_CHECK_LISTEN).await;
        } => None,
    };
    let _ = sink.close().await;
    match heard {
        None => AccountVerdict::Passed,
        Some(Some(FailKind::Transient)) | Some(None) => {
            AccountVerdict::Inconclusive("the connection ended without a verdict")
        }
        Some(Some(kind)) => AccountVerdict::Failed(kind),
    }
}

/// "Test keys" half of the account check: true unless the provider rejects
/// the key's account. A key that passed before is not checked again, and an
/// inconclusive check does not fail a key the connect probe just passed.
async fn passes_account_check(
    provider: &dyn SttProvider,
    key: &str,
    opts: &SttSessionOpts,
) -> bool {
    let Some(audio) = provider.account_check_audio() else {
        return true;
    };
    if key_checks::has_passed(provider.id(), key) {
        return true;
    }
    match check_account(provider, key, opts, audio).await {
        AccountVerdict::Passed => {
            key_checks::record_pass(provider.id(), key);
            true
        }
        AccountVerdict::Failed(_) => false,
        AccountVerdict::Inconclusive(_) => true,
    }
}

/// Settings-window "Test keys": probe `keys_to_test` against `cfg`'s selected
/// provider, all keys **in parallel**, invoking `on_result(key, ok)` as each
/// verdict lands. Purely diagnostic — does not touch the live KeyPool (a
/// passed account check is remembered in `key_checks`, nothing else).
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
            let ok = probe_key(provider.as_ref(), &key, &opts).await.is_ok()
                && passes_account_check(provider.as_ref(), &key, &opts).await;
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
    let ProviderSession {
        mut sink,
        mut stream,
    } = match connect_probe(provider, key, opts).await {
        Ok(session) => session,
        // A timeout is the network's doing, not the key's.
        Err(refused) => return Err(refused.unwrap_or(FailKind::Transient)),
    };
    // ~0.1 s of silence: harmless for streaming providers (no VAD trigger),
    // and gives batch providers a body to submit.
    let silence = vec![0i16; (opts.sample_rate / 10) as usize];
    let _ = sink.send_audio(&silence).await;
    if provider.id() == "google" {
        // Batch: the key is only exercised by the recognize POST in commit().
        let _ = sink.commit().await;
    }
    let listen = tokio::time::timeout(
        Duration::from_millis(1500),
        first_key_failure(stream.as_mut()),
    );
    match listen.await {
        Ok(Some(kind)) => Err(kind),
        // Timeout (quiet stream) or clean close: the provider accepted the key.
        _ => {
            let _ = sink.close().await;
            Ok(())
        }
    }
}

/// Connect for a key probe or an account check, under the same bound a
/// press's own connect has ([`CONNECT_TIMEOUT`]). `Err(None)` is a handshake
/// that timed out, which is the network's doing and says nothing about the
/// key; `Err(Some(kind))` is the provider refusing it, classified.
async fn connect_probe(
    provider: &dyn SttProvider,
    key: &str,
    opts: &SttSessionOpts,
) -> Result<ProviderSession, Option<FailKind>> {
    match tokio::time::timeout(CONNECT_TIMEOUT, provider.connect(key, opts)).await {
        Ok(Ok(session)) => Ok(session),
        Ok(Err(e)) => Err(Some(provider.classify_connect_error(&e))),
        Err(_) => Err(None),
    }
}

/// Read `stream` until the provider blames the key, and say how. `None` once
/// the stream ends, cleanly or not, without doing so. Everything else
/// (session start, partials, commits) is not a verdict and is skipped.
async fn first_key_failure(stream: &mut dyn ProviderStream) -> Option<FailKind> {
    loop {
        match stream.recv_event().await {
            Ok(Some(SttEvent::KeyFailure(kind))) => return Some(kind),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_provider(id: &str) -> Config {
        Config {
            stt_provider: id.into(),
            ..Config::default()
        }
    }

    /// The pip's choice between a live word count and the spinner, for every
    /// provider the app can be set to. A new adapter that answers only at
    /// commit has to be added here (and override the trait method) or its
    /// users watch a "0" that never moves.
    #[test]
    fn only_the_commit_only_providers_hide_the_live_word_count() {
        for id in ["elevenlabs", "deepgram", "assemblyai", "dashscope"] {
            assert!(
                provider_streams_interim_text(&with_provider(id)),
                "{id} streams partials, so the pip should count words"
            );
        }
        for id in ["google", "local", "openai"] {
            assert!(
                !provider_streams_interim_text(&with_provider(id)),
                "{id} answers only after commit, so the pip should spin"
            );
        }
        // An unknown id falls back to ElevenLabs, which does stream.
        assert!(provider_streams_interim_text(&with_provider("nonsense")));
    }

    fn check_opts() -> SttSessionOpts {
        SttSessionOpts {
            language: "en".into(),
            sample_rate: 16_000,
            model: None,
            custom_vocabulary: Vec::new(),
        }
    }

    const TWELVE_SECONDS: Duration = Duration::from_secs(12);

    #[tokio::test(start_paused = true)]
    async fn an_account_that_takes_the_whole_stream_passes() {
        use std::sync::atomic::Ordering;
        let provider = super::super::mock::MockProvider {
            hold_open: true,
            ..Default::default()
        };
        let verdict = check_account(&provider, "k", &check_opts(), TWELVE_SECONDS).await;
        assert_eq!(verdict, AccountVerdict::Passed);
        assert_eq!(
            provider.sent_chunks.load(Ordering::Acquire),
            120,
            "twelve seconds of audio in 100 ms chunks"
        );
    }

    /// The 2026-09-18 keys: ElevenLabs closed the session with
    /// `unaccepted_terms`, which the adapter reports as an Invalid key.
    #[tokio::test(start_paused = true)]
    async fn an_account_the_provider_rejects_fails() {
        let provider = super::super::mock::MockProvider {
            script: vec![
                SttEvent::SessionStarted,
                SttEvent::KeyFailure(FailKind::Invalid),
            ],
            ..Default::default()
        };
        let verdict = check_account(&provider, "k", &check_opts(), TWELVE_SECONDS).await;
        assert_eq!(verdict, AccountVerdict::Failed(FailKind::Invalid));
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_just_ends_proves_nothing() {
        let provider = super::super::mock::MockProvider::default();
        let verdict = check_account(&provider, "k", &check_opts(), TWELVE_SECONDS).await;
        assert!(matches!(verdict, AccountVerdict::Inconclusive(_)));
        // A transient complaint blames the network, not the account.
        let provider = super::super::mock::MockProvider {
            script: vec![SttEvent::KeyFailure(FailKind::Transient)],
            hold_open: true,
            ..Default::default()
        };
        let verdict = check_account(&provider, "k", &check_opts(), TWELVE_SECONDS).await;
        assert!(matches!(verdict, AccountVerdict::Inconclusive(_)));
    }

    /// The startup / "Test keys" probe: a key the provider blames fails with
    /// the provider's own verdict; one it just listens to passes.
    #[tokio::test(start_paused = true)]
    async fn a_probe_fails_only_the_keys_the_provider_blames() {
        let quiet = super::super::mock::MockProvider {
            script: vec![SttEvent::SessionStarted, SttEvent::Partial("hm".into())],
            hold_open: true,
            ..Default::default()
        };
        assert_eq!(probe_key(&quiet, "k", &check_opts()).await, Ok(()));
        let refused = super::super::mock::MockProvider {
            script: vec![
                SttEvent::SessionStarted,
                SttEvent::KeyFailure(FailKind::Exhausted),
            ],
            hold_open: true,
            ..Default::default()
        };
        assert_eq!(
            probe_key(&refused, "k", &check_opts()).await,
            Err(FailKind::Exhausted)
        );
    }

    /// Which providers the stall watchdog may reconnect, each measured against
    /// its live API: a partial within a couple of seconds and a steady cadence.
    #[test]
    fn stall_recovery_is_on_exactly_for_the_measured_streaming_providers() {
        for id in ["elevenlabs", "deepgram", "assemblyai", "dashscope"] {
            assert!(
                make_provider(&with_provider(id)).supports_stall_recovery(),
                "{id} streams partials fast enough to be watched"
            );
        }
        for id in ["google", "local", "openai"] {
            assert!(
                !make_provider(&with_provider(id)).supports_stall_recovery(),
                "{id} says nothing until commit; every sentence would look like a stall"
            );
        }
    }

    #[test]
    fn only_elevenlabs_needs_an_account_check() {
        for id in [
            "deepgram",
            "assemblyai",
            "dashscope",
            "google",
            "local",
            "openai",
        ] {
            assert!(
                make_provider(&with_provider(id))
                    .account_check_audio()
                    .is_none(),
                "{id} would spend quota on a check it does not need"
            );
        }
        let audio = make_provider(&with_provider("elevenlabs")).account_check_audio();
        assert!(
            audio.is_some_and(|a| a > Duration::from_secs(11)),
            "ElevenLabs rejects at about ten seconds of audio"
        );
    }
}
