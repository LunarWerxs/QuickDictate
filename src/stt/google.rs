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
}

impl GoogleWorker {
    async fn recognize(&self, pcm: &[i16]) -> Result<Option<String>, FailKind> {
        let content = base64::engine::general_purpose::STANDARD.encode(i16_slice_as_bytes(pcm));
        let body = json!({
            "config": {
                "encoding": "LINEAR16",
                "sampleRateHertz": self.sample_rate,
                "languageCode": self.language,
                "model": self.model,
            },
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
                    match self.recognize(&pcm).await {
                        Ok(Some(text)) => pending_events.push(SttEvent::Committed(text)),
                        Ok(None) => {}
                        Err(kind) => {
                            pending_events.push(SttEvent::KeyFailure(kind));
                            failed = true;
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
}
