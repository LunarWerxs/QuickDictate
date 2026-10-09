//! `quickdictate.exe --mcp`: a headless Model Context Protocol server over stdio.
//!
//! Newline-delimited JSON-RPC on stdin and stdout, nothing else on stdout.
//! Progress and errors go to stderr. It never opens a window, tray icon, hotkey
//! or the single-instance mutex, so it runs beside the normal app.

mod cloud;
mod decode;
mod engines;

use std::io::{BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::config::Config;
use crate::local_stt;
use engines::{Engine, Usable, CLOUD_PROVIDERS, LOCAL_MODELS};

/// Newest first. The first entry is what an unknown client version gets back.
const SUPPORTED_PROTOCOLS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Whisper windows its own 30 s steps but is fed one utterance at a time, so a
/// long file is cut into windows here first.
const WHISPER_WINDOW_SAMPLES: usize = 16_000 * 60 * 10;

const INSTRUCTIONS: &str = "Transcribes audio files with the speech engines of the user's QuickDictate install. \
Call list_engines to see which engines work here; transcribe_file takes an absolute path to a wav, mp3, m4a, mp4, aac, flac or ogg file.";

pub fn run() -> i32 {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(&message),
            Err(_) => Some(error_reply(Value::Null, -32700, "parse error: not JSON")),
        };
        if let Some(reply) = reply {
            if writeln!(out, "{reply}").and_then(|()| out.flush()).is_err() {
                break;
            }
        }
    }
    0
}

/// `None` for a notification, which gets no reply.
pub(crate) fn handle(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => Ok(call_tool_reply(&params)),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => error_reply(id, code, &message),
    })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize_result(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|v| SUPPORTED_PROTOCOLS.contains(v))
        .unwrap_or(SUPPORTED_PROTOCOLS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "quickdictate", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "transcribe_file",
            "title": "Transcribe an audio file",
            "description": "Transcribe an audio file on this PC with a local model (free, private) or a cloud provider the user has an API key for. Long recordings are fine. Pass an absolute path.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path to the audio file." },
                    "engine": { "type": "string", "description": "An engine id from list_engines, or 'local'. Omit for the default: local Parakeet if installed, else the user's configured provider." },
                    "language": { "type": "string", "description": "Language code such as 'en' or 'de'. Omit to let the engine detect it." }
                },
                "required": ["path"]
            },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "duration_seconds": { "type": "number" },
                    "engine": { "type": "string" },
                    "source_sample_rate_hz": { "type": "integer" }
                },
                "required": ["text", "duration_seconds", "engine"]
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "list_engines",
            "title": "List usable transcription engines",
            "description": "List the speech engines that work on this PC right now: installed local models and cloud providers with an API key, and which one transcribe_file uses by default.",
            "inputSchema": { "type": "object", "properties": {} },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "engines": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "kind": { "type": "string", "enum": ["local", "cloud"] },
                                "label": { "type": "string" }
                            },
                            "required": ["id", "kind", "label"]
                        }
                    },
                    "default": { "type": ["string", "null"] }
                },
                "required": ["engines", "default"]
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        }
    ])
}

fn call_tool_reply(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let outcome = catch_unwind(AssertUnwindSafe(|| match name {
        "transcribe_file" => transcribe_file(&args),
        "list_engines" => Ok(list_engines()),
        other => Err(format!("unknown tool '{other}'")),
    }))
    .unwrap_or_else(|_| {
        Err("the transcription crashed; check that the file is a normal audio file".into())
    });
    match outcome {
        Ok(structured) => json!({
            "content": [{ "type": "text", "text": text_summary(&structured) }],
            "structuredContent": structured,
            "isError": false
        }),
        Err(message) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true
        }),
    }
}

fn text_summary(structured: &Value) -> String {
    match structured.get("text").and_then(Value::as_str) {
        Some(text) => text.to_string(),
        None => serde_json::to_string(structured).unwrap_or_default(),
    }
}

fn load_config() -> Config {
    Config::load_or_create().0
}

fn usable_engines(config: &Config) -> Usable {
    Usable {
        local: LOCAL_MODELS
            .iter()
            .filter(|id| local_stt::is_installed(id))
            .map(|id| id.to_string())
            .collect(),
        cloud: CLOUD_PROVIDERS
            .iter()
            .filter(|p| !config.keys_for(p).is_empty())
            .map(|p| p.to_string())
            .collect(),
    }
}

fn engine_label(engine: &Engine) -> String {
    match engine {
        Engine::Local(id) => local_stt::model(id)
            .map(|m| m.label.to_string())
            .unwrap_or_else(|| id.clone()),
        Engine::Cloud(provider) => match provider.as_str() {
            "openai" => "OpenAI (gpt-4o-transcribe)".into(),
            "deepgram" => "Deepgram (nova-3)".into(),
            "elevenlabs" => "ElevenLabs (scribe_v1)".into(),
            other => other.to_string(),
        },
    }
}

