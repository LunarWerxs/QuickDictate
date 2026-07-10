//! Deepgram nova-3 realtime adapter.
//!
//! Deepgram is the "closest twin" to ElevenLabs but simpler on the wire: raw
//! binary PCM16 frames (no base64/JSON envelope) and `Authorization: Token`
//! auth. Interim `Results` map to `Partial`, `is_final` results to `Committed`.
//! End-of-stream is signaled with `{"type":"CloseStream"}`, after which the
//! server flushes finals and closes on its own.

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::provider::{
    i16_slice_as_bytes, AudioFormat, ConnectError, Encoding, ProviderSession, ProviderSink,
    ProviderStream, RecvError, SendError, SttEvent, SttProvider, SttSessionOpts,
};

const WS_URL: &str = "wss://api.deepgram.com/v1/listen";
const MODEL_ID: &str = "nova-3";

type WsSink = futures_util::stream::SplitSink<
    WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream =
    futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>;

pub struct DeepgramProvider;

#[async_trait]
impl SttProvider for DeepgramProvider {
    fn id(&self) -> &'static str {
        "deepgram"
    }

    fn required_audio_format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: 16_000,
            encoding: Encoding::Pcm16Le,
        }
    }

    async fn connect(
        &self,
        key: &str,
        opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        let model = opts.model.as_deref().unwrap_or(MODEL_ID);
        let url = format!(
            "{WS_URL}?model={model}&encoding=linear16&sample_rate={rate}&interim_results=true&language={lang}",
            rate = opts.sample_rate,
            lang = opts.language,
        );
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| ConnectError(format!("ws request: {e}")))?;
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Token {key}"))
                .map_err(|e| ConnectError(format!("bad key header: {e}")))?,
        );
        let (ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| ConnectError(format!("ws connect failed: {e}")))?;
        let (sink, stream) = ws.split();
        Ok(ProviderSession {
            sink: Box::new(DeepgramSink { sink }),
            stream: Box::new(DeepgramStream {
                stream,
                closed: false,
            }),
        })
    }
}

struct DeepgramSink {
    sink: WsSink,
}

#[async_trait]
impl ProviderSink for DeepgramSink {
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        // Raw little-endian PCM16 binary frame — no envelope.
        let bytes = i16_slice_as_bytes(pcm).to_vec();
        self.sink
            .send(Message::Binary(bytes))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        // Tell Deepgram to flush interim → final and close. The server then
        // sends any remaining finals and closes the socket itself.
        self.sink
            .send(Message::Text("{\"type\":\"CloseStream\"}".into()))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn close(&mut self) -> Result<(), SendError> {
        // No-op: CloseStream already initiates the server-side close. Sending a
        // client Close here would race Deepgram's final-results flush.
        Ok(())
    }
}

struct DeepgramStream {
    stream: WsStream,
    closed: bool,
}

#[async_trait]
impl ProviderStream for DeepgramStream {
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
                Ok(_) => continue, // binary/ping/pong — nothing to map
                Err(e) => return Err(RecvError(e.to_string())),
            };
            if let Some(ev) = map_frame(&text) {
                return Ok(Some(ev));
            }
            // Metadata / SpeechStarted / UtteranceEnd / empty transcript: keep reading.
        }
    }
}

#[derive(Deserialize)]
struct DgMessage {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    is_final: Option<bool>,
    channel: Option<DgChannel>,
}

#[derive(Deserialize)]
struct DgChannel {
    alternatives: Option<Vec<DgAlt>>,
}

#[derive(Deserialize)]
struct DgAlt {
    transcript: Option<String>,
}

/// Pure text-frame → event mapping. `None` for frames the runner should ignore
/// (non-JSON, Metadata/SpeechStarted/UtteranceEnd, empty transcript). Split out
/// from `recv_event` for fixture testing without a live socket.
fn map_frame(text: &str) -> Option<SttEvent> {
    let parsed: DgMessage = serde_json::from_str(text).ok()?;
    match parsed.msg_type.as_deref().unwrap_or("") {
        "Results" => {
            let transcript = parsed
                .channel
                .and_then(|c| c.alternatives)
                .and_then(|mut a| a.drain(..).next())
                .and_then(|alt| alt.transcript)
                .unwrap_or_default();
            let transcript = transcript.trim();
            if transcript.is_empty() {
                return None;
            }
            if parsed.is_final.unwrap_or(false) {
                Some(SttEvent::Committed(transcript.to_string()))
            } else {
                Some(SttEvent::Partial(transcript.to_string()))
            }
        }
        // Deepgram surfaces auth/quota problems at the handshake (a connect
        // error), so a mid-stream Error frame is rare; classify defensively
        // rather than dropping the session silently.
        "Error" => Some(SttEvent::KeyFailure(
            super::provider::classify_by_substring(text),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::FailKind;

    fn results(transcript: &str, is_final: bool) -> String {
        format!(
            r#"{{"type":"Results","is_final":{is_final},"channel":{{"alternatives":[{{"transcript":"{transcript}"}}]}}}}"#
        )
    }

    #[test]
    fn interim_results_map_to_partial() {
        assert!(matches!(
            map_frame(&results("hello wor", false)),
            Some(SttEvent::Partial(t)) if t == "hello wor"
        ));
    }

    #[test]
    fn final_results_map_to_committed() {
        assert!(matches!(
            map_frame(&results("hello world", true)),
            Some(SttEvent::Committed(t)) if t == "hello world"
        ));
    }

    #[test]
    fn empty_transcript_is_ignored() {
        assert!(map_frame(&results("   ", true)).is_none());
        assert!(map_frame(&results("", false)).is_none());
    }

    #[test]
    fn metadata_and_speechstarted_ignored() {
        assert!(map_frame(r#"{"type":"Metadata","duration":1.2}"#).is_none());
        assert!(map_frame(r#"{"type":"SpeechStarted"}"#).is_none());
        assert!(map_frame("not json").is_none());
    }

    #[test]
    fn error_frame_classifies() {
        assert!(matches!(
            map_frame(r#"{"type":"Error","description":"401 Unauthorized"}"#),
            Some(SttEvent::KeyFailure(FailKind::Invalid))
        ));
    }
}
