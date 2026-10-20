//! Connections dictation: live speech-to-text from the Connections Dictation
//! service, signed in with the Connections account the user linked for the
//! dictation engine. A session is minted over HTTPS, then streamed over a
//! WebSocket that carries the session token as a subprotocol.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use super::provider::{
    AudioFormat, ConnectError, ProviderSession, SttEvent, SttProvider, SttSessionOpts,
};
use super::ws;

const SESSIONS_URL: &str = "https://dictation.connectionsapi.com/v1/sessions";
const SUBPROTOCOL: &str = "connections-dictation.v1";
const READY_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_FRAME: &str = "{\"type\":\"stop\"}";
const DICTATION_SCOPE: &str = "dictation:listen";

pub struct ConnectionsProvider;

#[async_trait]
impl SttProvider for ConnectionsProvider {
    fn id(&self) -> &'static str {
        "connections"
    }

    fn prewarm_host(&self) -> Option<&'static str> {
        Some("dictation.connectionsapi.com")
    }

    fn required_audio_format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: 16_000,
        }
    }

    fn requires_api_key(&self) -> bool {
        false
    }

    async fn connect(
        &self,
        _key: &str,
        opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        // The stored login is refreshed with blocking HTTP, so keep it off the runtime.
        let access = tokio::task::spawn_blocking(crate::sync::dictation_access_token)
            .await
            .map_err(|e| ConnectError(format!("dictation token task: {e}")))?
            .map_err(|e| ConnectError(format!("connections dictation sign-in: {e}")))?;
        let minted = mint(&access, &opts.language).await?;
        start(&minted).await
    }
}

/// Where a minted session streams, and the token that authorizes it.
#[derive(Debug)]
pub(super) struct Minted {
    url: String,
    token: String,
}

#[derive(Deserialize)]
struct SessionReply {
    url: String,
    token: String,
}

/// Server frames. `interim` and `final` carry the transcript; the text field
/// name is taken as `text` or `transcript`.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Frame {
    Ready,
    Interim {
        #[serde(alias = "transcript")]
        text: String,
    },
    Final {
        #[serde(alias = "transcript")]
        text: String,
    },
    End {
        #[serde(default)]
        reason: String,
    },
}

async fn mint(access: &str, lang: &str) -> Result<Minted, ConnectError> {
    let resp = reqwest::Client::new()
        .post(SESSIONS_URL)
        .bearer_auth(access)
        .json(&serde_json::json!({ "mode": "live", "lang": lang }))
        .send()
        .await
        .map_err(|e| ConnectError(format!("dictation session request: {e}")))?;
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        let reply: SessionReply = resp
            .json()
            .await
            .map_err(|e| ConnectError(format!("dictation session reply: {e}")))?;
        return Ok(Minted {
            url: reply.url,
            token: reply.token,
        });
    }
    let body = resp.text().await.unwrap_or_default();
    if matches!(status, 401 | 403) && body.contains(DICTATION_SCOPE) {
        crate::sync::clear_dictation_scope();
    }
    Err(refusal(status, &body))
}

/// The user-facing reason a session request was refused.
fn refusal(status: u16, body: &str) -> ConnectError {
    if status == 429 && body.contains("too_many_sessions") {
        return ConnectError(
            "Connections dictation: too many sessions are open; try again shortly".into(),
        );
    }
    if matches!(status, 401 | 403) && body.contains(DICTATION_SCOPE) {
        return ConnectError(
            "Connections dictation needs sign-in again; choose Use my Connections minutes".into(),
        );
    }
    ConnectError(format!("Connections dictation refused the session (HTTP {status})"))
}

/// Open the streaming socket for `minted` and wait for the server's `ready`.
pub(super) async fn start(minted: &Minted) -> Result<ProviderSession, ConnectError> {
    let mut request = minted
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| ConnectError(format!("dictation request: {e}")))?;
    let protocols = format!("{SUBPROTOCOL}, token.{}", minted.token);
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(&protocols)
            .map_err(|_| ConnectError("dictation token is not a valid header value".into()))?,
    );
    let mut conn = ws::open(request).await?;
    match tokio::time::timeout(READY_TIMEOUT, wait_for_ready(&mut conn)).await {
        Ok(result) => result?,
        Err(_) => return Err(ConnectError("dictation did not become ready in time".into())),
    }
    Ok(ws::pcm_session(conn, STOP_FRAME, None, map_frame))
}

async fn wait_for_ready(conn: &mut ws::WsConn) -> Result<(), ConnectError> {
    while let Some(frame) = conn.next().await {
        match frame {
            Ok(Message::Text(text)) => match serde_json::from_str::<Frame>(text.as_str()) {
                Ok(Frame::Ready) => return Ok(()),
                Ok(Frame::End { reason }) => {
                    return Err(ConnectError(format!(
                        "dictation ended before it was ready: {reason}"
                    )))
                }
                _ => {}
            },
            Ok(Message::Close(_)) => break,
            Err(e) => return Err(ConnectError(format!("dictation handshake: {e}"))),
            Ok(_) => {}
        }
    }
    Err(ConnectError("dictation closed before it was ready".into()))
}

