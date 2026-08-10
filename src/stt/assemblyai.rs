//! AssemblyAI Universal-Streaming (v3) adapter.
//!
//! Wire shape is close to Deepgram: raw binary PCM16 frames, JSON events in.
//! Auth is the API key in a bare `Authorization` header. The v3 protocol emits
//! `Begin` (session start) and `Turn` events; `end_of_turn=true` marks a
//! finalized turn (→ `Committed`), interim turns are `Partial`. End the session
//! with `{"type":"Terminate"}`, after which the server flushes and closes.

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::provider::{
    i16_slice_as_bytes, AudioFormat, ConnectError, ProviderSession, ProviderSink, ProviderStream,
    RecvError, SendError, SttEvent, SttProvider, SttSessionOpts,
};

const WS_URL: &str = "wss://streaming.assemblyai.com/v3/ws";
/// v3 streaming hard-errors above 100 keyterms per session.
const MAX_KEYTERMS: usize = 100;

type WsSink = futures_util::stream::SplitSink<
    WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream =
    futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>;

pub struct AssemblyAiProvider;

#[async_trait]
impl SttProvider for AssemblyAiProvider {
    fn id(&self) -> &'static str {
        "assemblyai"
    }

    fn required_audio_format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: 16_000,
        }
    }

    async fn connect(
        &self,
        key: &str,
        opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        // v3 streaming is English-only and takes no language param.
        let url = build_url(opts);
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| ConnectError(format!("ws request: {e}")))?;
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(key).map_err(|e| ConnectError(format!("bad key header: {e}")))?,
        );
        let (ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| ConnectError(format!("ws connect failed: {e}")))?;
        let (sink, stream) = ws.split();
        Ok(ProviderSession {
            sink: Box::new(AssemblyAiSink { sink }),
            stream: Box::new(AssemblyAiStream {
                stream,
                closed: false,
            }),
        })
    }
}

/// Build the v3 streaming WebSocket URL. Pure (fixture-tested); `connect`
/// just calls this. `keyterms_prompt` is a JSON-encoded array of strings per
/// the v3 streaming API; the server hard-errors above 100 terms and silently
/// ignores any single term over 50 characters, so only the count is capped
/// here.
fn build_url(opts: &SttSessionOpts) -> String {
    let mut url = format!(
        "{WS_URL}?sample_rate={rate}&encoding=pcm_s16le",
        rate = opts.sample_rate,
    );
    if !opts.custom_vocabulary.is_empty() {
        let terms: Vec<&str> = opts
            .custom_vocabulary
            .iter()
            .take(MAX_KEYTERMS)
            .map(String::as_str)
            .collect();
        let json = serde_json::to_string(&terms).unwrap_or_default();
        url.push_str("&keyterms_prompt=");
        url.push_str(&url::form_urlencoded::byte_serialize(json.as_bytes()).collect::<String>());
    }
    url
}

struct AssemblyAiSink {
    sink: WsSink,
}

#[async_trait]
impl ProviderSink for AssemblyAiSink {
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        let bytes = i16_slice_as_bytes(pcm).to_vec();
        self.sink
            .send(Message::Binary(bytes))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        // Terminate flushes the final turn(s) and closes the socket server-side.
        self.sink
            .send(Message::Text("{\"type\":\"Terminate\"}".into()))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn keepalive(&mut self) -> Result<(), SendError> {
        // No documented no-audio keepalive for the streaming API, so use a
        // transport-level WS ping: a harmless standard control frame that resets
        // connection idle timers. The recv side ignores the Pong reply.
        self.sink
            .send(Message::Ping(Vec::new()))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn close(&mut self) -> Result<(), SendError> {
        // No-op: Terminate already initiates the server-side close.
        Ok(())
    }
}

struct AssemblyAiStream {
    stream: WsStream,
    closed: bool,
}

#[async_trait]
impl ProviderStream for AssemblyAiStream {
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
            if let Some(ev) = map_frame(&text) {
                return Ok(Some(ev));
            }
        }
    }
}

#[derive(Deserialize)]
struct AaiMessage {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    transcript: Option<String>,
    end_of_turn: Option<bool>,
}

/// Pure frame → event mapping (fixture-tested).
fn map_frame(text: &str) -> Option<SttEvent> {
    let parsed: AaiMessage = serde_json::from_str(text).ok()?;
    match parsed.msg_type.as_deref().unwrap_or("") {
        "Begin" => Some(SttEvent::SessionStarted),
        "Turn" => {
            let transcript = parsed.transcript.unwrap_or_default();
            let transcript = transcript.trim();
            if transcript.is_empty() {
                return None;
            }
            if parsed.end_of_turn.unwrap_or(false) {
                Some(SttEvent::Committed(transcript.to_string()))
            } else {
                Some(SttEvent::Partial(transcript.to_string()))
            }
        }
        // Termination is the server ack of our Terminate; nothing to emit.
        "Termination" => None,
        "Error" => Some(SttEvent::KeyFailure(
            super::provider::classify_by_substring(text),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_maps_to_session_started() {
        assert!(matches!(
            map_frame(r#"{"type":"Begin","id":"x","expires_at":1}"#),
            Some(SttEvent::SessionStarted)
        ));
    }

    #[test]
    fn interim_turn_is_partial() {
        let f = r#"{"type":"Turn","transcript":"hello wor","end_of_turn":false}"#;
        assert!(matches!(map_frame(f), Some(SttEvent::Partial(t)) if t == "hello wor"));
    }

    #[test]
    fn final_turn_is_committed() {
        let f = r#"{"type":"Turn","transcript":"hello world","end_of_turn":true}"#;
        assert!(matches!(map_frame(f), Some(SttEvent::Committed(t)) if t == "hello world"));
    }

    #[test]
    fn empty_turn_and_termination_ignored() {
        assert!(map_frame(r#"{"type":"Turn","transcript":"","end_of_turn":true}"#).is_none());
        assert!(map_frame(r#"{"type":"Termination","audio_duration_seconds":6}"#).is_none());
        assert!(map_frame("not json").is_none());
    }

    fn test_opts(vocab: Vec<&str>) -> SttSessionOpts {
        SttSessionOpts {
            language: "en".into(),
            sample_rate: 16_000,
            model: None,
            custom_vocabulary: vocab.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn empty_vocabulary_url_is_unchanged() {
        let url = build_url(&test_opts(vec![]));
        assert_eq!(
            url,
            format!("{WS_URL}?sample_rate=16000&encoding=pcm_s16le")
        );
    }

    #[test]
    fn vocabulary_adds_keyterms_prompt_as_json_array() {
        let url = build_url(&test_opts(vec!["Anthropic", "QuickDictate"]));
        let expected_json = serde_json::to_string(&["Anthropic", "QuickDictate"]).unwrap();
        let expected_encoded: String =
            url::form_urlencoded::byte_serialize(expected_json.as_bytes()).collect();
        assert!(url.ends_with(&format!("&keyterms_prompt={expected_encoded}")));
    }

    #[test]
    fn keyterms_prompt_is_capped_at_100_terms() {
        let many: Vec<String> = (0..150).map(|i| format!("term{i}")).collect();
        let opts = SttSessionOpts {
            language: "en".into(),
            sample_rate: 16_000,
            model: None,
            custom_vocabulary: many,
        };
        let url = build_url(&opts);
        assert!(url.contains("term99"));
        assert!(!url.contains("term100"));
    }
}
