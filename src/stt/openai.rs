//! OpenAI Realtime transcription adapter (gpt-4o-transcribe).
//!
//! Uses the Realtime WebSocket with `intent=transcription` (the plain
//! `/v1/audio/transcriptions` Whisper endpoint is batch-only). Audio is JSON
//! base64 PCM16 at **24 kHz**. On connect we push a `transcription_session.update`
//! to select the model and PCM16 format; transcription `.delta` events stream
//! in (accumulated into a live partial) and `.completed` carries the final text.
//!
//! Verified end-to-end against the live OpenAI Realtime API (2026-07);
//! `live_test::live_openai` exercises it with a real `OPENAI_KEYS` key.

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
    classify_by_substring, i16_slice_as_bytes, AudioFormat, ConnectError, ProviderSession,
    ProviderSink, ProviderStream, RecvError, SendError, SttEvent, SttProvider, SttSessionOpts,
};

const WS_URL: &str = "wss://api.openai.com/v1/realtime?intent=transcription";
const DEFAULT_MODEL: &str = "gpt-4o-transcribe";

type WsSink = futures_util::stream::SplitSink<
    WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream =
    futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>;

pub struct OpenAiProvider;

#[async_trait]
impl SttProvider for OpenAiProvider {
    fn id(&self) -> &'static str {
        "openai"
    }

    fn required_audio_format(&self) -> AudioFormat {
        // OpenAI Realtime expects 24 kHz PCM16.
        AudioFormat {
            sample_rate: 24_000,
        }
    }

    async fn connect(
        &self,
        key: &str,
        opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        let model = opts.model.as_deref().unwrap_or(DEFAULT_MODEL);
        let mut request = WS_URL
            .into_client_request()
            .map_err(|e| ConnectError(format!("ws request: {e}")))?;
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|e| ConnectError(format!("bad key header: {e}")))?,
        );
        let (mut ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| ConnectError(format!("ws connect failed: {e}")))?;

        // Configure the transcription session (GA Realtime shape). Manual commit
        // (turn_detection = null) so we control end-of-utterance.
        let update = json!({
            "type": "session.update",
            "session": {
                "type": "transcription",
                "audio": {
                    "input": {
                        "format": { "type": "audio/pcm", "rate": opts.sample_rate },
                        "transcription": { "model": model, "language": opts.language },
                        "turn_detection": null
                    }
                }
            }
        })
        .to_string();
        ws.send(Message::Text(update))
            .await
            .map_err(|e| ConnectError(format!("session.update send: {e}")))?;

        let (sink, stream) = ws.split();
        Ok(ProviderSession {
            sink: Box::new(OpenAiSink { sink }),
            stream: Box::new(OpenAiStream {
                stream,
                closed: false,
                accum: String::new(),
            }),
        })
    }
}

struct OpenAiSink {
    sink: WsSink,
}

