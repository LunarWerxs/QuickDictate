//! AssemblyAI Universal-Streaming (v3) adapter.
//!
//! Wire shape is close to Deepgram: raw binary PCM16 frames, JSON events in.
//! Auth is the API key in a bare `Authorization` header. The v3 protocol emits
//! `Begin` (session start) and `Turn` events; `end_of_turn=true` marks a
//! finalized turn (→ `Committed`), interim turns are `Partial`. End the session
//! with `{"type":"Terminate"}`, after which the server flushes and closes.
//!
//! The `use` list below is deepgram.rs's, token for token: both are raw-PCM
//! WebSocket adapters over `ws`. A shared prelude is not worth it for an import
//! list; revisit if a third adapter needs the same set.

use async_trait::async_trait;
use serde::Deserialize;

use super::provider::{
    classify_by_substring, server_error_event, AudioFormat, ConnectError, ProviderSession,
    SttEvent, SttProvider, SttSessionOpts,
};
use super::ws;

const WS_URL: &str = "wss://streaming.assemblyai.com/v3/ws";
/// Ends the session: the server flushes the final turn(s) and closes the
/// socket itself. The streaming API documents no no-audio keepalive, so the
/// sink keeps an idle socket open with a transport ping.
const TERMINATE: &str = "{\"type\":\"Terminate\"}";
/// v3 streaming hard-errors above 100 keyterms per session.
const MAX_KEYTERMS: usize = 100;

pub struct AssemblyAiProvider;

#[async_trait]
// Same shape as deepgram.rs's impl. A trait default for the 16 kHz format and
// stall recovery is not worth it: each provider states its own wire format
// (OpenAI's is 24 kHz), and a default would let a new one forget. Revisit if the
// trait grows a per-provider config struct.
impl SttProvider for AssemblyAiProvider {
    fn id(&self) -> &'static str {
        "assemblyai"
    }

    /// Measured 2026-09-11 (`live_assemblyai` at realtime pace): first partial
    /// ~1.0 s after speech starts, then one every ~1.2 s, and partials keep
    /// coming after each committed sentence. Comfortably inside the stall
    /// watchdog's 5 s window, so a server that goes quiet is replaced.
    fn supports_stall_recovery(&self) -> bool {
        true
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
        let conn = ws::connect(&build_url(opts), "Authorization", key).await?;
        Ok(ws::pcm_session(conn, TERMINATE, None, map_frame))
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
    ws::push_json_terms(
        &mut url,
        "keyterms_prompt",
        &opts.custom_vocabulary,
        MAX_KEYTERMS,
    );
    url
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
        // Only a verdict on the key rotates it; anything else is the
        // server's problem and must not end the press.
        "Error" => Some(server_error_event(
            "assemblyai",
            classify_by_substring(text),
            text,
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

    #[test]
    fn a_server_error_keeps_the_press_alive_but_a_key_error_rotates() {
        assert!(matches!(
            map_frame(r#"{"type":"Error","error":"internal server error"}"#),
            Some(SttEvent::ProviderFailure(m)) if m.starts_with("assemblyai sent")
        ));
        assert!(matches!(
            map_frame(r#"{"type":"Error","error":"401 unauthorized"}"#),
            Some(SttEvent::KeyFailure(crate::keys::FailKind::Invalid))
        ));
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
