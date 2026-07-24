//! Alibaba Cloud DashScope Paraformer realtime adapter.
//!
//! Protocol is the richest of the streaming set: a JSON `run-task` handshake,
//! then raw binary PCM16, then `finish-task` to end. The server acks with
//! `task-started`, streams `result-generated` events (per-sentence, with
//! `sentence_end` marking a finalized sentence), and closes after
//! `task-finished`. We complete the whole handshake inside `connect` (waiting
//! for `task-started`) so the split sink/stream model matches the other
//! providers — the sink can send audio immediately once `connect` returns.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::provider::{
    i16_slice_as_bytes, AudioFormat, ConnectError, ProviderSession, ProviderSink, ProviderStream,
    RecvError, SendError, SttEvent, SttProvider, SttSessionOpts,
};

// Host is chosen by the `dashscope_intl` config flag: mainland-China (default)
// vs. the `-intl` host for International accounts. A key from the wrong region
// 401s at the WebSocket upgrade.
const WS_URL_CN: &str = "wss://dashscope.aliyuncs.com/api-ws/v1/inference";
const WS_URL_INTL: &str = "wss://dashscope-intl.aliyuncs.com/api-ws/v1/inference";
const MODEL_ID: &str = "paraformer-realtime-v2";

/// Hard cap on the post-connect `run-task` → `task-started` exchange. Without
/// it, a connection the server accepts but never answers (black-holed network,
/// silent proxy) would park the session forever waiting for `task-started`.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(6);

type WsSink = futures_util::stream::SplitSink<
    WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream =
    futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>;

/// DashScope wants a 32-char task_id, reused across run-task/finish-task.
fn gen_task_id() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64;
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in nanos.to_le_bytes().iter().chain(c.to_le_bytes().iter()) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let h2 = h.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ nanos;
    format!("{h:016x}{h2:016x}")
}

pub struct DashScopeProvider {
    /// `true` = International (`-intl`) host, `false` = mainland-China host.
    pub intl: bool,
}

impl DashScopeProvider {
    fn ws_url(&self) -> &'static str {
        if self.intl {
            WS_URL_INTL
        } else {
            WS_URL_CN
        }
    }
}

#[async_trait]
impl SttProvider for DashScopeProvider {
    fn id(&self) -> &'static str {
        "dashscope"
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
        let model = opts.model.as_deref().unwrap_or(MODEL_ID);
        let task_id = gen_task_id();

        let mut request = self
            .ws_url()
            .into_client_request()
            .map_err(|e| ConnectError(format!("ws request: {e}")))?;
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("bearer {key}"))
                .map_err(|e| ConnectError(format!("bad key header: {e}")))?,
        );
        let (mut ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| ConnectError(format!("ws connect failed: {e}")))?;

        // 1) send run-task
        let run_task = json!({
            "header": { "action": "run-task", "task_id": task_id, "streaming": "duplex" },
            "payload": {
                "task_group": "audio",
                "task": "asr",
                "function": "recognition",
                "model": model,
                "parameters": { "format": "pcm", "sample_rate": opts.sample_rate },
                "input": {}
            }
        })
        .to_string();
        ws.send(Message::Text(run_task))
            .await
            .map_err(|e| ConnectError(format!("run-task send: {e}")))?;

        // 2) await task-started (or task-failed) before letting audio flow,
        //    bounded so a silent-but-open connection can't hang the session.
        let handshake = async {
            loop {
                match ws.next().await {
                    Some(Ok(Message::Text(t))) => match header_event(&t).as_deref() {
                        Some("task-started") => return Ok(()),
                        Some("task-failed") => {
                            return Err(ConnectError(format!("task-failed: {t}")))
                        }
                        _ => continue,
                    },
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(ConnectError(format!("handshake recv: {e}"))),
                    None => return Err(ConnectError("closed before task-started".into())),
                }
            }
        };
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(ConnectError(format!(
                    "no task-started within {HANDSHAKE_TIMEOUT:?}"
                )))
            }
        }

        let (sink, stream) = ws.split();
        Ok(ProviderSession {
            sink: Box::new(DashScopeSink { sink, task_id }),
            stream: Box::new(DashScopeStream {
                stream,
                closed: false,
            }),
        })
    }
}

struct DashScopeSink {
    sink: WsSink,
    task_id: String,
}

