//! Live provider round-trip tests.
//!
//! These exercise each adapter against the **real** provider API: canned TTS
//! speech (`tests/fixtures/speech_16k.wav`, known phrase) → `connect` →
//! `send_audio` (chunked) → `commit`/`close` → collect `recv_event` → assert
//! the transcript contains the expected words. This is the automated stand-in
//! for the old "speak into a mic and watch it paste" manual test.
//!
//! All tests are `#[ignore]` (they need network + real keys), so plain
//! `cargo test` skips them. Run explicitly:
//!
//! ```text
//! cargo test --test-threads=1 -- --ignored --nocapture live_elevenlabs
//! cargo test -- --ignored --nocapture            # all providers
//! ```
//!
//! Keys are read from the gitignored `my.keys.env`; a fixture/key that isn't
//! present makes the test skip (print + return) rather than fail.

use std::time::Duration;

use super::provider::{ProviderSession, SttEvent, SttProvider, SttSessionOpts};

const KEYS_ENV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/my.keys.env");
const WAV_16K: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/speech_16k.wav");
#[allow(dead_code)] // used by the OpenAI 24 kHz live test
const WAV_24K: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/speech_24k.wav");

/// Read the comma-separated keys for a provider out of `my.keys.env`. Returns
/// empty if the file or line is missing (test then skips).
fn keys_for(provider: &str) -> Vec<String> {
    let var = format!("{}_KEYS", provider.to_ascii_uppercase());
    let Ok(contents) = std::fs::read_to_string(KEYS_ENV) else {
        return Vec::new();
    };
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix(&format!("{var}=")) {
            return rest
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }
    Vec::new()
}

/// Load a mono PCM16 WAV fixture into i16 samples.
fn load_wav(path: &str) -> Option<Vec<i16>> {
    let reader = hound::WavReader::open(path).ok()?;
    let spec = reader.spec();
    assert_eq!(spec.channels, 1, "fixture must be mono");
    assert_eq!(spec.bits_per_sample, 16, "fixture must be 16-bit");
    Some(
        reader
            .into_samples::<i16>()
            .filter_map(|s| s.ok())
            .collect(),
    )
}

