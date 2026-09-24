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
use tokio_tungstenite::tungstenite::Message;

use super::provider::{
    classify_by_substring, server_error_event, AudioFormat, ConnectError, ProviderSession,
    ProviderSink, ProviderStream, RecvError, SendError, SttEvent, SttProvider, SttSessionOpts,
};
use super::ws::{self, WsConn, WsReader, WsSink};
use crate::keys::FailKind;

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

/// How a handshake `task-failed` starts its [`ConnectError`], followed by
/// `error_code error_message`. `classify_connect_error` keys on it.
const TASK_FAILED: &str = "task-failed: ";

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

    /// Measured 2026-09-18 against the live API (`live_dashscope`): first
    /// partial at 0.73 s, then one every ~0.5 s through the utterance, the
    /// fastest cadence of any provider here and far inside the watchdog's 5 s
    /// window. Unmeasurable until then: every key the app had was in arrears.
    fn supports_stall_recovery(&self) -> bool {
        true
    }

    /// A handshake `task-failed` is classified by its error code, exactly as
    /// the same event is mid-stream (see [`classify_error_code`]). Every
    /// other connect failure (the upgrade's HTTP status, a timeout) keeps the
    /// substring default.
    fn classify_connect_error(&self, err: &ConnectError) -> FailKind {
        match err.0.strip_prefix(TASK_FAILED) {
            Some(detail) => classify_error_code(detail.split(' ').next().unwrap_or("")),
            None => classify_by_substring(&err.0),
        }
    }

    async fn connect(
        &self,
        key: &str,
        opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        let model = opts.model.as_deref().unwrap_or(MODEL_ID);
        let task_id = gen_task_id();

        let mut conn =
            ws::connect(self.ws_url(), "Authorization", &format!("bearer {key}")).await?;

        // 1) send run-task
        let run_task = build_run_task(&task_id, model, opts);
        conn.send(Message::Text(run_task.into()))
            .await
            .map_err(|e| ConnectError(format!("run-task send: {e}")))?;

        // 2) await task-started (or task-failed) before letting audio flow,
        //    bounded so a silent-but-open connection can't hang the session.
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, await_task_started(&mut conn)).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(ConnectError(format!(
                    "no task-started within {HANDSHAKE_TIMEOUT:?}"
                )))
            }
        }

        let (sink, stream) = conn.split();
        Ok(ProviderSession {
            sink: Box::new(DashScopeSink { sink, task_id }),
            stream: Box::new(DashScopeStream {
                ws: WsReader::new(stream),
            }),
        })
    }
}

