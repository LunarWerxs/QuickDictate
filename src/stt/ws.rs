//! WebSocket plumbing shared by the streaming adapters (ElevenLabs, Deepgram,
//! AssemblyAI, OpenAI Realtime, DashScope).
//!
//! All five open the socket the same way (a URL plus one auth header), send
//! through the same error mapping, and read through the same loop: a text
//! frame goes to the adapter's own mapper, a Close frame ends the stream with
//! its reason, pings and binary frames are skipped. That loop used to be
//! pasted into every adapter, where a fix to one could silently miss the
//! others. The same went for the connect tail (split, wrap the halves), the
//! keyterm query encoding, and the whole sink of the three raw-PCM adapters.
//! What stays in each adapter is only its actual protocol: the URL, the
//! header's value, the frames it sends and how it reads the ones it gets.

use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::provider::{
    i16_slice_as_bytes, ConnectError, ProviderSession, ProviderSink, ProviderStream, RecvError,
    SendError, SttEvent,
};

/// An open provider socket, before it is split into its two halves.
pub(super) type WsConn = WebSocketStream<MaybeTlsStream<TcpStream>>;
/// The outbound half of a split [`WsConn`].
pub(super) type WsSink = SplitSink<WsConn, Message>;

/// The upgrade request for `url`, carrying the credential in `header`.
/// Separate from [`open`] for the adapter that retries the upgrade itself
/// (ElevenLabs), where a request that cannot even be built must fail at once
/// rather than be retried.
pub(super) fn request(
    url: &str,
    header: &'static str,
    value: &str,
) -> Result<Request, ConnectError> {
    let mut request = url
        .into_client_request()
        .map_err(|e| ConnectError(format!("ws request: {e}")))?;
    request.headers_mut().insert(
        header,
        HeaderValue::from_str(value).map_err(|e| ConnectError(format!("bad key header: {e}")))?,
    );
    Ok(request)
}

/// Perform the WebSocket upgrade for a built [`request`].
pub(super) async fn open(request: Request) -> Result<WsConn, ConnectError> {
    let (ws, _resp) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| ConnectError(format!("ws connect failed: {e}")))?;
    Ok(ws)
}

/// [`request`] then [`open`]: the whole connect for every adapter that makes
/// a single attempt.
pub(super) async fn connect(
    url: &str,
    header: &'static str,
    value: &str,
) -> Result<WsConn, ConnectError> {
    open(request(url, header, value)?).await
}

/// Send a setup frame on the still-whole socket (OpenAI's `session.update`,
/// DashScope's `run-task`). Failing here fails the connect, so it is a
/// [`ConnectError`] naming the frame, classified like any other.
pub(super) async fn send_setup(
    conn: &mut WsConn,
    what: &str,
    text: String,
) -> Result<(), ConnectError> {
    conn.send(Message::Text(text.into()))
        .await
        .map_err(|e| ConnectError(format!("{what} send: {e}")))
}

/// Split an open socket into its outbound half and a [`WsReader`] over its
/// inbound half: the last step of every adapter's connect, once any
/// handshake of its own is done on the whole socket.
pub(super) fn split(conn: WsConn) -> (WsSink, WsReader) {
    let (sink, stream) = conn.split();
    (sink, WsReader::new(stream))
}

/// The whole session of a [`PcmSink`] adapter whose inbound frames map one
/// at a time through `map_text`.
pub(super) fn pcm_session(
    conn: WsConn,
    commit: impl Into<Utf8Bytes>,
    keepalive: Option<&'static str>,
    map_text: fn(&str) -> Option<SttEvent>,
) -> ProviderSession {
    let (sink, reader) = split(conn);
    ProviderSession {
        sink: Box::new(PcmSink {
            sink,
            commit: commit.into(),
            keepalive: keepalive.map(Utf8Bytes::from_static),
        }),
        stream: Box::new(MappedStream::new(reader, map_text)),
    }
}

/// Append `&name=value` to a socket URL, `value` form-urlencoded.
pub(super) fn push_query(url: &mut String, name: &str, value: &str) {
    url.push('&');
    url.push_str(name);
    url.push('=');
    url.extend(url::form_urlencoded::byte_serialize(value.as_bytes()));
}

