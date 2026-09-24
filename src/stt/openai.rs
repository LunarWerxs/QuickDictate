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
use tokio_tungstenite::tungstenite::Message;

use super::provider::{
    i16_slice_as_bytes, server_error_event, AudioFormat, ConnectError, ProviderSession,
    ProviderSink, ProviderStream, RecvError, SendError, SttEvent, SttProvider, SttSessionOpts,
};
use super::ws::{self, Frame, WsReader, WsSink};
use crate::keys::FailKind;

const WS_URL: &str = "wss://api.openai.com/v1/realtime?intent=transcription";
const DEFAULT_MODEL: &str = "gpt-4o-transcribe";

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

    /// Measured against the live API on 2026-09-11: with `turn_detection`
    /// null (manual commit, what this adapter uses) OpenAI sends NO delta
    /// while the user is speaking. The first one landed 0.9 s after commit
    /// and the whole transcript streamed in over the next 100 ms. So the pip
    /// spins here, like the batch providers, instead of showing a "0" that
    /// never moves for the length of the dictation.
    fn streams_interim_text(&self) -> bool {
        false
    }

    fn final_transcript_timeout(&self) -> std::time::Duration {
        // Realtime `.completed` has taken a little over two seconds in field
        // logs. The socket intentionally remains open after commit, so give
        // the server enough room to replace the last delta with its full final.
        std::time::Duration::from_secs(5)
    }

    async fn connect(
        &self,
        key: &str,
        opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        let model = opts.model.as_deref().unwrap_or(DEFAULT_MODEL);
        let mut conn = ws::connect(WS_URL, "Authorization", &format!("Bearer {key}")).await?;

        // Configure the transcription session (GA Realtime shape). Manual commit
        // (turn_detection = null) so we control end-of-utterance.
        let update = build_session_update(model, opts).to_string();
        conn.send(Message::Text(update.into()))
            .await
            .map_err(|e| ConnectError(format!("session.update send: {e}")))?;

        let (sink, stream) = conn.split();
        Ok(ProviderSession {
            sink: Box::new(OpenAiSink { sink }),
            stream: Box::new(OpenAiStream {
                ws: WsReader::new(stream),
                accum: String::new(),
            }),
        })
    }
}

/// Build the `session.update` payload for `model`/`opts`. Pure
/// (fixture-tested); `connect` just serializes and sends this. Unlike the
/// term-list biasing knobs the other providers use, `gpt-4o-transcribe`
/// takes a free-text `prompt` -- `vocabulary_prompt()` joins the vocabulary
/// into one. The field is added only when the vocabulary is non-empty, so a
/// user with none set gets the exact same request as before this existed.
fn build_session_update(model: &str, opts: &SttSessionOpts) -> serde_json::Value {
    let mut transcription = json!({ "model": model, "language": opts.language });
    let prompt = opts.vocabulary_prompt();
    if !prompt.is_empty() {
        transcription["prompt"] = json!(prompt);
    }
    json!({
        "type": "session.update",
        "session": {
            "type": "transcription",
            "audio": {
                "input": {
                    "format": { "type": "audio/pcm", "rate": opts.sample_rate },
                    "transcription": transcription,
                    "turn_detection": null
                }
            }
        }
    })
}

struct OpenAiSink {
    sink: WsSink,
}

#[async_trait]
impl ProviderSink for OpenAiSink {
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        let audio = base64::engine::general_purpose::STANDARD.encode(i16_slice_as_bytes(pcm));
        let msg = json!({ "type": "input_audio_buffer.append", "audio": audio }).to_string();
        ws::send_text(&mut self.sink, msg).await
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        ws::send_text(&mut self.sink, "{\"type\":\"input_audio_buffer.commit\"}").await
    }

    async fn keepalive(&mut self) -> Result<(), SendError> {
        // The realtime socket has no short audio-idle close, but a transport WS
        // ping keeps any connection idle timer from firing during a long silent
        // tail.
        ws::ping(&mut self.sink).await
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
    ws: WsReader,
    /// Delta events are incremental; we accumulate them into the live partial.
    accum: String,
}

#[async_trait]
impl ProviderStream for OpenAiStream {
    async fn recv_event(&mut self) -> Result<Option<SttEvent>, RecvError> {
        while let Some(frame) = self.ws.next_frame().await? {
            let text = match frame {
                Frame::Text(text) => text,
                Frame::Close(reason) => return Ok(Some(ws::closed_event(reason))),
            };
            if let Some(answer) = self.apply(classify_event(&text), &text) {
                return Ok(answer);
            }
        }
        Ok(None)
    }
}

impl OpenAiStream {
    /// Fold one classified frame into the stream's state. `Some(answer)` is
    /// what this `recv_event` call returns; `None` means keep reading.
    fn apply(&mut self, event: OaEvent, frame: &str) -> Option<Option<SttEvent>> {
        match event {
            OaEvent::Delta(d) => {
                self.accum.push_str(&d);
                let trimmed = self.accum.trim();
                (!trimmed.is_empty()).then(|| Some(SttEvent::Partial(trimmed.to_string())))
            }
            OaEvent::Completed(t) => {
                self.accum.clear();
                // One QuickDictate socket carries one utterance. Mark the
                // inbound half drained so the generic receiver exits after
                // delivering this final instead of waiting for a WS close
                // that OpenAI does not send here.
                self.ws.finish();
                let t = t.trim();
                Some((!t.is_empty()).then(|| SttEvent::Committed(t.to_string())))
            }
            OaEvent::Created => Some(Some(SttEvent::SessionStarted)),
            OaEvent::Failure(kind) => Some(Some(server_error_event("openai", kind, frame))),
            OaEvent::Other => None,
        }
    }
}

