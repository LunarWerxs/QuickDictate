//! Optional LLM cleanup pass over a transcript, on a strict latency budget.
//!
//! Streaming STT commits a segment every time the speaker pauses, and it has
//! no way to know whether a pause ended a thought or just a breath. The
//! deterministic rules in [`crate::text`] fix the mechanical damage that
//! causes (see `ends_mid_sentence`), but only a language model can tell that
//! "the peer evaluator" should have been "the pier evaluator", or that a
//! period landed in the middle of a sentence the speaker never finished.
//!
//! The cost of asking one is latency, and latency is the thing this app is
//! for. Two mechanisms keep it off the critical path:
//!
//! 1. **Speculation.** While the hotkey is still down, committed chunks pile
//!    up in the session's held buffer and nothing is pasted yet. That idle
//!    time is free, so every held commit re-runs the pass over the transcript
//!    so far. By the time the user releases, the answer is usually already
//!    sitting in [`Polisher::ready`] and costs zero.
//! 2. **A deadline race.** When speculation missed (a short dictation, a
//!    post-release commit, a slow network) the paste path fires a pass and
//!    waits at most `polish_deadline_ms` for it. Whichever finishes first
//!    wins, and losing costs exactly the deadline, never the round trip.
//!
//! Failure is never fatal and never visible: no key, a dead network, a
//! malformed reply, or a model that tried to rewrite the user all fall back
//! to the unpolished text, which is what would have been pasted anyway.
//!
//! The model returns an **edit list** rather than a rewritten transcript.
//! That is ~10x fewer output tokens (latency here scales with output length,
//! not with how hard the task is), and it is the safer shape: an edit whose
//! `before` is not a verbatim, unique substring of the input is rejected
//! outright, so a hallucinating model gets dropped rather than pasted.

mod edits;
mod polisher;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::config::Config;

pub use polisher::Polisher;

use edits::*;

// Reached only by the mutation-fuzz suite, which is itself `#[cfg(test)]`.
#[cfg(test)]
pub(crate) use edits::parse_reply;

/// Hard cap on how many edits one reply may contain. A genuine cleanup of a
/// paragraph is a handful; a model returning dozens is rewriting, not fixing.
const MAX_EDITS: usize = 24;

/// Reject the whole edit set if it actually changes more than this fraction
/// of the transcript. The pass is allowed to repair the user, not to speak
/// for them, and the single worst failure mode here is pasting fluent text
/// they never said.
///
/// Measured with [`changed_extent`], NOT by the length of the quoted spans.
/// The prompt tells the model to include surrounding words so that `before`
/// is unique, so spans are routinely far larger than the edit inside them: a
/// real reply that deleted three pause-ellipses from a 520-character
/// transcript quoted 34% of it to do so, which a span-length rule would have
/// thrown away for being a "rewrite". Measuring the actual difference puts
/// that same reply at 9%.
const MAX_CHANGED_FRACTION: f64 = 0.25;

/// Don't bother asking about text this short. A few words carry no context
/// for the model to reason about, and they are exactly the pastes where the
/// deadline would be most noticeable relative to the work.
const MIN_CHARS: usize = 24;

/// Never send more than this to the model. Long dictations are rare, and the
/// tail of one is the part the speculation pass has already seen anyway.
const MAX_CHARS: usize = 8_000;

/// How much new speech a pause must have added before speculation re-asks.
///
/// Every speculative pass except the final one is thrown away by construction
/// (the cache is keyed on exact text, so an earlier prefix only ever matches
/// if no further speech arrives). Without a floor, a stop-start dictation
/// therefore fires one full-transcript request per pause and discards all but
/// the last. Roughly a sentence: below that the answer already in hand covers
/// everything but the tail, and `resolve` still asks for real at release if it
/// turns out to matter.
const MIN_GROWTH_CHARS: usize = 120;

/// Ceiling on output tokens. The reply is an edit list, so this is generous;
/// it exists so a model that starts babbling hits a wall instead of holding
/// the socket open until the request timeout.
const MAX_OUTPUT_TOKENS: u32 = 700;

/// Whole-request timeout, well above any deadline the user can configure.
/// The deadline decides what the *paste* waits for; this one just stops a
/// hung socket from leaking a task for the life of the process.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

const SYSTEM_PROMPT: &str = "\
You repair dictated speech-to-text transcripts. The speaker paused while \
talking and the recognizer guessed at sentence boundaries.