#[async_trait]
impl ProviderSink for DashScopeSink {
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        let bytes = i16_slice_as_bytes(pcm).to_vec();
        self.sink
            .send(Message::Binary(bytes))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        let finish = json!({
            "header": { "action": "finish-task", "task_id": self.task_id, "streaming": "duplex" },
            "payload": { "input": {} }
        })
        .to_string();
        self.sink
            .send(Message::Text(finish))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn keepalive(&mut self) -> Result<(), SendError> {
        // No documented no-audio keepalive, so use a transport-level WS ping: a
        // harmless standard control frame that resets idle timers. The recv side
        // ignores the Pong reply.
        self.sink
            .send(Message::Ping(Vec::new()))
            .await
            .map_err(|e| SendError(e.to_string()))
    }

    async fn close(&mut self) -> Result<(), SendError> {
        // No-op: finish-task drives the server-side close after task-finished.
        Ok(())
    }
}

struct DashScopeStream {
    stream: WsStream,
    closed: bool,
}

#[async_trait]
impl ProviderStream for DashScopeStream {
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

/// Extract `header.event` from a DashScope frame (used during the handshake).
fn header_event(text: &str) -> Option<String> {
    serde_json::from_str::<DsMessage>(text)
        .ok()
        .and_then(|m| m.header)
        .and_then(|h| h.event)
}

#[derive(Deserialize)]
struct DsMessage {
    header: Option<DsHeader>,
    payload: Option<DsPayload>,
}

#[derive(Deserialize)]
struct DsHeader {
    event: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
}

#[derive(Deserialize)]
struct DsPayload {
    output: Option<DsOutput>,
}

#[derive(Deserialize)]
struct DsOutput {
    sentence: Option<DsSentence>,
}

#[derive(Deserialize)]
struct DsSentence {
    text: Option<String>,
    sentence_end: Option<bool>,
}

/// Pure frame → event mapping (fixture-tested).
fn map_frame(text: &str) -> Option<SttEvent> {
    let parsed: DsMessage = serde_json::from_str(text).ok()?;
    let header = parsed.header?;
    match header.event.as_deref().unwrap_or("") {
        "result-generated" => {
            let sentence = parsed.payload?.output?.sentence?;
            let t = sentence.text.unwrap_or_default();
            let t = t.trim();
            if t.is_empty() {
                return None;
            }
            if sentence.sentence_end.unwrap_or(false) {
                Some(SttEvent::Committed(t.to_string()))
            } else {
                Some(SttEvent::Partial(t.to_string()))
            }
        }
        "task-finished" => Some(SttEvent::Closed(None)),
        "task-failed" => {
            let msg = format!(
                "{} {}",
                header.error_code.unwrap_or_default(),
                header.error_message.unwrap_or_default()
            );
            Some(SttEvent::KeyFailure(
                super::provider::classify_by_substring(&msg),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sentence(text: &str, end: bool) -> String {
        format!(
            r#"{{"header":{{"event":"result-generated"}},"payload":{{"output":{{"sentence":{{"text":"{text}","sentence_end":{end}}}}}}}}}"#
        )
    }

    #[test]
    fn interim_sentence_is_partial() {
        assert!(matches!(
            map_frame(&sentence("hello wor", false)),
            Some(SttEvent::Partial(t)) if t == "hello wor"
        ));
    }

    #[test]
    fn ended_sentence_is_committed() {
        assert!(matches!(
            map_frame(&sentence("hello world", true)),
            Some(SttEvent::Committed(t)) if t == "hello world"
        ));
    }

    #[test]
    fn task_finished_closes() {
        assert!(matches!(
            map_frame(r#"{"header":{"event":"task-finished"}}"#),
            Some(SttEvent::Closed(None))
        ));
    }

    #[test]
    fn task_failed_is_key_failure() {
        assert!(matches!(
            map_frame(
                r#"{"header":{"event":"task-failed","error_code":"InvalidApiKey","error_message":"unauthorized"}}"#
            ),
            Some(SttEvent::KeyFailure(_))
        ));
    }

    #[test]
    fn task_started_and_empty_ignored() {
        assert!(map_frame(r#"{"header":{"event":"task-started"}}"#).is_none());
        assert!(map_frame(&sentence("", true)).is_none());
    }

    #[test]
    fn task_ids_are_32_char_and_unique() {
        let a = gen_task_id();
        let b = gen_task_id();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }
}