fn list_engines() -> Value {
    let config = load_config();
    let usable = usable_engines(&config);
    let default = engines::choose(None, &usable, config.resolve_provider().as_deref()).ok();
    let mut listed = Vec::new();
    for id in &usable.local {
        listed.push(
            json!({ "id": id, "kind": "local", "label": engine_label(&Engine::Local(id.clone())) }),
        );
    }
    for provider in &usable.cloud {
        listed.push(json!({ "id": provider, "kind": "cloud", "label": engine_label(&Engine::Cloud(provider.clone())) }));
    }
    json!({
        "engines": listed,
        "default": default.map(|e| e.id().to_string()),
    })
}

fn transcribe_file(args: &Value) -> Result<Value, String> {
    let raw_path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or("path is required: an absolute path to an audio file")?;
    let path = Path::new(raw_path);
    if !path.is_absolute() {
        return Err(format!("path must be absolute, got '{raw_path}'"));
    }
    if !path.is_file() {
        return Err(format!("no file at '{raw_path}'"));
    }
    let language = args
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let config = load_config();
    let usable = usable_engines(&config);
    let engine = engines::choose(
        args.get("engine").and_then(Value::as_str),
        &usable,
        config.resolve_provider().as_deref(),
    )?;

    eprintln!("quickdictate mcp: decoding {raw_path}");
    let decoded = decode::decode_file(path)?;
    let seconds = decoded.duration_seconds();
    eprintln!(
        "quickdictate mcp: {seconds:.1} s of audio (source {} Hz), transcribing with {}",
        decoded.source_rate,
        engine.id()
    );

    let text = match &engine {
        Engine::Local(model) => transcribe_local(model, &language, &decoded.pcm)?,
        Engine::Cloud(provider) => {
            let key =
                config.keys_for(provider).first().cloned().ok_or_else(|| {
                    format!("'{provider}' has no API key in QuickDictate settings")
                })?;
            cloud::transcribe(provider, &key, &decoded.pcm, &language)?
        }
    };

    Ok(json!({
        "text": text,
        "duration_seconds": (seconds * 100.0).round() / 100.0,
        "engine": engine.id(),
        "source_sample_rate_hz": decoded.source_rate,
    }))
}

fn transcribe_local(model: &str, language: &str, pcm: &[i16]) -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("could not start the local engine's runtime: {e}"))?;
    runtime.block_on(async {
        let windows: Vec<&[i16]> = if model == "whisper-turbo-q5" {
            pcm.chunks(WHISPER_WINDOW_SAMPLES).collect()
        } else {
            vec![pcm]
        };
        let mut parts = Vec::with_capacity(windows.len());
        for window in windows {
            let text = local_stt::transcribe(
                model.to_string(),
                language.to_string(),
                String::new(),
                window.to_vec(),
                Arc::new(AtomicBool::new(false)),
            )
            .await?;
            parts.push(text.unwrap_or_default());
        }
        Ok(local_stt::join_transcripts(parts).unwrap_or_default())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(message: Value) -> Value {
        handle(&message).unwrap_or(Value::Null)
    }

    #[test]
    fn notifications_get_no_reply() {
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(&note).is_none());
    }

    #[test]
    fn initialize_echoes_a_supported_client_version() {
        let reply = call(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {} }
        }));
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert!(reply["result"]["capabilities"]["tools"].is_object());
        assert_eq!(reply["result"]["serverInfo"]["name"], "quickdictate");
    }

    #[test]
    fn initialize_answers_with_the_newest_version_for_an_unknown_client_version() {
        let reply = call(json!({
            "jsonrpc": "2.0", "id": 2, "method": "initialize",
            "params": { "protocolVersion": "2999-01-01" }
        }));
        assert_eq!(reply["result"]["protocolVersion"], SUPPORTED_PROTOCOLS[0]);
    }

    #[test]
    fn tools_list_names_both_tools_with_schemas() {
        let reply = call(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }));
        let tools = reply["result"]["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["transcribe_file", "list_engines"]);
        assert_eq!(tools[0]["inputSchema"]["required"][0], "path");
    }

    #[test]
    fn ping_returns_an_empty_result() {
        let reply = call(json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" }));
        assert_eq!(reply["result"], json!({}));
    }

    #[test]
    fn unknown_method_is_a_jsonrpc_error_not_a_crash() {
        let reply = call(json!({ "jsonrpc": "2.0", "id": 5, "method": "nope" }));
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[test]
    fn unknown_tool_is_a_tool_error_with_iserror() {
        let reply = call(json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": { "name": "bogus", "arguments": {} }
        }));
        assert_eq!(reply["result"]["isError"], true);
        assert!(reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown tool"));
    }

    #[test]
    fn relative_path_is_refused_with_a_plain_message() {
        let err = transcribe_file(&json!({ "path": "clip.wav" }))
            .err()
            .unwrap_or_default();
        assert!(err.contains("must be absolute"), "{err}");
    }

    #[test]
    fn missing_file_is_refused_with_a_plain_message() {
        let err = transcribe_file(&json!({ "path": r"C:\definitely\not\here.wav" }))
            .err()
            .unwrap_or_default();
        assert!(err.contains("no file at"), "{err}");
    }
}
