//! ElevenLabs Scribe v2 realtime adapter.
//!
//! Extracted **verbatim in behavior** from the original hardcoded `stt.rs`:
//! same WS URL/model, `xi-api-key` header, base64-JSON audio envelope, manual
//! commit + pre-close delay, and `message_type` → event mapping. Nothing about
//! the ElevenLabs wire protocol lives outside this file.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::provider::{
    AudioFormat, ConnectError, Encoding, ProviderSession, ProviderSink, ProviderStream, RecvError,
    SendError, SttEvent, SttProvider, SttSessionOpts,
};
use crate::keys::FailKind;

const WS_URL: &str = "wss://api.elevenlabs.io/v1/speech-to-text/realtime";
const MODEL_ID: &str = "scribe_v2_realtime";

/// How long the send task waits after the manual commit before sending the
/// WebSocket Close. Some servers race Close against in-flight commit
/// processing and skip the committed_transcript response. 300 ms gives
/// ElevenLabs enough time to flush the final transcript.
const PRE_CLOSE_DELAY: Duration = Duration::from_millis(300);

type WsSink = futures_util::stream::SplitSink<
    WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream =
    futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>;

pub struct ElevenLabsProvider;

#[async_trait]
impl SttProvider for ElevenLabsProvider {
    fn id(&self) -> &'static str {
        "elevenlabs"
    }

    fn required_audio_format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: 16_000,
            encoding: Encoding::Pcm16Le,
        }
    }

    /// Scribe's LM prior finalizes a trailing question into a phantom short
    /// "answer" ("Yes.") at end-of-stream. Enable the runner's guard so those
    /// zero-speech post-release commits are dropped instead of pasted. See
    /// `SCRIBE_HALLUCINATION_HANDOFF.md`.
    fn suppress_phantom_finalization(&self) -> bool {
        true
    }

    async fn connect(
        &self,
        key: &str,
        opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        let model = opts.model.as_deref().unwrap_or(MODEL_ID);
        let url = format!(
            "{WS_URL}?language_code={lang}&model_id={model}&audio_format=pcm_16000&commit_strategy=vad",
            lang = opts.language,
        );
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| ConnectError(format!("ws request: {e}")))?;
        request.headers_mut().insert(
            "xi-api-key",
            HeaderValue::from_str(key).map_err(|e| ConnectError(format!("bad key header: {e}")))?,
        );
        let (ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| ConnectError(format!("ws connect failed: {e}")))?;
        let (sink, stream) = ws.split();
        Ok(ProviderSession {
            sink: Box::new(ElevenLabsSink {
                sink,
                buf: String::with_capacity(8192),
                engine: base64::engine::general_purpose::STANDARD,
                sample_rate: opts.sample_rate,
            }),
            stream: Box::new(ElevenLabsStream {
                stream,
                closed: false,
            }),
        })
    }
}

struct ElevenLabsSink {
    sink: WsSink,
    /// Reused across chunks to avoid a per-frame allocation, exactly as the
    /// original `ship()` did.
    buf: String,
    engine: base64::engine::general_purpose::GeneralPurpose,
    sample_rate: u32,
}

