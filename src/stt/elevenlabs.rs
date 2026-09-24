//! ElevenLabs Scribe v2 realtime adapter.
//!
//! Extracted **verbatim in behavior** from the original hardcoded `stt.rs`:
//! same WS URL/model, `xi-api-key` header, base64-JSON audio envelope, manual
//! commit + pre-close delay, and `message_type` → event mapping. Nothing about
//! the ElevenLabs wire protocol lives outside this file.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

use super::provider::{
    i16_slice_as_bytes, provider_failure, summarize_frame, AudioFormat, ConnectError,
    ProviderSession, ProviderSink, ProviderStream, RecvError, SendError, SttEvent, SttProvider,
    SttSessionOpts,
};
use super::ws::{self, WsConn, WsReader, WsSink};
use crate::keys::FailKind;

const WS_URL: &str = "wss://api.elevenlabs.io/v1/speech-to-text/realtime";
const MODEL_ID: &str = "scribe_v2_realtime";
/// Scribe v2 Realtime caps keyterm biasing at 50 terms (20 characters each).
const MAX_KEYTERMS: usize = 50;

/// How long the send task waits after the manual commit before sending the
/// WebSocket Close. Some servers race Close against in-flight commit
/// processing and skip the committed_transcript response. 300 ms gives
/// ElevenLabs enough time to flush the final transcript.
const PRE_CLOSE_DELAY: Duration = Duration::from_millis(300);

/// Everything of an audio frame before its base64 samples.
const AUDIO_FRAME_HEAD: &str = "{\"message_type\":\"input_audio_chunk\",\"audio_base_64\":\"";

pub struct ElevenLabsProvider;

#[async_trait]
impl SttProvider for ElevenLabsProvider {
    fn id(&self) -> &'static str {
        "elevenlabs"
    }

    fn required_audio_format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: 16_000,
        }
    }

    /// Scribe's LM prior finalizes a trailing question into a phantom short
    /// "answer" ("Yes.") at end-of-stream. Enable the runner's guard so those
    /// zero-speech post-release commits are dropped instead of pasted. See
    /// the phantom-finalization regression tests in `stt::tests`.
    fn suppress_phantom_finalization(&self) -> bool {
        true
    }

    /// Observed 2026-09-11: a session sends one VAD commit and then nothing
    /// at all for the rest of the press -- no partial, no commit, no close,
    /// not even an answer to the final manual commit -- while the socket
    /// keeps accepting audio. A fresh connection on the same key answers
    /// within ~200 ms and transcribes the replayed segment normally.
    fn supports_stall_recovery(&self) -> bool {
        true
    }

    /// An account that never accepted the Scribe terms connects and streams
    /// normally, then is closed with `unaccepted_terms` once about ten seconds
    /// of AUDIO have gone in, whether that audio took ten seconds or one
    /// (measured 2026-09-18: 10.3-10.5 s of audio, at real time and at 10x).
    /// Twelve clears that with room; good keys took thirteen without a word.
    fn account_check_audio(&self) -> Option<Duration> {
        Some(Duration::from_secs(12))
    }

    async fn connect(
        &self,
        key: &str,
        opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        let model = opts.model.as_deref().unwrap_or(MODEL_ID);
        let conn = connect_with_vocabulary_fallback(key, model, opts).await?;
        let (sink, stream) = conn.split();
        Ok(ProviderSession {
            sink: Box::new(ElevenLabsSink {
                sink,
                frame_tail: audio_frame_tail(opts.sample_rate),
                sample_rate: opts.sample_rate,
            }),
            stream: Box::new(ElevenLabsStream {
                ws: WsReader::new(stream),
            }),
        })
    }
}