/// Read until the server answers `run-task`: `Ok` on `task-started`, the
/// failure on `task-failed`, skipping anything else it sends first.
async fn await_task_started(conn: &mut WsConn) -> Result<(), ConnectError> {
    loop {
        match conn.next().await {
            Some(Ok(Message::Text(t))) => {
                let Some(header) = parse_header(&t) else {
                    continue;
                };
                match header.event.as_deref() {
                    Some("task-started") => return Ok(()),
                    Some("task-failed") => return Err(task_failed_error(&header)),
                    _ => continue,
                }
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(ConnectError(format!("handshake recv: {e}"))),
            None => return Err(ConnectError("closed before task-started".into())),
        }
    }
}

/// The [`ConnectError`] for a handshake `task-failed`: its code and message
/// only, never the whole frame. The frame echoes our random 32-hex `task_id`,
/// and a "401", "403" or "429" inside that id used to bench a good key as
/// Invalid or RateLimit over a transient server error.
fn task_failed_error(header: &DsHeader) -> ConnectError {
    ConnectError(format!(
        "{TASK_FAILED}{} {}",
        header.error_code.as_deref().unwrap_or_default(),
        header.error_message.as_deref().unwrap_or_default()
    ))
}

/// What a `task-failed` error code says about the key, read from the code
/// alone, never the prose. `InvalidParameter` (a model or language hint the
/// server rejects) contains "invalid" and `Throttling.RateQuota` contains
/// "quota", so the old substring read benched good keys as Dead or out of
/// credit over a setting or a throttle. Anything not named here is the
/// request's or the server's problem ([`FailKind::Transient`]).
fn classify_error_code(code: &str) -> FailKind {
    match code {
        "InvalidApiKey" => FailKind::Invalid,
        "Arrearage" => FailKind::Exhausted,
        c if c.starts_with("Throttling") => FailKind::RateLimit,
        // A spent allowance that is not a per-minute throttle, such as
        // `AllocationQuota.FreeTierOnly`.
        c if c.contains("Quota") => FailKind::Exhausted,
        _ => FailKind::Transient,
    }
}

/// Build the `run-task` payload for `model`/`opts`/`task_id`. Pure
/// (fixture-tested); `connect` just sends this. `language_hints` is
/// Paraformer realtime's array-of-language-codes parameter -- `opts.language`
/// was previously computed by the runner but never placed anywhere in this
/// payload, so a user's language choice was silently ignored.
///
/// The hint is only sent for a NON-default language. QuickDictate shipped for
/// months never sending it, which left Paraformer in its own auto-detect
/// mode, and DashScope's user base skews heavily toward speakers relying on
/// exactly that. The app-wide default language is "en-US" whether or not the
/// user ever looked at the setting, so forcing `["en"]` on every default
/// config would have flipped those users from working auto-detect to
/// forced-English. An explicit non-default choice is the only signal the user
/// actually wants a language pinned, so that is when the hint goes on the
/// wire.
///
/// `opts.custom_vocabulary` is intentionally **not** wired in here: DashScope's
/// only inline biasing knob, `vocabulary_id`, requires pre-registering a named
/// vocabulary through a separate Create Vocabulary List API call ahead of time
/// -- there is no way to bias recognition with a plain, on-the-fly term list
/// as the other providers allow.
fn build_run_task(task_id: &str, model: &str, opts: &SttSessionOpts) -> String {
    let mut parameters = json!({
        "format": "pcm",
        "sample_rate": opts.sample_rate,
    });
    // "en" is what the default "en-US" reduces to via language_for; anything
    // else is an explicit user choice.
    if !opts.language.is_empty() && opts.language != "en" {
        parameters["language_hints"] = json!([opts.language.clone()]);
    }
    json!({
        "header": { "action": "run-task", "task_id": task_id, "streaming": "duplex" },
        "payload": {
            "task_group": "audio",
            "task": "asr",
            "function": "recognition",
            "model": model,
            "parameters": parameters,
            "input": {}
        }
    })
    .to_string()
}

struct DashScopeSink {
    sink: WsSink,
    task_id: String,
}

#[async_trait]
impl ProviderSink for DashScopeSink {
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        ws::send_pcm(&mut self.sink, pcm).await
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        let finish = json!({
            "header": { "action": "finish-task", "task_id": self.task_id, "streaming": "duplex" },
            "payload": { "input": {} }
        })
        .to_string();
        ws::send_text(&mut self.sink, finish).await
    }

    async fn keepalive(&mut self) -> Result<(), SendError> {
        // No documented no-audio keepalive, so use a transport-level WS ping.
        ws::ping(&mut self.sink).await
    }

    async fn close(&mut self) -> Result<(), SendError> {
        // No-op: finish-task drives the server-side close after task-finished.
        Ok(())
    }
}

struct DashScopeStream {
    ws: WsReader,
}

#[async_trait]
impl ProviderStream for DashScopeStream {
    async fn recv_event(&mut self) -> Result<Option<SttEvent>, RecvError> {
        self.ws.recv(map_frame).await
    }
}

