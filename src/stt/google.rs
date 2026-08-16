//! Google Cloud Speech-to-Text adapter — **batch / non-streaming**.
//!
//! Google's streaming path needs a service-account + OAuth (no pasteable key),
//! so we ship the batch `v1/speech:recognize` REST endpoint instead: a plain
//! API key (`?key=`), no live partials, ~60 s per request. It fits the streaming
//! trait by buffering up to one 55 s segment and handing each completed segment
//! to a single background worker. This bounds raw-audio memory during long
//! dictations while preserving ordered, release-time-only transcript events.
//! `commit()` queues the final partial segment and waits for all requests.
//!
//! Compiled into every build, like the other five providers. This used to sit
//! behind a `--features google` gate to keep `reqwest` out of a streaming-only
//! build; the update checker made `reqwest` unconditional, which left the gate
//! buying ~39 KB while letting one source tree produce two different exes under
//! one filename. That shipped the wrong binary more than once, so the gate went.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use super::provider::{
    classify_by_substring, i16_slice_as_bytes, AudioFormat, ConnectError, ProviderSession,
    ProviderSink, ProviderStream, RecvError, SendError, SttEvent, SttProvider, SttSessionOpts,
};
use crate::keys::FailKind;

const RECOGNIZE_URL: &str = "https://speech.googleapis.com/v1/speech:recognize";
const DEFAULT_MODEL: &str = "latest_long";
/// Sync `recognize` accepts ≤ ~60 s of audio; segment below that with margin.
const SEGMENT_SECS: usize = 55;
/// Google's documented inline speech-adaptation ceiling: 5,000 phrases per
/// request (100 characters each). Our vocabulary is realistically far
/// smaller, but this keeps us honest with the documented limit.
const MAX_SPEECH_CONTEXT_PHRASES: usize = 5_000;
/// How many extra attempts a failed segment gets before we give up on it. A
/// transient network hiccup or 5xx on one 55-second segment must not cost the
/// rest of a long dictation.
const MAX_RETRIES: u32 = 2;
/// Exponential backoff base: 400 ms, then 800 ms.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(400);

pub struct GoogleProvider;

#[async_trait]
impl SttProvider for GoogleProvider {
    fn id(&self) -> &'static str {
        "google"
    }

    fn required_audio_format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: 16_000,
        }
    }

    /// Google wants a **full BCP-47** tag (`en-US`), not the region-stripped
    /// code the streaming providers use.
    fn language_for(&self, configured: &str) -> String {
        configured.to_string()
    }

    /// Batch: the final POST(s) are drained in `commit()`, so the runner must
    /// wait longer than the streaming tail budget.
    fn finalize_timeout(&self) -> Duration {
        Duration::from_secs(45)
    }

    async fn connect(
        &self,
        key: &str,
        opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        // One segment may wait behind the request in flight. Combined with the
        // sink's current partial segment, raw PCM stays bounded to roughly
        // three 55-second blocks even if Google's endpoint stalls.
        let (work_tx, work_rx) = mpsc::channel(1);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let worker = GoogleWorker {
            client: reqwest::Client::new(),
            key: key.to_string(),
            language: opts.language.clone(),
            model: opts
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            sample_rate: opts.sample_rate,
            vocabulary: opts.custom_vocabulary.clone(),
        };
        let worker_task = tokio::spawn(worker.run(work_rx, event_tx));

        let sink = GoogleSink {
            sample_rate: opts.sample_rate,
            buffer: Vec::new(),
            work_tx: Some(work_tx),
            worker_task: Some(worker_task),
        };
        Ok(ProviderSession {
            sink: Box::new(sink),
            stream: Box::new(GoogleStream { rx: event_rx }),
        })
    }
}

struct GoogleSink {
    sample_rate: u32,
    buffer: Vec<i16>,
    work_tx: Option<mpsc::Sender<GoogleCommand>>,
    worker_task: Option<tokio::task::JoinHandle<()>>,
}

enum GoogleCommand {
    Segment(Vec<i16>),
    Finish(oneshot::Sender<()>),
}

struct GoogleWorker {
    client: reqwest::Client,
    key: String,
    language: String,
    model: String,
    sample_rate: u32,
    /// Words/phrases to bias recognition toward, from `Config::custom_vocabulary`.
    /// Mapped onto `speechContexts[].phrases`; empty when the user set nothing.
    vocabulary: Vec<String>,
}