/// Two-shot connect, and only when a custom vocabulary is actually set.
/// The `keyterms` parameter's name and limits come from ElevenLabs' own
/// AsyncAPI spec, but their docs show no literal wire example, so the
/// encoding here is inferred from the identical shape AssemblyAI uses.
/// ElevenLabs is also the DEFAULT provider. If that guess is wrong the
/// server rejects the upgrade, and a user who typed a few words into
/// the vocabulary box would find dictation simply broken. So: if the
/// handshake fails with a vocabulary attached, drop it and try once
/// more. Losing the biasing is a small regression; losing dictation is
/// not. Remove this fallback once a live run confirms the encoding.
async fn connect_with_vocabulary_fallback(
    key: &str,
    model: &str,
    opts: &SttSessionOpts,
) -> Result<WsConn, ConnectError> {
    let attempts: &[bool] = if opts.custom_vocabulary.is_empty() {
        &[false]
    } else {
        &[true, false]
    };
    let mut last_err: Option<ConnectError> = None;
    for (i, &with_vocabulary) in attempts.iter().enumerate() {
        let request = ws::request(
            &attempt_url(model, opts, with_vocabulary),
            "xi-api-key",
            key,
        )?;
        match ws::open(request).await {
            Ok(conn) => {
                if i > 0 {
                    tracing::warn!(
                        "elevenlabs: the server rejected the connection with keyterms \
                         attached; reconnected without recognition biasing"
                    );
                }
                return Ok(conn);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or(ConnectError("ws connect failed".into())))
}

/// The URL for one connect attempt: with the vocabulary as `keyterms`, or
/// the same session without it.
fn attempt_url(model: &str, opts: &SttSessionOpts, with_vocabulary: bool) -> String {
    if with_vocabulary {
        build_url(model, opts)
    } else {
        build_url(
            model,
            &SttSessionOpts {
                custom_vocabulary: Vec::new(),
                ..opts.clone()
            },
        )
    }
}

/// Build the realtime WebSocket URL for `model`/`opts`. Pure
/// (fixture-tested); `connect` just calls this. `keyterms` biases the model
/// toward specific terms; realtime caps it at 50 terms of up to 20
/// characters each. ElevenLabs' own docs don't show a literal wire example
/// for this query param, so this follows the same JSON-array-in-one-param
/// encoding the sibling `keyterms_prompt`/AssemblyAI knob uses for the
/// identical "array of terms in a WS query string" shape.
fn build_url(model: &str, opts: &SttSessionOpts) -> String {
    let mut url = format!(
        "{WS_URL}?language_code={lang}&model_id={model}&audio_format=pcm_16000&commit_strategy=vad",
        lang = opts.language,
    );
    if !opts.custom_vocabulary.is_empty() {
        let terms: Vec<&str> = opts
            .custom_vocabulary
            .iter()
            .take(MAX_KEYTERMS)
            .map(String::as_str)
            .collect();
        let json = serde_json::to_string(&terms).unwrap_or_default();
        url.push_str("&keyterms=");
        url.push_str(&url::form_urlencoded::byte_serialize(json.as_bytes()).collect::<String>());
    }
    url
}

struct ElevenLabsSink {
    sink: WsSink,
    /// Everything of an audio frame after its samples; it depends only on
    /// the session's sample rate, so it is formatted once, not per chunk.
    frame_tail: String,
    sample_rate: u32,
}

/// The part of every audio frame after its base64 samples.
fn audio_frame_tail(sample_rate: u32) -> String {
    format!("\",\"sample_rate\":{sample_rate}}}")
}

/// One audio frame, built straight into a String of exactly its final
/// length. The socket takes ownership of it, and a String whose length is its
/// capacity converts without another allocation, so each chunk costs one
/// allocation and one base64 pass. A reused scratch buffer could not save
/// that: the socket needs a frame it owns, so it had to be cloned per chunk.
fn audio_frame(pcm: &[i16], tail: &str) -> String {
    let bytes = i16_slice_as_bytes(pcm);
    // Padded base64: four characters per started group of three bytes.
    let encoded_len = bytes.len().div_ceil(3) * 4;
    let mut frame = String::with_capacity(AUDIO_FRAME_HEAD.len() + encoded_len + tail.len());
    frame.push_str(AUDIO_FRAME_HEAD);
    base64::engine::general_purpose::STANDARD.encode_string(bytes, &mut frame);
    frame.push_str(tail);
    frame
}

#[async_trait]
impl ProviderSink for ElevenLabsSink {
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        ws::send_text(&mut self.sink, audio_frame(pcm, &self.frame_tail)).await
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        let commit = json!({
            "message_type": "input_audio_chunk",
            "audio_base_64": "",
            "sample_rate": self.sample_rate,
            "commit": true,
        })
        .to_string();
        ws::send_text(&mut self.sink, commit).await
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
        ws::send_text(&mut self.sink, ka).await
    }

    async fn close(&mut self) -> Result<(), SendError> {
        // Match the original: give ElevenLabs a beat to flush the final
        // transcript before the Close races in.
        tokio::time::sleep(PRE_CLOSE_DELAY).await;
        ws::send(&mut self.sink, Message::Close(None)).await
    }
}

struct ElevenLabsStream {
    ws: WsReader,
}

#[async_trait]
impl ProviderStream for ElevenLabsStream {
    async fn recv_event(&mut self) -> Result<Option<SttEvent>, RecvError> {
        // Non-JSON keep-alive, unknown type, or empty transcript map to
        // nothing, so the reader keeps going past them.
        self.ws.recv_with_close(map_frame, close_event).await
    }
}

/// A close reason that blames the key's account (out of credit, terms never
/// accepted) is a KeyFailure, so the runner rotates to the next key and
/// carries the text. Any other close is a plain close.
fn close_event(reason: String) -> SttEvent {
    match close_key_failure(&reason) {
        Some(kind) => SttEvent::KeyFailure(kind),
        None => ws::closed_event(reason),
    }
}

#[derive(Deserialize)]
struct Incoming {
    message_type: Option<String>,
    text: Option<String>,
    committed_transcript: Option<String>,
}

/// Pure text-frame → event mapping. Returns `None` for frames the runner should
/// ignore (non-JSON, unknown `message_type`, empty transcript). Kept separate
/// from `recv_event` so it can be fixture-tested without a live socket.
fn map_frame(text: &str) -> Option<SttEvent> {
    let parsed: Incoming = serde_json::from_str(text).ok()?;
    let message_type = parsed.message_type.as_deref().unwrap_or("");
    match message_type {
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
        _ => map_control_frame(message_type, text),
    }
}

/// Every frame that is not a transcript: a verdict on the key, an advisory,
/// or an error. Checked in that order.
fn map_control_frame(message_type: &str, text: &str) -> Option<SttEvent> {
    if let Some(kind) = frame_key_failure(message_type) {
        return Some(SttEvent::KeyFailure(kind));
    }
    match message_type {
        // Advisory frames the spec documents. None of them ends the session
        // or blames the key, but a session that went quiet after one is only
        // diagnosable if the log shows it, so they are never dropped silently.
        "warning"
        | "commit_throttled"
        | "insufficient_audio_activity"
        | "queue_overflow"
        | "resource_exhausted"
        | "session_time_limit_exceeded" => {
            tracing::warn!("elevenlabs: server sent {}", summarize_frame(text));
            None
        }
        // A billing-flavoured body under any type: the key is out of credit.
        _ if is_account_exhausted(text) => Some(SttEvent::KeyFailure(FailKind::Exhausted)),
        // Every other error-shaped frame (`error`, `transcriber_error`,
        // `input_error`, `invalid_request`, ...) is the SERVER's problem, not
        // the credential's. It used to be a transient key failure, which
        // benched a working key and aborted the attempt -- dropping the
        // sentences it was holding. As a provider failure the press keeps
        // going: if the server then closes or goes quiet, the stall watchdog
        // replaces the connection with the text intact, and the failure is
        // only surfaced if the press ends having delivered nothing.
        t if t.contains("error") || t == "invalid_request" || t == "chunk_size_exceeded" => {
            Some(provider_failure("elevenlabs", text))
        }
        "" => None,
        _ => {
            tracing::debug!("elevenlabs: ignoring frame {}", summarize_frame(text));
            None
        }
    }
}

/// The frame types that are a verdict on the key itself.
fn frame_key_failure(message_type: &str) -> Option<FailKind> {
    match message_type {
        "quota_exceeded" => Some(FailKind::Exhausted),
        "auth_error" | "invalid_api_key" | "unauthorized" => Some(FailKind::Invalid),
        "unaccepted_terms" => {
            warn_unaccepted_terms();
            Some(FailKind::Invalid)
        }
        // `rate_limited` is the name in ElevenLabs' own AsyncAPI spec; the
        // other two are kept for whatever older builds observed.
        "rate_limited" | "rate_limit_exceeded" | "too_many_requests" => Some(FailKind::RateLimit),
        _ => None,
    }
}

/// What a close-frame reason says about the key, if anything.
///
/// `unaccepted_terms` is the account behind the key never having accepted the
/// Scribe terms in the ElevenLabs dashboard. The server lets such a session
/// run for about ten seconds and then closes it with that reason, on every
/// press, so it has to bench the key: left as a plain close it kept the key at
/// the head of the pool, and every press longer than ten seconds lost the rest
/// of what was said (2026-09-18, a new key added at #1).
fn close_key_failure(reason: &str) -> Option<FailKind> {
    if is_account_exhausted(reason) {
        return Some(FailKind::Exhausted);
    }
    if reason.to_ascii_lowercase().contains("unaccepted_terms") {
        warn_unaccepted_terms();
        return Some(FailKind::Invalid);
    }
    None
}

/// The one log line that tells the owner what to do about `unaccepted_terms`.
/// The key's number comes from the runner's own "key #N of M" lines around it.
fn warn_unaccepted_terms() {
    tracing::warn!(
        "elevenlabs: this key's account has not accepted the Scribe terms (unaccepted_terms); \
         the key is benched until someone signs in to that ElevenLabs account and accepts them"
    );
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
        // A billing-flavored error body → Exhausted (the key's fault); any
        // other error shape is the server's problem and never benches a key.
        assert!(matches!(
            map_frame(r#"{"message_type":"error","text":"insufficient_funds"}"#),
            Some(SttEvent::KeyFailure(FailKind::Exhausted))
        ));
        for frame in [
            r#"{"message_type":"internal_error"}"#,
            r#"{"message_type":"transcriber_error","error":"model crashed"}"#,
            r#"{"message_type":"input_error","error":"bad chunk"}"#,
            r#"{"message_type":"invalid_request"}"#,
        ] {
            assert!(
                matches!(map_frame(frame), Some(SttEvent::ProviderFailure(m)) if m.contains("elevenlabs sent")),
                "{frame} must be a provider failure"
            );
        }
    }

    #[test]
    fn non_json_and_unknown_are_ignored() {
        assert!(map_frame("not json at all").is_none());
        assert!(map_frame(r#"{"message_type":"heartbeat"}"#).is_none());
    }

    #[test]
    fn unaccepted_terms_benches_the_key_as_a_frame_or_a_close_reason() {
        // The 2026-09-18 failure arrived as the close reason; the spec also
        // names it as an error frame. Either way the key has to rotate out.
        assert!(matches!(
            map_frame(r#"{"message_type":"unaccepted_terms","error":"accept the terms"}"#),
            Some(SttEvent::KeyFailure(FailKind::Invalid))
        ));
        assert_eq!(
            close_key_failure("unaccepted_terms"),
            Some(FailKind::Invalid)
        );
        assert_eq!(
            close_key_failure("quota_exceeded"),
            Some(FailKind::Exhausted)
        );
        // A reasonless close or a throttle says nothing about the key.
        assert_eq!(close_key_failure(""), None);
        assert_eq!(close_key_failure("commit_throttled"), None);
    }

    #[test]
    fn spec_named_rate_limit_rotates_the_key() {
        assert!(matches!(
            map_frame(r#"{"message_type":"rate_limited","error":"slow down"}"#),
            Some(SttEvent::KeyFailure(FailKind::RateLimit))
        ));
    }

    #[test]
    fn advisory_frames_are_logged_not_acted_on() {
        // The session keeps running and the key keeps its standing.
        for t in [
            "warning",
            "commit_throttled",
            "insufficient_audio_activity",
            "queue_overflow",
            "resource_exhausted",
            "session_time_limit_exceeded",
        ] {
            let frame = format!(r#"{{"message_type":"{t}","error":"x"}}"#);
            assert!(map_frame(&frame).is_none(), "{t} must not become an event");
        }
    }

    #[test]
    fn audio_frame_is_the_documented_envelope_at_exactly_its_length() {
        let frame = audio_frame(&[1, 2, 3], &audio_frame_tail(16_000));
        assert_eq!(
            frame,
            r#"{"message_type":"input_audio_chunk","audio_base_64":"AQACAAMA","sample_rate":16000}"#
        );
        // Length == capacity is what lets the socket take the String without
        // another allocation.
        assert_eq!(frame.len(), frame.capacity());
        // A length that is not a multiple of three still sizes exactly.
        let frame = audio_frame(&[7; 1601], &audio_frame_tail(16_000));
        assert_eq!(frame.len(), frame.capacity());
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
        let url = build_url(MODEL_ID, &test_opts(vec![]));
        assert_eq!(
            url,
            format!("{WS_URL}?language_code=en&model_id={MODEL_ID}&audio_format=pcm_16000&commit_strategy=vad")
        );
    }

    #[test]
    fn vocabulary_adds_keyterms_as_json_array() {
        let url = build_url(MODEL_ID, &test_opts(vec!["Anthropic", "Scribe"]));
        let expected_json = serde_json::to_string(&["Anthropic", "Scribe"]).unwrap();
        let expected_encoded: String =
            url::form_urlencoded::byte_serialize(expected_json.as_bytes()).collect();
        assert!(url.ends_with(&format!("&keyterms={expected_encoded}")));
    }

    #[test]
    fn keyterms_are_capped_at_50_terms() {
        let many: Vec<String> = (0..80).map(|i| format!("term{i}")).collect();
        let opts = SttSessionOpts {
            language: "en".into(),
            sample_rate: 16_000,
            model: None,
            custom_vocabulary: many,
        };
        let url = build_url(MODEL_ID, &opts);
        assert!(url.contains("term49"));
        assert!(!url.contains("term50"));
    }
}