/// Drive one provider end-to-end and return the transcript it produced.
async fn probe(provider: &dyn SttProvider, key: &str, samples: Vec<i16>) -> anyhow::Result<String> {
    let fmt = provider.required_audio_format();
    let opts = SttSessionOpts {
        language: provider.language_for("en-US"),
        sample_rate: fmt.sample_rate,
        model: None,
    };
    let ProviderSession { sink, mut stream } = provider
        .connect(key, &opts)
        .await
        .map_err(|e| anyhow::anyhow!("connect: {e}"))?;

    // Send task: stream 100 ms chunks (1600 samples @ 16 kHz), paced a few×
    // faster than realtime, then commit + close.
    let send = tokio::spawn(async move {
        let mut sink = sink;
        for chunk in samples.chunks(1600) {
            if sink.send_audio(chunk).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = sink.commit().await;
        let _ = sink.close().await;
    });

    let mut committed = String::new();
    let mut last_partial = String::new();
    let hard_deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    // Once a final chunk lands we only linger briefly for more (multi-segment),
    // rather than waiting out the hard deadline — OpenAI keeps the socket open.
    let mut deadline = hard_deadline;
    loop {
        let ev = tokio::select! {
            r = stream.recv_event() => r,
            _ = tokio::time::sleep_until(deadline) => { break; }
        };
        match ev {
            Ok(Some(SttEvent::Committed(t))) => {
                if !committed.is_empty() {
                    committed.push(' ');
                }
                committed.push_str(&t);
                deadline =
                    (tokio::time::Instant::now() + Duration::from_millis(1500)).min(hard_deadline);
            }
            Ok(Some(SttEvent::Partial(t))) => last_partial = t,
            Ok(Some(SttEvent::KeyFailure(k))) => {
                return Err(anyhow::anyhow!("provider signaled key failure: {k:?}"))
            }
            Ok(Some(SttEvent::Closed(_))) | Ok(None) => break,
            Ok(Some(SttEvent::SessionStarted)) => {}
            Err(e) => {
                // Some providers (ElevenLabs) reset the TCP right after the
                // final transcript instead of a clean WS close. Production
                // treats a recv error as a graceful end; mirror that and keep
                // whatever we accumulated (an auth failure yields no transcript,
                // which the assertion still catches).
                eprintln!("  (recv ended with transport error: {e})");
                break;
            }
        }
    }
    let _ = send.await;
    Ok(if committed.trim().is_empty() {
        last_partial
    } else {
        committed
    })
}

/// Assert the transcript looks like our known phrase (robust to STT variation).
fn assert_recognized(provider: &str, transcript: &str) {
    let lower = transcript.to_lowercase();
    let hits = [
        "quick", "brown", "fox", "lazy", "dog", "testing", "three", "jumps", "over",
    ]
    .iter()
    .filter(|w| lower.contains(**w))
    .count();
    println!("[{provider}] transcript = {transcript:?}  ({hits} keyword hits)");
    assert!(
        !transcript.trim().is_empty(),
        "[{provider}] empty transcript"
    );
    assert!(
        hits >= 2,
        "[{provider}] too few expected words in transcript: {transcript:?}"
    );
}

/// True if an error looks like a per-key auth/billing/rate problem (the app
/// would rotate to the next key) rather than an adapter/network bug (which
/// should fail the test loudly).
fn is_key_problem(msg: &str) -> bool {
    let l = msg.to_lowercase();
    [
        "arrearage",
        "good standing",
        "access denied",
        "insufficient",
        "quota",
        "billing",
        "balance",
        "payment",
        "exhausted",
        "unauthorized",
        "invalid",
        "401",
        "403",
        "rate limit",
        "too many",
        "key failure",
    ]
    .iter()
    .any(|m| l.contains(m))
}

/// Shared runner: rotate through the provider's keys (skipping billing/auth-dead
/// ones, exactly like the real KeyPool) and assert the first working key
/// transcribes the fixture. Skips (not fails) if every key is unusable.
async fn run_live(provider_id: &str, provider: Box<dyn SttProvider>) {
    let keys = keys_for(provider_id);
    if keys.is_empty() {
        eprintln!("[{provider_id}] SKIP: no key in my.keys.env");
        return;
    }
    // Feed the fixture that matches the provider's required rate (OpenAI = 24 kHz).
    let wav = if provider.required_audio_format().sample_rate >= 24_000 {
        WAV_24K
    } else {
        WAV_16K
    };
    let Some(samples) = load_wav(wav) else {
        eprintln!("[{provider_id}] SKIP: missing {wav} (run scripts/gen_test_audio.ps1)");
        return;
    };
    let mut last = String::new();
    for (i, key) in keys.iter().enumerate() {
        match probe(provider.as_ref(), key, samples.clone()).await {
            Ok(t) => {
                assert_recognized(provider_id, &t);
                return;
            }
            Err(e) if is_key_problem(&e.to_string()) => {
                eprintln!("[{provider_id}] key #{i} unusable (rotating): {e}");
                last = e.to_string();
            }
            Err(e) => panic!("[{provider_id}] probe failed (not a key problem): {e:#}"),
        }
    }
    eprintln!(
        "[{provider_id}] SKIP: all {} key(s) unusable; last: {last}",
        keys.len()
    );
}

#[tokio::test]
#[ignore = "live network + real key"]
async fn live_elevenlabs() {
    run_live(
        "elevenlabs",
        Box::new(super::elevenlabs::ElevenLabsProvider),
    )
    .await;
}

#[tokio::test]
#[ignore = "live network + real key"]
async fn live_deepgram() {
    run_live("deepgram", Box::new(super::deepgram::DeepgramProvider)).await;
}

#[tokio::test]
#[ignore = "live network + real key"]
async fn live_assemblyai() {
    run_live(
        "assemblyai",
        Box::new(super::assemblyai::AssemblyAiProvider),
    )
    .await;
}

#[tokio::test]
#[ignore = "live network + real key"]
async fn live_dashscope() {
    run_live(
        "dashscope",
        Box::new(super::dashscope::DashScopeProvider { intl: false }),
    )
    .await;
}

#[tokio::test]
#[ignore = "live network + real key"]
async fn live_openai() {
    run_live("openai", Box::new(super::openai::OpenAiProvider)).await;
}

#[cfg(feature = "google")]
#[tokio::test]
#[ignore = "live network + real key"]
async fn live_google() {
    run_live("google", Box::new(super::google::GoogleProvider)).await;
}