#[async_trait]
impl ProviderSink for OpenAiSink {
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        let audio = base64::engine::general_purpose::STANDARD.encode(i16_slice_as_bytes(pcm));
        let msg = json!({ "type": "input_audio_buffer.append", "audio": audio }).to_string();
        self.sink
            .send(Message::Text(msg))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        self.sink
            .send(Message::Text(
                "{\"type\":\"input_audio_buffer.commit\"}".into(),
            ))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn keepalive(&mut self) -> Result<(), SendError> {
        // The realtime socket has no short audio-idle close, but a transport WS
        // ping keeps any connection idle timer from firing during a long silent
        // tail. The recv side ignores the Pong reply.
        self.sink
            .send(Message::Ping(Vec::new()))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn close(&mut self) -> Result<(), SendError> {
        // No-op: after `input_audio_buffer.commit`, OpenAI streams the
        // transcription deltas + `.completed`. Sending a WS Close here would
        // race (and cut off) those results, so we let the recv side drain them
        // and drop the socket when finished.
        Ok(())
    }
}

struct OpenAiStream {
    stream: WsStream,
    closed: bool,
    /// Delta events are incremental; we accumulate them into the live partial.
    accum: String,
}

#[async_trait]
impl ProviderStream for OpenAiStream {
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
                    return Ok(Some(SttEvent::Closed(
                        (!reason.is_empty()).then_some(reason),
                    )));
                }
                Ok(_) => continue,
                Err(e) => return Err(RecvError(e.to_string())),
            };
            match classify_event(&text) {
                OaEvent::Delta(d) => {
                    self.accum.push_str(&d);
                    let trimmed = self.accum.trim();
                    if !trimmed.is_empty() {
                        return Ok(Some(SttEvent::Partial(trimmed.to_string())));
                    }
                }
                OaEvent::Completed(t) => {
                    self.accum.clear();
                    let t = t.trim();
                    if !t.is_empty() {
                        return Ok(Some(SttEvent::Committed(t.to_string())));
                    }
                }
                OaEvent::Created => return Ok(Some(SttEvent::SessionStarted)),
                OaEvent::Failure(k) => return Ok(Some(SttEvent::KeyFailure(k))),
                OaEvent::Other => continue,
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum OaEvent {
    Delta(String),
    Completed(String),
    Created,
    Failure(crate::keys::FailKind),
    Other,
}

#[derive(Deserialize)]
struct OaMessage {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    delta: Option<String>,
    transcript: Option<String>,
    error: Option<OaError>,
}

#[derive(Deserialize)]
struct OaError {
    #[serde(rename = "type")]
    err_type: Option<String>,
    code: Option<String>,
    message: Option<String>,
}

/// Pure event classifier (fixture-tested). Stateless; delta accumulation is
/// applied by `recv_event`.
fn classify_event(text: &str) -> OaEvent {
    let Ok(m) = serde_json::from_str::<OaMessage>(text) else {
        return OaEvent::Other;
    };
    match m.msg_type.as_deref().unwrap_or("") {
        "conversation.item.input_audio_transcription.delta" => {
            m.delta.map(OaEvent::Delta).unwrap_or(OaEvent::Other)
        }
        "conversation.item.input_audio_transcription.completed" => m
            .transcript
            .map(OaEvent::Completed)
            .unwrap_or(OaEvent::Other),
        "session.created" => OaEvent::Created,
        "error" => {
            let msg = m
                .error
                .map(|e| {
                    format!(
                        "{} {} {}",
                        e.err_type.unwrap_or_default(),
                        e.code.unwrap_or_default(),
                        e.message.unwrap_or_default()
                    )
                })
                .unwrap_or_default();
            OaEvent::Failure(classify_by_substring(&msg))
        }
        _ => OaEvent::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::FailKind;

    #[test]
    fn delta_and_completed() {
        let d = r#"{"type":"conversation.item.input_audio_transcription.delta","delta":"hello "}"#;
        assert_eq!(classify_event(d), OaEvent::Delta("hello ".to_string()));
        let c = r#"{"type":"conversation.item.input_audio_transcription.completed","transcript":"hello world"}"#;
        assert_eq!(
            classify_event(c),
            OaEvent::Completed("hello world".to_string())
        );
    }

    #[test]
    fn created_and_unknown() {
        assert_eq!(
            classify_event(r#"{"type":"session.created"}"#),
            OaEvent::Created
        );
        assert_eq!(
            classify_event(r#"{"type":"input_audio_buffer.committed"}"#),
            OaEvent::Other
        );
        assert_eq!(classify_event("not json"), OaEvent::Other);
    }

    #[test]
    fn error_maps_to_failure() {
        let e = r#"{"type":"error","error":{"type":"invalid_request_error","code":"invalid_api_key","message":"Incorrect API key"}}"#;
        assert_eq!(classify_event(e), OaEvent::Failure(FailKind::Invalid));
    }
}