impl GoogleWorker {
    async fn recognize(&self, pcm: &[i16]) -> Result<Option<String>, FailKind> {
        let content = base64::engine::general_purpose::STANDARD.encode(i16_slice_as_bytes(pcm));
        let body = json!({
            "config": build_config(self.sample_rate, &self.language, &self.model, &self.vocabulary),
            "audio": { "content": content }
        });
        let url = format!("{RECOGNIZE_URL}?key={}", self.key);
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| classify_by_substring(&e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| classify_by_substring(&e.to_string()))?;
        if !status.is_success() {
            return Err(classify_by_substring(&format!(
                "HTTP {} {text}",
                status.as_u16()
            )));
        }
        parse_transcript(&text)
    }

    /// `recognize`, retried up to `MAX_RETRIES` extra times with exponential
    /// backoff before giving up on this segment. See `run` for what happens
    /// to the final classified error.
    async fn recognize_with_retry(&self, pcm: &[i16]) -> Result<Option<String>, FailKind> {
        retry_with_backoff(RETRY_BASE_DELAY, || self.recognize(pcm)).await
    }

    async fn run(
        self,
        mut work_rx: mpsc::Receiver<GoogleCommand>,
        event_tx: mpsc::UnboundedSender<SttEvent>,
    ) {
        // Hold results until Finish so Google retains its documented batch
        // behavior: even hour-long dictations never paste while the hotkey is
        // still held. Transcript text is tiny compared with the audio it
        // replaces (one event per 55 seconds).
        let mut pending_events = Vec::new();
        let mut failed = false;

        while let Some(command) = work_rx.recv().await {
            match command {
                GoogleCommand::Segment(pcm) => {
                    if failed {
                        continue;
                    }
                    match self.recognize_with_retry(&pcm).await {
                        Ok(Some(text)) => pending_events.push(SttEvent::Committed(text)),
                        Ok(None) => {}
                        Err(kind) => {
                            // A bad key or exhausted quota poisons every later
                            // segment too, so stop draining and bench the key.
                            // A transient blip or rate-limit on one segment
                            // must not do either -- surface it and keep going.
                            let (event, stop) = segment_failure_event(kind);
                            pending_events.push(event);
                            failed = stop;
                        }
                    }
                }
                GoogleCommand::Finish(done) => {
                    for event in pending_events {
                        let _ = event_tx.send(event);
                    }
                    let _ = done.send(());
                    break;
                }
            }
        }
    }
}

/// Retry `attempt` up to `MAX_RETRIES` extra times with exponential backoff
/// (`base_delay`, then `base_delay * 2`, ...) before giving up, returning
/// whatever the final attempt returned. Generic over the attempt closure so
/// the retry/backoff schedule is fixture-tested without a live HTTP request
/// (tests pass `Duration::ZERO` to run instantly).
async fn retry_with_backoff<F, Fut>(
    base_delay: Duration,
    mut attempt: F,
) -> Result<Option<String>, FailKind>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<String>, FailKind>>,
{
    let mut last_err = None;
    for i in 0..=MAX_RETRIES {
        match attempt().await {
            Ok(result) => return Ok(result),
            Err(kind) => {
                last_err = Some(kind);
                if i < MAX_RETRIES {
                    tokio::time::sleep(base_delay * 2u32.pow(i)).await;
                }
            }
        }
    }
    // MAX_RETRIES is a constant >= 0, so the loop always runs at least once and
    // every failing arm sets `last_err` -- the fallback is unreachable. It is a
    // value rather than an `.expect()` because this runs on a tokio worker,
    // where a panic takes the runtime down silently. `Transient` is the right
    // default: it retries later instead of condemning a healthy key.
    Err(last_err.unwrap_or(FailKind::Transient))
}

/// Decide what a segment's final (post-retry) failure means for the session:
/// a bad/exhausted key is worth benching (`KeyFailure`, and `run` stops
/// draining further segments since they'd fail too); a transient blip or
/// rate-limit is just this segment's bad luck (`ProviderFailure`, which does
/// not poison the key pool, and `run` keeps draining). Pure (fixture-tested).
fn segment_failure_event(kind: FailKind) -> (SttEvent, bool) {
    match kind {
        FailKind::Invalid | FailKind::Exhausted => (SttEvent::KeyFailure(kind), true),
        FailKind::Transient | FailKind::RateLimit => (
            SttEvent::ProviderFailure(format!(
                "Google speech:recognize failed for one segment after retries: {kind:?}"
            )),
            false,
        ),
    }
}

/// Build the `config` object for one `speech:recognize` request. Pure
/// (fixture-tested); `recognize` just calls this. `speechContexts[].phrases`
/// is added only when the vocabulary is non-empty, so a user with none set
/// gets the exact same request body as before this existed.
fn build_config(
    sample_rate: u32,
    language: &str,
    model: &str,
    vocabulary: &[String],
) -> serde_json::Value {
    let mut config = json!({
        "encoding": "LINEAR16",
        "sampleRateHertz": sample_rate,
        "languageCode": language,
        "model": model,
    });
    if !vocabulary.is_empty() {
        let phrases: Vec<&str> = vocabulary
            .iter()
            .take(MAX_SPEECH_CONTEXT_PHRASES)
            .map(String::as_str)
            .collect();
        config["speechContexts"] = json!([{ "phrases": phrases }]);
    }
    config
}

