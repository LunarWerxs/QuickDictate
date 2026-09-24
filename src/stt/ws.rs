//! WebSocket plumbing shared by the streaming adapters (ElevenLabs, Deepgram,
//! AssemblyAI, OpenAI Realtime, DashScope).
//!
//! All five open the socket the same way (a URL plus one auth header), send
//! through the same error mapping, and read through the same loop: a text
//! frame goes to the adapter's own mapper, a Close frame ends the stream with
//! its reason, pings and binary frames are skipped. That loop used to be
//! pasted into every adapter, where a fix to one could silently miss the
//! others. What stays in each adapter is only its actual protocol: the URL,
//! the header's value, the frames it sends and how it reads the ones it gets.

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::provider::{i16_slice_as_bytes, ConnectError, RecvError, SendError, SttEvent};

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

    /// The whole `recv_event` for an adapter whose frames map one at a time
    /// with no state: the next frame `map_text` turns into an event, or the
    /// peer's close as [`closed_event`].
    pub(super) async fn recv(
        &mut self,
        map_text: fn(&str) -> Option<SttEvent>,
    ) -> Result<Option<SttEvent>, RecvError> {
        self.recv_with_close(map_text, closed_event).await
    }

    /// [`recv`](Self::recv) for an adapter whose close reason can carry a
    /// verdict of its own (ElevenLabs benches a key closed for its account).
    pub(super) async fn recv_with_close(
        &mut self,
        map_text: fn(&str) -> Option<SttEvent>,
        map_close: fn(String) -> SttEvent,
    ) -> Result<Option<SttEvent>, RecvError> {
        while let Some(frame) = self.next_frame().await? {
            match frame {
                Frame::Text(text) => {
                    if let Some(ev) = map_text(text.as_str()) {
                        return Ok(Some(ev));
                    }
                    // Keep-alive, unknown type or empty transcript: keep reading.
                }
                Frame::Close(reason) => return Ok(Some(map_close(reason))),
            }
        }
        Ok(None)
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
}