#[async_trait]
impl ProviderSink for ElevenLabsSink {
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        let bytes = super::provider::i16_slice_as_bytes(pcm);
        self.buf.clear();
        self.buf
            .push_str("{\"message_type\":\"input_audio_chunk\",\"audio_base_64\":\"");
        self.engine.encode_string(bytes, &mut self.buf);
        self.buf.push_str("\",\"sample_rate\":");
        self.buf.push_str(&self.sample_rate.to_string());
        self.buf.push('}');
        self.sink
            .send(Message::Text(self.buf.clone()))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        let commit = json!({
            "message_type": "input_audio_chunk",
            "audio_base_64": "",
            "sample_rate": self.sample_rate,
            "commit": true,
        })
        .to_string();
        self.sink
            .send(Message::Text(commit))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn keepalive(&mut self) -> Result<(), SendError> {
        // An empty audio chunk with NO commit: zero samples (so nothing to
        // transcribe and no VAD trigger) but a real message on the audio channel,
        // which resets the server's idle timer. Same envelope the server already
        // accepts for `commit`, minus the samples and the commit flag.
        let ka = json!({
            "message_type": "input_audio_chunk",
            "audio_base_64": "",
            "sample_rate": self.sample_rate,
        })
        .to_string();
        self.sink
            .send(Message::Text(ka))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn close(&mut self) -> Result<(), SendError> {
        // Match the original: give ElevenLabs a beat to flush the final
        // transcript before the Close races in.
        tokio::time::sleep(PRE_CLOSE_DELAY).await;
        self.sink
            .send(Message::Close(None))
            .await
            .map_err(|e| SendError(e.to_string()))
    }
}

struct ElevenLabsStream {
    stream: WsStream,
    /// Set once we've observed the transport close so the next `recv_event`
    /// reports end-of-stream.
    closed: bool,
}

#[async_trait]
impl ProviderStream for ElevenLabsStream {
    async fn recv_event(&mut self) -> Result<Option<SttEvent>, RecvError> {
        if self.closed {
            return Ok(None);
        }
        loop {
            let msg = match self.stream.next().await {
                Some(m) => m,
                None => {
                    self.closed = true;
                    return Ok(None);
                }
            };
            let text = match msg {
                Ok(Message::Text(t)) => t.to_string(),
                Ok(Message::Close(c)) => {
                    self.closed = true;
                    let reason = c.as_ref().map(|f| f.reason.to_string()).unwrap_or_default();
                    // A billing/quota close reason means the key is exhausted;
                    // surface that as a KeyFailure (the runner then rotates).
                    if is_account_exhausted(&reason) {
                        return Ok(Some(SttEvent::KeyFailure(FailKind::Exhausted)));
                    }
                    return Ok(Some(SttEvent::Closed(
                        (!reason.is_empty()).then_some(reason),
                    )));
                }
                Ok(_) => continue, // ping/pong/binary — nothing to map
                Err(e) => return Err(RecvError(e.to_string())),
            };
            if let Some(ev) = map_frame(&text) {
                return Ok(Some(ev));
            }
            // Non-JSON keep-alive, unknown type, or empty transcript: keep reading.
        }
    }
}

#[derive(Deserialize)]
struct Incoming {
    message_type: Option<String>,
    text: Option<String>,
    committed_transcript: Option<String>,
    #[allow(dead_code)]
    session_id: Option<String>,
}

/// Pure text-frame → event mapping. Returns `None` for frames the runner should
/// ignore (non-JSON, unknown `message_type`, empty transcript). Kept separate
/// from `recv_event` so it can be fixture-tested without a live socket.
fn map_frame(text: &str) -> Option<SttEvent> {
    let parsed: Incoming = serde_json::from_str(text).ok()?;
    match parsed.message_type.as_deref().unwrap_or("") {
        "session_started" => Some(SttEvent::SessionStarted),
        "partial_transcript" => {
            let t = parsed.text?;
            let t = t.trim();
            (!t.is_empty()).then(|| SttEvent::Partial(t.to_string()))
        }
        "committed_transcript" | "committed_transcript_with_timestamps" => {
            let final_text = parsed
                .text
                .or(parsed.committed_transcript)
                .unwrap_or_default()
                .trim()
                .to_string();
            (!final_text.is_empty()).then_some(SttEvent::Committed(final_text))
        }
        "quota_exceeded" => Some(SttEvent::KeyFailure(FailKind::Exhausted)),
        "auth_error" | "invalid_api_key" | "unauthorized" => {
            Some(SttEvent::KeyFailure(FailKind::Invalid))
        }
        "rate_limit_exceeded" | "too_many_requests" => {
            Some(SttEvent::KeyFailure(FailKind::RateLimit))
        }
        t if t.contains("error") || is_account_exhausted(text) => {
            let kind = if is_account_exhausted(text) {
                FailKind::Exhausted
            } else {
                FailKind::Transient
            };
            Some(SttEvent::KeyFailure(kind))
        }
        _ => None,
    }
}

/// Substring check on close-frame reasons / message bodies that indicate the
/// ElevenLabs API key is out of credits (or otherwise billing-failed).
fn is_account_exhausted(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "insufficient_funds",
        "insufficient funds",
        "quota_exceeded",
        "quota exceeded",
        "credit balance",
        "billing",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_maps_to_partial() {
        let f = r#"{"message_type":"partial_transcript","text":"hello wor"}"#;
        assert!(matches!(map_frame(f), Some(SttEvent::Partial(t)) if t == "hello wor"));
    }

    #[test]
    fn committed_maps_to_committed_and_trims() {
        let f = r#"{"message_type":"committed_transcript","text":"  hello world  "}"#;
        assert!(matches!(map_frame(f), Some(SttEvent::Committed(t)) if t == "hello world"));
    }

    #[test]
    fn committed_with_timestamps_variant() {
        let f = r#"{"message_type":"committed_transcript_with_timestamps","committed_transcript":"final text"}"#;
        assert!(matches!(map_frame(f), Some(SttEvent::Committed(t)) if t == "final text"));
    }

    #[test]
    fn empty_partial_is_ignored() {
        let f = r#"{"message_type":"partial_transcript","text":"   "}"#;
        assert!(map_frame(f).is_none());
    }

    #[test]
    fn session_started_maps() {
        let f = r#"{"message_type":"session_started","session_id":"abc"}"#;
        assert!(matches!(map_frame(f), Some(SttEvent::SessionStarted)));
    }

    #[test]
    fn failure_kinds_map_correctly() {
        assert!(matches!(
            map_frame(r#"{"message_type":"quota_exceeded"}"#),
            Some(SttEvent::KeyFailure(FailKind::Exhausted))
        ));
        assert!(matches!(
            map_frame(r#"{"message_type":"invalid_api_key"}"#),
            Some(SttEvent::KeyFailure(FailKind::Invalid))
        ));
        assert!(matches!(
            map_frame(r#"{"message_type":"rate_limit_exceeded"}"#),
            Some(SttEvent::KeyFailure(FailKind::RateLimit))
        ));
    }

    #[test]
    fn generic_error_frame_classifies() {
        // A billing-flavored error body → Exhausted; a plain error → Transient.
        assert!(matches!(
            map_frame(r#"{"message_type":"error","text":"insufficient_funds"}"#),
            Some(SttEvent::KeyFailure(FailKind::Exhausted))
        ));
        assert!(matches!(
            map_frame(r#"{"message_type":"internal_error"}"#),
            Some(SttEvent::KeyFailure(FailKind::Transient))
        ));
    }

    #[test]
    fn non_json_and_unknown_are_ignored() {
        assert!(map_frame("not json at all").is_none());
        assert!(map_frame(r#"{"message_type":"heartbeat"}"#).is_none());
    }
}