/// Append the first `max` of `terms` as ONE query parameter holding a JSON
/// array of strings, the shape both AssemblyAI's `keyterms_prompt` and
/// ElevenLabs' `keyterms` take. Nothing at all when there are no terms, so a
/// user with no vocabulary gets exactly the URL they always did.
pub(super) fn push_json_terms(url: &mut String, name: &str, terms: &[String], max: usize) {
    if terms.is_empty() {
        return;
    }
    let terms: Vec<&str> = terms.iter().take(max).map(String::as_str).collect();
    let json = serde_json::to_string(&terms).unwrap_or_default();
    push_query(url, name, &json);
}

/// Send one frame, mapping a transport failure onto the runner's [`SendError`].
pub(super) async fn send(sink: &mut WsSink, msg: Message) -> Result<(), SendError> {
    sink.send(msg).await.map_err(|e| SendError(e.to_string()))
}

/// One chunk of raw little-endian PCM16 as a binary frame, no envelope.
pub(super) async fn send_pcm(sink: &mut WsSink, pcm: &[i16]) -> Result<(), SendError> {
    send(
        sink,
        Message::Binary(i16_slice_as_bytes(pcm).to_vec().into()),
    )
    .await
}

/// One text (JSON) frame.
pub(super) async fn send_text(
    sink: &mut WsSink,
    text: impl Into<Utf8Bytes>,
) -> Result<(), SendError> {
    send(sink, Message::Text(text.into())).await
}

/// A transport-level ping for the providers with no documented no-audio
/// keepalive: a standard control frame that resets connection idle timers and
/// carries nothing the model could transcribe. [`WsReader`] skips the Pong.
pub(super) async fn ping(sink: &mut WsSink) -> Result<(), SendError> {
    send(sink, Message::Ping(Vec::new().into())).await
}

/// What [`WsReader::next_frame`] surfaces; pings, pongs and binary frames are
/// already skipped.
pub(super) enum Frame {
    /// A text frame for the adapter's own mapper.
    Text(Utf8Bytes),
    /// The peer's Close frame, with its reason (empty when it gave none).
    Close(String),
}

/// The inbound half of a split [`WsConn`]. Remembers once the transport has
/// ended, so every later read reports end-of-stream instead of polling a
/// finished stream.
pub(super) struct WsReader {
    stream: SplitStream<WsConn>,
    closed: bool,
}

impl WsReader {
    pub(super) fn new(stream: SplitStream<WsConn>) -> Self {
        Self {
            stream,
            closed: false,
        }
    }

    /// Next text or Close frame; `Ok(None)` once the transport has ended.
    pub(super) async fn next_frame(&mut self) -> Result<Option<Frame>, RecvError> {
        if self.closed {
            return Ok(None);
        }
        loop {
            match self.stream.next().await {
                Some(Ok(Message::Text(t))) => return Ok(Some(Frame::Text(t))),
                Some(Ok(Message::Close(c))) => {
                    self.closed = true;
                    return Ok(Some(Frame::Close(close_reason(c))));
                }
                Some(Ok(_)) => continue, // binary/ping/pong: nothing to map
                Some(Err(e)) => return Err(RecvError(e.to_string())),
                None => {
                    self.closed = true;
                    return Ok(None);
                }
            }
        }
    }

    /// Report end-of-stream from now on, for a protocol whose last event is a
    /// frame rather than a Close (OpenAI's `.completed`).
    pub(super) fn finish(&mut self) {
        self.closed = true;
    }
}

/// The whole inbound half of an adapter whose frames map one at a time with
/// no state (every one but OpenAI, which accumulates deltas): the next frame
/// `map_text` turns into an event, or the peer's close through `map_close`.
pub(super) struct MappedStream {
    ws: WsReader,
    map_text: fn(&str) -> Option<SttEvent>,
    map_close: fn(String) -> SttEvent,
}