Reply with JSON only: {\"edits\":[{\"before\":\"...\",\"after\":\"...\"}]}

Fix ONLY these:
- a sentence boundary invented by a pause: a stray period or ellipsis in the \
middle of a thought, or a word capitalized because of one
- a word clearly misheard in context (homophones, mangled proper nouns)
- a doubled word, or an abandoned false start the speaker talked over

Never reword, restyle, shorten, summarize, translate, add content, remove \
meaning, or change the speaker's terminology, register, or profanity.

Each \"before\" MUST be copied character for character from the input and MUST \
appear in it exactly once; include a few surrounding words if that is what \
makes it unique. If nothing needs fixing, reply {\"edits\":[]}.";

/// The settings this pass needs, resolved for one paste (globals folded with
/// whatever per-app profile matched). Built by [`settings_for`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolishSettings {
    pub endpoint: String,
    pub model: String,
    /// Every configured key, round-robined per request by [`Polisher`]. Keys
    /// for these endpoints are commonly free-tier and rate-limited per
    /// *project*, so several keys from different projects multiply the
    /// requests-per-minute this can use. Never empty (see [`settings_for`]).
    pub keys: Vec<String>,
    pub deadline: Duration,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Edit {
    before: String,
    after: String,
}

#[derive(Debug, Deserialize)]
struct EditList {
    #[serde(default)]
    edits: Vec<Edit>,
}

#[derive(Default)]
struct State {
    /// `(input, polished)` from the most recent completed pass. Keyed by the
    /// exact input so a stale answer can never be applied to different text.
    ready: Option<(String, String)>,
    /// Input of the pass currently in flight, if any. One at a time: a
    /// dictation commits every few seconds and we would rather have one
    /// answer about the whole prefix than five about stale ones.
    inflight: Option<String>,
    /// Paste threads blocked on `inflight`. This is the case that makes the
    /// whole design pay off: the release flush joins exactly the chunks the
    /// last speculation ran on, so at release the answer for that text is
    /// usually already *being computed*. Attaching to it inherits however
    /// much of the round trip has already elapsed, where firing a duplicate
    /// would start the clock over.
    waiters: Vec<std::sync::mpsc::SyncSender<String>>,
    /// Latest text offered while a pass was in flight. Latest wins; the
    /// intermediate ones are already obsolete by the time we could ask.
    queued: Option<String>,
}

/// Probe one cleanup key by asking the configured endpoint for a
/// single token. Not a dry run: a key can be valid and still be rejected by
/// *this* endpoint (a Google key with only Speech-to-Text enabled is exactly
/// that case), so the probe has to be the real request to mean anything.
///
/// Reported as pass/fail rather than a reason, matching the provider key test
/// it shares a button with.
pub fn spawn_key_test(
    app: &crate::state::App,
    settings: PolishSettings,
    keys: Vec<String>,
    on_result: Arc<dyn Fn(String, bool) + Send + Sync>,
) {
    let settings = Arc::new(settings);
    for key in keys {
        let settings = Arc::clone(&settings);
        let on_result = Arc::clone(&on_result);
        app.rt.spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(12))
                .build()
                .unwrap_or_default();
            let ok = probe_key(&client, &settings, &key).await;
            on_result(key, ok);
        });
    }
}

async fn probe_key(client: &reqwest::Client, settings: &PolishSettings, key: &str) -> bool {
    let mut body = json!({
        "model": settings.model,
        "max_completion_tokens": 1,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    if settings.model.starts_with("gemini") {
        body["reasoning_effort"] = json!("low");
    }
    match client
        .post(&settings.endpoint)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let head: String = body.chars().take(200).collect();
                tracing::info!("polish key test: HTTP {} {head}", status.as_u16());
            }
            status.is_success()
        }
        Err(e) => {
            tracing::info!("polish key test: request failed ({e})");
            false
        }
    }
}

/// Resolve the polish settings for a paste, or `None` if the pass is off for
/// this app or has nothing to authenticate with.
pub fn settings_for(cfg: &Config, exe_name: Option<&str>) -> Option<PolishSettings> {
    if !cfg.polish_for_exe(exe_name) {
        return None;
    }
    let keys = cfg.polish_key_pool();
    if keys.is_empty() {
        return None;
    }
    Some(PolishSettings {
        endpoint: cfg.polish_endpoint.clone(),
        model: cfg.polish_model.clone(),
        keys,
        deadline: Duration::from_millis(cfg.polish_deadline_ms),
    })
}
