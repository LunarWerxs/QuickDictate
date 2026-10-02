//! OpenAI Realtime transcription adapter (gpt-live-transcribe by default).
//!
//! Uses the Realtime WebSocket with `intent=transcription` (the plain
//! `/v1/audio/transcriptions` Whisper endpoint is batch-only). Audio is JSON
//! base64 PCM16 at **24 kHz**. On connect we push a `session.update` to select
//! the model and PCM16 format; transcription `.delta` events stream in
//! (accumulated into a live partial) and `.completed` carries the final text.
//!
//! The default moved from gpt-4o-transcribe to gpt-live-transcribe on
//! 2026-10-02, measured on four 16-20 s clips through this adapter's own
//! wire shape: final text 0.55 s after commit instead of 1.13 s, words while
//! the user is still talking instead of none, and no word errors (0.9%
//! before). `stt_model` still selects any other model.
//!
//! `live_test::live_openai` exercises it with a real `OPENAI_KEYS` key.

use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

use super::provider::{
    i16_slice_as_bytes, server_error_event, AudioFormat, ConnectError, ProviderSession,
    ProviderSink, ProviderStream, RecvError, SendError, SttEvent, SttProvider, SttSessionOpts,
};
use super::ws::{self, Frame, WsReader, WsSink};
use crate::keys::FailKind;

const WS_URL: &str = "wss://api.openai.com/v1/realtime?intent=transcription";
const DEFAULT_MODEL: &str = "gpt-live-transcribe";

/// `model` is the user's `stt_model` override, if any: it decides whether
/// words stream while they talk ([`SttProvider::streams_interim_text`]).
pub struct OpenAiProvider {
    pub model: Option<String>,
}

impl OpenAiProvider {
    fn model(&self) -> &str {
        self.model.as_deref().unwrap_or(DEFAULT_MODEL)
    }
}

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

    /// Depends on the model. With `turn_detection` null (manual commit, what
    /// this adapter uses) gpt-4o-transcribe sends NO delta while the user is
    /// speaking (measured 2026-09-11: the first landed 0.9 s after commit),
    /// so for it the pip spins like the batch providers instead of showing a
    /// "0" that never moves. The live models stream words during speech
    /// ([`streams_while_speaking`]), so their pip counts them.
    fn streams_interim_text(&self) -> bool {
        streams_while_speaking(self.model())
    }

    /// A key with no credit left connects and accepts audio; OpenAI says so
    /// only once it transcribes a committed buffer. One second of faint noise,
    /// committed, surfaces it at startup (once per key, recorded in
    /// `key_checks`) instead of on the user's first dictation, for a
    /// fraction of a cent.
    fn account_check_audio(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(1))
    }

    fn account_check_commits(&self) -> bool {
        true
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
        let model = opts.model.as_deref().unwrap_or(self.model());
        let mut conn = ws::connect(WS_URL, "Authorization", &format!("Bearer {key}")).await?;

        // Configure the transcription session (GA Realtime shape). Manual commit
        // (turn_detection = null) so we control end-of-utterance.
        let update = build_session_update(model, opts).to_string();
        ws::send_setup(&mut conn, "session.update", update).await?;

        let (sink, ws) = ws::split(conn);
        Ok(ProviderSession {
            sink: Box::new(OpenAiSink { sink }),
            stream: Box::new(OpenAiStream {
                ws,
                accum: String::new(),
            }),
        })
    }
}

/// Whether `model` is OpenAI's live-transcription model, which takes a
/// different `transcription` object (see [`build_session_update`]).
fn is_live_model(model: &str) -> bool {
    model.starts_with("gpt-live-transcribe")
}

/// Whether `model` sends transcript deltas while the user is still talking
/// with manual commit. Measured 2026-10-02 on four 16-20 s clips: the live
/// model and gpt-realtime-whisper send their first delta about 0.5-1.2 s
/// into speech; gpt-4o-transcribe, gpt-4o-mini-transcribe and gpt-transcribe
/// send nothing until the commit.
fn streams_while_speaking(model: &str) -> bool {
    is_live_model(model) || model.starts_with("gpt-realtime-whisper")
}