/// The `header` of a DashScope frame (used during the handshake).
fn parse_header(text: &str) -> Option<DsHeader> {
    serde_json::from_str::<DsMessage>(text).ok()?.header
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
        // The same code-only read as a handshake failure, so the two paths
        // cannot drift; only a verdict on the key rotates it.
        "task-failed" => Some(server_error_event(
            "dashscope",
            classify_error_code(header.error_code.as_deref().unwrap_or_default()),
            text,
        )),
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

    fn task_failed(code: &str) -> String {
        format!(
            r#"{{"header":{{"task_id":"401aa403bb429cc00123456789abcdef","event":"task-failed","error_code":"{code}","error_message":"something went wrong"}}}}"#
        )
    }

    #[test]
    fn task_failed_is_key_failure() {
        assert!(matches!(
            map_frame(
                r#"{"header":{"event":"task-failed","error_code":"InvalidApiKey","error_message":"unauthorized"}}"#
            ),
            Some(SttEvent::KeyFailure(FailKind::Invalid))
        ));
        assert!(matches!(
            map_frame(&task_failed("Arrearage")),
            Some(SttEvent::KeyFailure(FailKind::Exhausted))
        ));
        assert!(matches!(
            map_frame(&task_failed("Throttling.RateQuota")),
            Some(SttEvent::KeyFailure(FailKind::RateLimit))
        ));
    }

    #[test]
    fn a_task_failure_that_is_not_the_keys_fault_keeps_the_press_alive() {
        // InvalidParameter used to read as Invalid off the word "invalid",
        // and anything unrecognised as a key failure that ended the press.
        for code in ["InternalError", "InvalidParameter"] {
            assert!(
                matches!(
                    map_frame(&task_failed(code)),
                    Some(SttEvent::ProviderFailure(m)) if m.starts_with("dashscope sent")
                ),
                "{code}"
            );
        }
    }

    #[test]
    fn a_handshake_failure_is_classified_by_its_code_not_the_echoed_task_id() {
        // The task_id holds "401", "403" and "429"; before, the whole frame
        // went to the substring classifier and a transient InternalError
        // benched the key as Invalid.
        let provider = DashScopeProvider { intl: false };
        let classify = |code: &str| {
            let header = parse_header(&task_failed(code)).unwrap();
            provider.classify_connect_error(&task_failed_error(&header))
        };
        assert_eq!(classify("InternalError"), FailKind::Transient);
        assert_eq!(classify("InvalidParameter"), FailKind::Transient);
        assert_eq!(classify("InvalidApiKey"), FailKind::Invalid);
        assert_eq!(classify("Arrearage"), FailKind::Exhausted);
        // Failures before the handshake still read the upgrade's status.
        assert_eq!(
            provider.classify_connect_error(&ConnectError(
                "ws connect failed: HTTP error: 401 Unauthorized".into()
            )),
            FailKind::Invalid
        );
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

    fn test_opts(language: &str) -> SttSessionOpts {
        SttSessionOpts {
            language: language.into(),
            sample_rate: 16_000,
            model: None,
            custom_vocabulary: Vec::new(),
        }
    }

    #[test]
    fn run_task_carries_language_hints_only_for_a_non_default_language() {
        let payload = build_run_task("t1", MODEL_ID, &test_opts("es"));
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            parsed["payload"]["parameters"]["language_hints"],
            serde_json::json!(["es"])
        );
        // The default language ("en-US" -> "en") must NOT pin a language:
        // omitting the field is what preserves Paraformer's auto-detect for
        // every config that never touched the setting.
        let payload = build_run_task("t1", MODEL_ID, &test_opts("en"));
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert!(parsed["payload"]["parameters"]
            .as_object()
            .unwrap()
            .get("language_hints")
            .is_none());
    }

    #[test]
    fn run_task_still_carries_format_and_sample_rate() {
        let payload = build_run_task("t1", MODEL_ID, &test_opts("en"));
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["payload"]["parameters"]["format"], "pcm");
        assert_eq!(parsed["payload"]["parameters"]["sample_rate"], 16_000);
        assert_eq!(parsed["payload"]["model"], MODEL_ID);
    }
}