/// Move one complete prefix out without copying that 55-second prefix. Only
/// the small tail after the split is copied into a fresh allocation.
fn take_full_segment(buffer: &mut Vec<i16>, segment_samples: usize) -> Option<Vec<i16>> {
    if segment_samples == 0 || buffer.len() < segment_samples {
        return None;
    }
    let tail = buffer.split_off(segment_samples);
    Some(std::mem::replace(buffer, tail))
}

impl GoogleSink {
    async fn finish_worker(&mut self) -> Result<(), SendError> {
        let Some(work_tx) = self.work_tx.take() else {
            return Ok(());
        };

        let final_segment = std::mem::take(&mut self.buffer);
        if !final_segment.is_empty() {
            work_tx
                .send(GoogleCommand::Segment(final_segment))
                .await
                .map_err(|_| SendError("Google recognition worker stopped".into()))?;
        }

        let (done_tx, done_rx) = oneshot::channel();
        work_tx
            .send(GoogleCommand::Finish(done_tx))
            .await
            .map_err(|_| SendError("Google recognition worker stopped".into()))?;
        done_rx
            .await
            .map_err(|_| SendError("Google recognition worker stopped".into()))?;
        if let Some(worker_task) = self.worker_task.take() {
            worker_task
                .await
                .map_err(|e| SendError(format!("Google recognition worker failed: {e}")))?;
        }
        Ok(())
    }
}

impl Drop for GoogleSink {
    fn drop(&mut self) {
        // The runner can time out and abort its send task while reqwest is
        // awaiting a stalled endpoint. Keep the worker strictly session-owned
        // so that cancellation also releases its queued PCM and HTTP future.
        if let Some(worker_task) = self.worker_task.take() {
            worker_task.abort();
        }
    }
}

#[async_trait]
impl ProviderSink for GoogleSink {
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        let work_tx = self
            .work_tx
            .clone()
            .ok_or_else(|| SendError("Google recognition session is already finished".into()))?;
        self.buffer.extend_from_slice(pcm);
        let segment_samples = (self.sample_rate as usize) * SEGMENT_SECS;
        while let Some(segment) = take_full_segment(&mut self.buffer, segment_samples) {
            work_tx
                .send(GoogleCommand::Segment(segment))
                .await
                .map_err(|_| SendError("Google recognition worker stopped".into()))?;
        }
        Ok(())
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        self.finish_worker().await
    }

    async fn close(&mut self) -> Result<(), SendError> {
        // Normally commit already finished the worker. This also makes an
        // unusual close-without-commit drain safely instead of abandoning PCM.
        self.finish_worker().await
    }
}

struct GoogleStream {
    rx: mpsc::UnboundedReceiver<SttEvent>,
}

#[async_trait]
impl ProviderStream for GoogleStream {
    async fn recv_event(&mut self) -> Result<Option<SttEvent>, RecvError> {
        Ok(self.rx.recv().await)
    }
}

#[derive(Deserialize)]
struct GResp {
    results: Option<Vec<GResult>>,
    error: Option<GError>,
}

#[derive(Deserialize)]
struct GResult {
    alternatives: Option<Vec<GAlt>>,
}

#[derive(Deserialize)]
struct GAlt {
    transcript: Option<String>,
}

#[derive(Deserialize)]
struct GError {
    code: Option<i64>,
    message: Option<String>,
    status: Option<String>,
}