impl MappedStream {
    /// A stream whose peer close is a plain [`closed_event`].
    pub(super) fn new(ws: WsReader, map_text: fn(&str) -> Option<SttEvent>) -> Self {
        Self::with_close(ws, map_text, closed_event)
    }

    /// A stream whose close reason can carry a verdict of its own
    /// (ElevenLabs benches a key closed for its account).
    pub(super) fn with_close(
        ws: WsReader,
        map_text: fn(&str) -> Option<SttEvent>,
        map_close: fn(String) -> SttEvent,
    ) -> Self {
        Self {
            ws,
            map_text,
            map_close,
        }
    }
}

#[async_trait]
impl ProviderStream for MappedStream {
    async fn recv_event(&mut self) -> Result<Option<SttEvent>, RecvError> {
        while let Some(frame) = self.ws.next_frame().await? {
            match frame {
                Frame::Text(text) => {
                    if let Some(ev) = (self.map_text)(text.as_str()) {
                        return Ok(Some(ev));
                    }
                    // Keep-alive, unknown type or empty transcript: keep reading.
                }
                Frame::Close(reason) => return Ok(Some((self.map_close)(reason))),
            }
        }
        Ok(None)
    }
}

/// The outbound half of an adapter that streams raw PCM16 binary frames and
/// ends the utterance with one text frame, after which the server flushes its
/// finals and closes the socket itself (AssemblyAI's `Terminate`, Deepgram's
/// `CloseStream`, DashScope's `finish-task`). Those three sinks differed in
/// nothing but those two frames.
pub(super) struct PcmSink {
    sink: WsSink,
    /// The frame that ends the utterance.
    commit: Utf8Bytes,
    /// The provider's documented no-audio keepalive frame, or `None` for a
    /// transport [`ping`] where it documents none.
    keepalive: Option<Utf8Bytes>,
}

#[async_trait]
impl ProviderSink for PcmSink {
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        send_pcm(&mut self.sink, pcm).await
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        send_text(&mut self.sink, self.commit.clone()).await
    }

    async fn keepalive(&mut self) -> Result<(), SendError> {
        match &self.keepalive {
            Some(frame) => send_text(&mut self.sink, frame.clone()).await,
            None => ping(&mut self.sink).await,
        }
    }

    async fn close(&mut self) -> Result<(), SendError> {
        // No-op: the commit frame already starts the server-side close, and a
        // client Close here would race the server's flush of its finals.
        Ok(())
    }
}

/// [`SttEvent::Closed`] for a peer close: the reason, or `None` when it gave
/// none.
pub(super) fn closed_event(reason: String) -> SttEvent {
    SttEvent::Closed((!reason.is_empty()).then_some(reason))
}

