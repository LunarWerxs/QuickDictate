//! Google Cloud Speech-to-Text adapter — **batch / non-streaming**.
//!
//! Google's streaming path needs a service-account + OAuth (no pasteable key),
//! so we ship the batch `v1/speech:recognize` REST endpoint instead: a plain
//! API key (`?key=`), record-then-send, no live partials, ~60 s per request.
//! It fits the streaming trait by *buffering*: `send_audio` appends to memory,
//! and `commit()` does the single HTTPS round-trip (segmenting audio longer
//! than ~55 s), emitting one `Committed` per segment through an internal channel
//! that the stream half drains.
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
use tokio::sync::mpsc;

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

    /// Batch: the POST happens in `commit()`, so the runner must wait longer
    /// than the streaming tail budget.
    fn finalize_timeout(&self) -> Duration {
        Duration::from_secs(45)
    }

    async fn connect(
        &self,
        key: &str,
        opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        // No socket — just wire up the buffering sink and its event channel.
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = GoogleSink {
            client: reqwest::Client::new(),
            key: key.to_string(),
            language: opts.language.clone(),
            model: opts
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            sample_rate: opts.sample_rate,
            buffer: Vec::new(),
            tx,
        };
        Ok(ProviderSession {
            sink: Box::new(sink),
            stream: Box::new(GoogleStream { rx }),
        })
    }
}

struct GoogleSink {
    client: reqwest::Client,
    key: String,
    language: String,
    model: String,
    sample_rate: u32,
    buffer: Vec<i16>,
    tx: mpsc::UnboundedSender<SttEvent>,
}

impl GoogleSink {
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
}

#[async_trait]
impl ProviderSink for GoogleSink {
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        // Pure buffering — no network, no partials.
        self.buffer.extend_from_slice(pcm);
        Ok(())
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        let buffer = std::mem::take(&mut self.buffer);
        if buffer.is_empty() {
            return Ok(());
        }
        let seg = (self.sample_rate as usize) * SEGMENT_SECS;
        for segment in buffer.chunks(seg) {
            match self.recognize(segment).await {
                Ok(Some(t)) => {
                    let _ = self.tx.send(SttEvent::Committed(t));
                }
                Ok(None) => {} // empty result for this segment
                Err(kind) => {
                    let _ = self.tx.send(SttEvent::KeyFailure(kind));
                    break;
                }
            }
        }
        Ok(())
    }

    async fn close(&mut self) -> Result<(), SendError> {
        // No-op: dropping the sink drops `tx`, which ends the stream.
        Ok(())
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
}