/// Pure body parser (fixture-tested): concatenated transcript, `None` if empty,
/// or a `FailKind` if the body carried an API error object.
fn parse_transcript(body: &str) -> Result<Option<String>, FailKind> {
    let parsed: GResp = serde_json::from_str(body).map_err(|_| FailKind::Transient)?;
    if let Some(err) = parsed.error {
        return Err(classify_by_substring(&format!(
            "{} {} {}",
            err.code.unwrap_or(0),
            err.status.unwrap_or_default(),
            err.message.unwrap_or_default()
        )));
    }
    let transcript = parsed
        .results
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            r.alternatives
                .and_then(|mut a| a.drain(..).next())
                .and_then(|alt| alt.transcript)
        })
        .collect::<Vec<_>>()
        .join(" ");
    let transcript = transcript.trim().to_string();
    Ok((!transcript.is_empty()).then_some(transcript))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_concatenated_results() {
        let body = r#"{"results":[
            {"alternatives":[{"transcript":"the quick brown fox","confidence":0.9}]},
            {"alternatives":[{"transcript":"testing one two three"}]}
        ]}"#;
        assert_eq!(
            parse_transcript(body).unwrap(),
            Some("the quick brown fox testing one two three".to_string())
        );
    }

    #[test]
    fn empty_results_is_none() {
        assert_eq!(parse_transcript(r#"{"results":[]}"#).unwrap(), None);
        assert_eq!(parse_transcript(r#"{}"#).unwrap(), None);
    }

    #[test]
    fn error_object_maps_to_failkind() {
        let body =
            r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"API key not valid"}}"#;
        assert!(matches!(parse_transcript(body), Err(FailKind::Invalid)));
        let quota =
            r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Quota exceeded"}}"#;
        assert!(matches!(parse_transcript(quota), Err(FailKind::Exhausted)));
    }

    #[test]
    fn completed_segments_are_removed_and_tail_is_preserved() {
        let mut buffer: Vec<i16> = (0..12).collect();
        assert_eq!(take_full_segment(&mut buffer, 5), Some(vec![0, 1, 2, 3, 4]));
        assert_eq!(take_full_segment(&mut buffer, 5), Some(vec![5, 6, 7, 8, 9]));
        assert_eq!(take_full_segment(&mut buffer, 5), None);
        assert_eq!(buffer, vec![10, 11]);
    }

    #[test]
    fn empty_vocabulary_config_is_unchanged() {
        let config = build_config(16_000, "en-US", DEFAULT_MODEL, &[]);
        assert!(config.get("speechContexts").is_none());
        assert_eq!(config["languageCode"], "en-US");
        assert_eq!(config["model"], DEFAULT_MODEL);
        assert_eq!(config["encoding"], "LINEAR16");
        assert_eq!(config["sampleRateHertz"], 16_000);
    }

    #[test]
    fn vocabulary_adds_speech_contexts() {
        let vocab = vec!["Anthropic".to_string(), "QuickDictate".to_string()];
        let config = build_config(16_000, "en-US", DEFAULT_MODEL, &vocab);
        assert_eq!(
            config["speechContexts"],
            serde_json::json!([{ "phrases": ["Anthropic", "QuickDictate"] }])
        );
    }

    #[test]
    fn speech_context_phrases_are_capped_at_documented_limit() {
        let vocab: Vec<String> = (0..(MAX_SPEECH_CONTEXT_PHRASES + 10))
            .map(|i| format!("term{i}"))
            .collect();
        let config = build_config(16_000, "en-US", DEFAULT_MODEL, &vocab);
        let phrases = config["speechContexts"][0]["phrases"].as_array().unwrap();
        assert_eq!(phrases.len(), MAX_SPEECH_CONTEXT_PHRASES);
    }

    // Task C: retry-then-continue instead of one bad segment poisoning the
    // whole session and the key pool.

    #[tokio::test]
    async fn transient_failure_retries_and_eventually_succeeds() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let attempts = AtomicU32::new(0);
        let result = retry_with_backoff(Duration::ZERO, || {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < MAX_RETRIES {
                    Err(FailKind::Transient)
                } else {
                    Ok(Some("hello world".to_string()))
                }
            }
        })
        .await;
        assert_eq!(result, Ok(Some("hello world".to_string())));
        // The initial attempt plus MAX_RETRIES retries.
        assert_eq!(attempts.load(Ordering::SeqCst), MAX_RETRIES + 1);
    }

    #[tokio::test]
    async fn retries_are_exhausted_and_final_error_is_returned() {
        let result: Result<Option<String>, FailKind> =
            retry_with_backoff(Duration::ZERO, || async { Err(FailKind::Transient) }).await;
        assert_eq!(result, Err(FailKind::Transient));
    }

    #[test]
    fn transient_and_rate_limit_emit_provider_failure_and_keep_draining() {
        let (event, stop) = segment_failure_event(FailKind::Transient);
        assert!(matches!(event, SttEvent::ProviderFailure(_)));
        assert!(!stop);
        let (event, stop) = segment_failure_event(FailKind::RateLimit);
        assert!(matches!(event, SttEvent::ProviderFailure(_)));
        assert!(!stop);
    }

    #[test]
    fn invalid_and_exhausted_emit_key_failure_and_stop_draining() {
        let (event, stop) = segment_failure_event(FailKind::Invalid);
        assert!(matches!(event, SttEvent::KeyFailure(FailKind::Invalid)));
        assert!(stop);
        let (event, stop) = segment_failure_event(FailKind::Exhausted);
        assert!(matches!(event, SttEvent::KeyFailure(FailKind::Exhausted)));
        assert!(stop);
    }
}