/// Pure text-frame to event mapping. `None` for frames the runner ignores.
fn map_frame(text: &str) -> Option<SttEvent> {
    match serde_json::from_str::<Frame>(text).ok()? {
        Frame::Ready => None,
        Frame::Interim { text } => non_empty(text).map(SttEvent::Partial),
        Frame::Final { text } => non_empty(text).map(SttEvent::Committed),
        Frame::End { reason } => Some(end_event(&reason)),
    }
}

fn non_empty(text: String) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn end_event(reason: &str) -> SttEvent {
    let failure = |message: &str| SttEvent::ProviderFailure(message.to_string());
    match reason {
        "stopped" | "idle" => SttEvent::Closed(None),
        "limit" => failure("Connections dictation stopped at its time limit"),
        "credits" => failure("Connections minutes are used up"),
        "busy" => failure("Connections dictation is busy; try again shortly"),
        "no_worker" | "worker_lost" => failure("Connections dictation lost its worker; try again"),
        "refused" => failure("Connections dictation refused this session"),
        other => failure(&format!("Connections dictation ended: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::handshake::server::{Request as ServerRequest, Response};

    #[test]
    fn transcript_frames_map_to_partial_and_committed() {
        assert!(matches!(
            map_frame(r#"{"type":"interim","text":" hello wor "}"#),
            Some(SttEvent::Partial(t)) if t == "hello wor"
        ));
        assert!(matches!(
            map_frame(r#"{"type":"final","transcript":"hello world"}"#),
            Some(SttEvent::Committed(t)) if t == "hello world"
        ));
        assert!(map_frame(r#"{"type":"interim","text":"   "}"#).is_none());
        assert!(map_frame(r#"{"type":"ready"}"#).is_none());
        assert!(map_frame("not json").is_none());
    }

    #[test]
    fn end_reasons_close_cleanly_or_name_the_failure() {
        assert!(matches!(
            map_frame(r#"{"type":"end","reason":"stopped"}"#),
            Some(SttEvent::Closed(None))
        ));
        assert!(matches!(
            map_frame(r#"{"type":"end","reason":"credits"}"#),
            Some(SttEvent::ProviderFailure(m)) if m.contains("minutes")
        ));
        assert!(matches!(
            map_frame(r#"{"type":"end","reason":"surprise"}"#),
            Some(SttEvent::ProviderFailure(m)) if m.ends_with("surprise")
        ));
    }

    #[test]
    fn refusals_name_their_cause() {
        assert!(refusal(429, r#"{"error":"too_many_sessions"}"#)
            .0
            .contains("too many sessions"));
        assert!(refusal(403, "missing dictation:listen")
            .0
            .contains("sign-in again"));
        assert!(refusal(500, "").0.contains("HTTP 500"));
    }

    /// A stand-in dictation server: it reports the subprotocol the client offered,
    /// sends `ready`, reads audio until `stop`, then sends one interim, one final
    /// and an `end`.
    #[allow(clippy::result_large_err)]
    async fn fake_server(listener: tokio::net::TcpListener) -> String {
        let (tcp, _) = listener.accept().await.unwrap();
        let (offered_tx, offered_rx) = tokio::sync::oneshot::channel::<String>();
        let mut ws = tokio_tungstenite::accept_hdr_async(
            tcp,
            move |req: &ServerRequest, mut resp: Response| {
                let offered = req
                    .headers()
                    .get("sec-websocket-protocol")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                let _ = offered_tx.send(offered);
                resp.headers_mut()
                    .insert("Sec-WebSocket-Protocol", HeaderValue::from_static(SUBPROTOCOL));
                Ok(resp)
            },
        )
        .await
        .unwrap();
        use futures_util::SinkExt;
        ws.send(Message::Text(r#"{"type":"ready"}"#.into()))
            .await
            .unwrap();
        let mut saw_audio = false;
        while let Some(Ok(msg)) = ws.next().await {
            match msg {
                Message::Binary(_) => saw_audio = true,
                Message::Text(t) if t.as_str().contains("\"stop\"") => break,
                _ => {}
            }
        }
        assert!(saw_audio, "audio frames must arrive before stop");
        for frame in [
            r#"{"type":"interim","text":"hello"}"#,
            r#"{"type":"final","text":"hello world"}"#,
            r#"{"type":"end","reason":"stopped"}"#,
        ] {
            ws.send(Message::Text(frame.into())).await.unwrap();
        }
        offered_rx.await.unwrap()
    }

    #[tokio::test]
    async fn session_streams_audio_and_transcript_over_the_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(fake_server(listener));

        let minted = Minted {
            url: format!("ws://{addr}/"),
            token: "test-token".into(),
        };
        let mut session = start(&minted).await.unwrap();
        session.sink.send_audio(&[1, 2, 3]).await.unwrap();
        session.sink.commit().await.unwrap();

        assert!(matches!(
            session.stream.recv_event().await.unwrap(),
            Some(SttEvent::Partial(t)) if t == "hello"
        ));
        assert!(matches!(
            session.stream.recv_event().await.unwrap(),
            Some(SttEvent::Committed(t)) if t == "hello world"
        ));
        assert!(matches!(
            session.stream.recv_event().await.unwrap(),
            Some(SttEvent::Closed(None))
        ));
        assert_eq!(
            server.await.unwrap(),
            format!("{SUBPROTOCOL}, token.test-token")
        );
    }
}