/// The live model's latency/accuracy dial (`minimal` .. `xhigh`). Measured
/// 2026-10-02 against the same four clips: `low` returned the final 0.55 s
/// after commit with its first delta 0.76 s into speech and no word errors;
/// `minimal` was 0.38 s / 0.46 s but misheard a word.
const LIVE_DELAY: &str = "low";

/// Build the `session.update` payload for `model`/`opts`. Pure
/// (fixture-tested); `connect` just serializes and sends this.
///
/// `gpt-live-transcribe` takes `languages` (a list of bare codes; the
/// singular `language` alongside it is rejected), `delay`, and the
/// vocabulary as `keywords` -- one term each; a term holding `<`, `>` or a
/// line break fails the whole session, so such terms are dropped. The older
/// models take `language` and a free-text `prompt`, which
/// `vocabulary_prompt()` builds. Either way the vocabulary field is added
/// only when the vocabulary is non-empty.
fn build_session_update(model: &str, opts: &SttSessionOpts) -> serde_json::Value {
    let language = opts.language.trim();
    let detect = language.is_empty() || language.eq_ignore_ascii_case("auto");
    let mut transcription = json!({ "model": model });
    if is_live_model(model) {
        if !detect {
            transcription["languages"] = json!([language]);
        }
        transcription["delay"] = json!(LIVE_DELAY);
        let keywords: Vec<&str> = opts
            .custom_vocabulary
            .iter()
            .map(|term| term.trim())
            .filter(|term| !term.is_empty() && !term.contains(['<', '>', '\r', '\n']))
            .collect();
        if !keywords.is_empty() {
            transcription["keywords"] = json!(keywords);
        }
    } else {
        if !detect {
            transcription["language"] = json!(language);
        }
        let prompt = opts.vocabulary_prompt();
        if !prompt.is_empty() {
            transcription["prompt"] = json!(prompt);
        }
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

    const OLDER_MODEL: &str = "gpt-4o-transcribe";

    fn transcription(model: &str, opts: &SttSessionOpts) -> serde_json::Value {
        build_session_update(model, opts)["session"]["audio"]["input"]["transcription"].clone()
    }

    #[test]
    fn empty_vocabulary_omits_prompt_field() {
        let t = transcription(OLDER_MODEL, &test_opts(vec![]));
        assert!(t.get("prompt").is_none());
        assert_eq!(t["model"], OLDER_MODEL);
        assert_eq!(t["language"], "en");
    }

    #[test]
    fn vocabulary_sets_prompt_field() {
        let t = transcription(OLDER_MODEL, &test_opts(vec!["Anthropic", "Claude"]));
        assert_eq!(t["prompt"], "Anthropic, Claude");
    }

    // Contract: the live model's session.update is the shape its API accepts.
    // Regression (each verified live 2026-10-02): sending `language` with
    // `languages` is rejected, and one keyword holding `<` fails the whole
    // session, so either mistake would kill every OpenAI dictation.
    #[test]
    fn live_model_takes_languages_delay_and_clean_keywords() {
        let t = transcription(
            DEFAULT_MODEL,
            &test_opts(vec!["Anthropic", " Claude ", "a<b", "two\nlines", ""]),
        );
        assert_eq!(t["model"], "gpt-live-transcribe");
        assert_eq!(t["languages"], json!(["en"]));
        assert!(t.get("language").is_none());
        assert_eq!(t["delay"], LIVE_DELAY);
        assert_eq!(t["keywords"], json!(["Anthropic", "Claude"]));
        assert!(t.get("prompt").is_none());

        let none = transcription(DEFAULT_MODEL, &test_opts(vec![]));
        assert!(none.get("keywords").is_none());
    }

    #[test]
    fn auto_language_sends_no_language_hint_to_either_shape() {
        let mut opts = test_opts(vec![]);
        opts.language = "auto".into();
        assert!(transcription(DEFAULT_MODEL, &opts)
            .get("languages")
            .is_none());
        assert!(transcription(OLDER_MODEL, &opts).get("language").is_none());
    }

    #[test]
    fn only_streaming_models_promise_words_while_speaking() {
        let pip = |model: Option<&str>| {
            OpenAiProvider {
                model: model.map(String::from),
            }
            .streams_interim_text()
        };
        assert!(pip(None));
        assert!(pip(Some("gpt-realtime-whisper")));
        assert!(!pip(Some(OLDER_MODEL)));
        assert!(!pip(Some("gpt-transcribe")));
    }
}