#[derive(Debug, PartialEq)]
enum OaEvent {
    Delta(String),
    Completed(String),
    Created,
    Failure(FailKind),
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
        // The transcription of a committed buffer failed. THIS is where a key
        // with no credit left is reported (`insufficient_quota` /
        // `credit_balance_exhausted`), not in a top-level `error` frame --
        // measured against the live API on 2026-09-11. Ignoring it (as this
        // did) left the press waiting out its whole timeout in silence: no
        // words, no error pip, no rotation to the next key, and the dead key
        // credited as alive. A second key that would have worked was never
        // tried.
        "conversation.item.input_audio_transcription.failed" | "error" => {
            OaEvent::Failure(classify_error(m.error.as_ref()))
        }
        "session.created" => OaEvent::Created,
        _ => OaEvent::Other,
    }
}

/// What an OpenAI error says about the key, read from its `code` and then its
/// `type`, never its prose. The generic client-error type is literally
/// `invalid_request_error`, so the old substring read ("invalid") marked
/// every key Dead over a request OpenAI merely rejected: an unknown model, an
/// unsupported language, a commit of under 100 ms of audio. Anything not named
/// here, or a failure with no error object at all, is the request's or the
/// server's problem: `Transient`, which surfaces as a provider failure.
fn classify_error(err: Option<&OaError>) -> FailKind {
    let Some(err) = err else {
        return FailKind::Transient;
    };
    [err.code.as_deref(), err.err_type.as_deref()]
        .into_iter()
        .flatten()
        .find_map(key_fail_kind)
        .unwrap_or(FailKind::Transient)
}

/// The error codes and types that are a verdict on the key itself.
fn key_fail_kind(code: &str) -> Option<FailKind> {
    match code {
        "invalid_api_key" | "account_deactivated" => Some(FailKind::Invalid),
        "insufficient_quota" | "credit_balance_exhausted" | "billing_hard_limit_reached" => {
            Some(FailKind::Exhausted)
        }
        "rate_limit_exceeded" => Some(FailKind::RateLimit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn an_out_of_credit_key_is_reported_through_the_transcription_failed_event() {
        // Captured verbatim from the live API, 2026-09-11. Before this was
        // handled the session simply went quiet and the key was never rotated.
        let e = r#"{"type":"conversation.item.input_audio_transcription.failed","item_id":"item_x","content_index":0,"error":{"type":"insufficient_quota","code":"credit_balance_exhausted","message":"You have no credits remaining. Add credits to continue using the API at https://platform.openai.com/settings/organization/billing/."}}"#;
        assert_eq!(classify_event(e), OaEvent::Failure(FailKind::Exhausted));
    }

    #[test]
    fn a_transcription_failure_with_no_error_object_is_still_a_failure() {
        let e =
            r#"{"type":"conversation.item.input_audio_transcription.failed","item_id":"item_x"}"#;
        assert_eq!(classify_event(e), OaEvent::Failure(FailKind::Transient));
    }

    #[test]
    fn a_rejected_request_is_not_a_dead_key() {
        // The generic client-error type contains "invalid"; only the code
        // may condemn the key. A model OpenAI does not have, and a commit
        // with under 100 ms of audio:
        for e in [
            r#"{"type":"error","error":{"type":"invalid_request_error","code":"invalid_value","message":"Invalid value: 'gpt-nope'."}}"#,
            r#"{"type":"error","error":{"type":"invalid_request_error","code":"input_audio_buffer_commit_empty","message":"buffer too small"}}"#,
        ] {
            assert_eq!(
                classify_event(e),
                OaEvent::Failure(FailKind::Transient),
                "{e}"
            );
        }
        let rate = r#"{"type":"error","error":{"type":"requests","code":"rate_limit_exceeded","message":"Rate limit reached"}}"#;
        assert_eq!(classify_event(rate), OaEvent::Failure(FailKind::RateLimit));
    }

    fn test_opts(vocab: Vec<&str>) -> SttSessionOpts {
        SttSessionOpts {
            language: "en".into(),
            sample_rate: 24_000,
            model: None,
            custom_vocabulary: vocab.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn empty_vocabulary_omits_prompt_field() {
        let update = build_session_update(DEFAULT_MODEL, &test_opts(vec![]));
        let transcription = &update["session"]["audio"]["input"]["transcription"];
        assert!(transcription.get("prompt").is_none());
        assert_eq!(transcription["model"], DEFAULT_MODEL);
        assert_eq!(transcription["language"], "en");
    }

    #[test]
    fn vocabulary_sets_prompt_field() {
        let update = build_session_update(DEFAULT_MODEL, &test_opts(vec!["Anthropic", "Claude"]));
        let transcription = &update["session"]["audio"]["input"]["transcription"];
        assert_eq!(transcription["prompt"], "Anthropic, Claude");
    }
}