fn close_reason(frame: Option<CloseFrame>) -> String {
    frame.map(|f| f.reason.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reasonless_close_carries_no_reason() {
        assert!(matches!(
            closed_event(String::new()),
            SttEvent::Closed(None)
        ));
        assert!(matches!(
            closed_event("bye".into()),
            SttEvent::Closed(Some(r)) if r == "bye"
        ));
    }

    #[test]
    fn close_reason_reads_the_frame() {
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
        assert_eq!(close_reason(None), "");
        let frame = CloseFrame {
            code: CloseCode::Normal,
            reason: "done".into(),
        };
        assert_eq!(close_reason(Some(frame)), "done");
    }

    #[test]
    fn the_request_carries_the_credential_header() {
        let req = request("wss://example.invalid/v1", "xi-api-key", "k").unwrap();
        assert_eq!(req.headers()["xi-api-key"], "k");
        assert_eq!(req.uri(), "wss://example.invalid/v1");
    }

    #[test]
    fn a_key_that_cannot_be_a_header_is_a_connect_error() {
        let err = request("wss://example.invalid/v1", "Authorization", "bad\nkey").unwrap_err();
        assert!(err.0.starts_with("bad key header: "), "{}", err.0);
    }

    #[test]
    fn a_query_value_is_form_urlencoded() {
        let mut url = String::from("wss://example.invalid/v1?a=1");
        push_query(&mut url, "keyterm", "Quick Dictate&co");
        assert_eq!(
            url,
            "wss://example.invalid/v1?a=1&keyterm=Quick+Dictate%26co"
        );
    }

    #[test]
    fn json_terms_are_one_capped_parameter_and_absent_when_empty() {
        let mut url = String::from("u?a=1");
        push_json_terms(&mut url, "keyterms", &[], 2);
        assert_eq!(url, "u?a=1");
        let terms = ["one".to_string(), "two".into(), "three".into()];
        push_json_terms(&mut url, "keyterms", &terms, 2);
        assert_eq!(url, "u?a=1&keyterms=%5B%22one%22%2C%22two%22%5D");
    }

    /// The server end of a loopback socket.
    type ServerWs = WebSocketStream<TcpStream>;

    /// A real socket to a loopback server that runs `serve` on the one
    /// connection it accepts, for the tests of what goes over the wire.
    async fn loopback<T, Fut>(
        serve: impl FnOnce(ServerWs) -> Fut + Send + 'static,
    ) -> (WsConn, tokio::task::JoinHandle<T>)
    where
        Fut: std::future::Future<Output = T> + Send,
        T: Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            serve(tokio_tungstenite::accept_async(tcp).await.unwrap()).await
        });
        let conn = connect(&format!("ws://{addr}/"), "Authorization", "k")
            .await
            .unwrap();
        (conn, server)
    }

    /// The first `n` messages the server end receives.
    async fn receive(mut ws: ServerWs, n: usize) -> Vec<Message> {
        let mut got = Vec::new();
        while got.len() < n {
            match ws.next().await {
                Some(Ok(msg)) => got.push(msg),
                _ => break,
            }
        }
        got
    }

    #[tokio::test]
    async fn a_pcm_sink_sends_raw_audio_then_its_own_frames() {
        let (conn, server) = loopback(|ws| receive(ws, 3)).await;
        let mut session = pcm_session(
            conn,
            "{\"type\":\"CloseStream\"}",
            Some("{\"type\":\"KeepAlive\"}"),
            |_| None,
        );
        session.sink.send_audio(&[1, -2]).await.unwrap();
        session.sink.commit().await.unwrap();
        session.sink.keepalive().await.unwrap();
        session.sink.close().await.unwrap();
        assert_eq!(
            server.await.unwrap(),
            [
                Message::Binary(vec![1u8, 0, 0xFE, 0xFF].into()),
                Message::Text("{\"type\":\"CloseStream\"}".into()),
                Message::Text("{\"type\":\"KeepAlive\"}".into()),
            ]
        );
    }

    #[tokio::test]
    async fn a_pcm_sink_with_no_keepalive_frame_pings() {
        let (conn, server) = loopback(|ws| receive(ws, 1)).await;
        let mut session = pcm_session(conn, "{\"type\":\"Terminate\"}", None, |_| None);
        session.sink.keepalive().await.unwrap();
        let got = server.await.unwrap();
        assert!(
            matches!(&got[..], [Message::Ping(p)] if p.is_empty()),
            "{got:?}"
        );
    }

    #[tokio::test]
    async fn a_mapped_stream_reads_past_unmapped_frames_to_the_close() {
        let (conn, _server) = loopback(|mut ws| async move {
            ws.send(Message::Text("skip".into())).await.unwrap();
            ws.send(Message::Binary(vec![0u8; 4].into())).await.unwrap();
            ws.send(Message::Text("hit".into())).await.unwrap();
            ws.close(Some(CloseFrame {
                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
                reason: "bye".into(),
            }))
            .await
            .unwrap();
        })
        .await;
        let mut stream = pcm_session(conn, "", None, |t| {
            (t == "hit").then(|| SttEvent::Partial(t.to_string()))
        })
        .stream;
        assert!(matches!(
            stream.recv_event().await.unwrap(),
            Some(SttEvent::Partial(t)) if t == "hit"
        ));
        assert!(matches!(
            stream.recv_event().await.unwrap(),
            Some(SttEvent::Closed(Some(r))) if r == "bye"
        ));
        assert!(stream.recv_event().await.unwrap().is_none());
    }
}
