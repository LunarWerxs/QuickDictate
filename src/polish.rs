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

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;
use tokio::runtime::Handle as TokioHandle;

use crate::config::Config;

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
struct Edit {
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

pub struct Polisher {
    rt: TokioHandle,
    client: reqwest::Client,
    state: Arc<Mutex<State>>,
    /// Round-robin cursor over `PolishSettings::keys`. Plain wrapping
    /// increment: a skipped or repeated key costs nothing here, because a
    /// failed pass just pastes the unpolished text.
    next_key: Arc<std::sync::atomic::AtomicUsize>,
}

impl Polisher {
    pub fn new(rt: TokioHandle) -> Self {
        Self {
            rt,
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
            state: Arc::new(Mutex::new(State::default())),
            next_key: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// The next key to send with, or `None` if none are configured.
    fn take_key(
        settings: &PolishSettings,
        cursor: &std::sync::atomic::AtomicUsize,
    ) -> Option<String> {
        if settings.keys.is_empty() {
            return None;
        }
        let n = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        settings.keys.get(n % settings.keys.len()).cloned()
    }

    /// Drop any cached answer. Called when a new dictation starts so a
    /// result computed for the previous press can never be handed to this one.
    pub fn reset(&self) {
        let mut st = self.state.lock();
        st.ready = None;
        st.queued = None;
        // `inflight` is deliberately left alone: the task owns it and clears
        // it on completion. Its result will simply not match any future text.
    }

    /// Run a pass over `text` in the background, if one isn't already running
    /// for it. Fire-and-forget: this is the free-time path taken while the
    /// user is still talking, so it never blocks and never reports anything.
    pub fn speculate(&self, settings: &PolishSettings, text: &str) {
        if !worth_polishing(text) {
            return;
        }
        let start = {
            let mut st = self.state.lock();
            if st.ready.as_ref().is_some_and(|(input, _)| input == text)
                || st.inflight.as_deref() == Some(text)
            {
                return;
            }
            // One pause that added a few words is not worth another round
            // trip: the answer we already have covers all but the tail, and
            // `resolve` will ask for real if it turns out to matter. Without
            // this, a stop-start dictation fires a request per pause, each one
            // re-sending everything the last already covered.
            let answered = st
                .ready
                .as_ref()
                .map(|(input, _)| input.as_str())
                .or(st.inflight.as_deref());
            if let Some(answered) = answered {
                let grew = text
                    .chars()
                    .count()
                    .saturating_sub(answered.chars().count());
                if text.starts_with(answered) && grew < MIN_GROWTH_CHARS {
                    return;
                }
            }
            if st.inflight.is_some() {
                st.queued = Some(text.to_string());
                false
            } else {
                st.inflight = Some(text.to_string());
                true
            }
        };
        if start {
            self.spawn(settings.clone(), text.to_string());
        }
    }

    /// The polished form of `text`, or `None` to paste it as-is.
    ///
    /// Returns instantly when speculation already answered for exactly this
    /// text. Otherwise it starts a pass and blocks the paste thread for at
    /// most `settings.deadline` -- that bounded wait IS the feature, and a
    /// timeout is a normal outcome, not an error.
    pub fn resolve(&self, settings: &PolishSettings, text: &str) -> Option<String> {
        if !worth_polishing(text) {
            return None;
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(1);
        let start = {
            let mut st = self.state.lock();
            if let Some((input, polished)) = st.ready.take() {
                if input == text {
                    tracing::debug!("polish: speculation already answered this, no wait");
                    return changed(text, polished);
                }
            }
            if st.inflight.as_deref() == Some(text) {
                // Already being computed -- wait on that one rather than
                // restarting the clock with a duplicate request.
                st.waiters.push(tx);
                false
            } else {
                st.inflight = Some(text.to_string());
                st.waiters = vec![tx];
                st.queued = None;
                true
            }
        };
        if start {
            self.spawn(settings.clone(), text.to_string());
        }

        let started = Instant::now();
        match rx.recv_timeout(settings.deadline) {
            Ok(polished) => {
                tracing::info!(
                    "polish: answered in {:?}, inside the deadline",
                    started.elapsed()
                );
                changed(text, polished)
            }
            Err(_) => {
                // Timed out, or the task gave up and dropped its sender.
                // Either way the raw text is pasted, exactly as it would have
                // been without any of this.
                tracing::info!(
                    "polish: deadline {:?} expired, pasting unpolished",
                    settings.deadline
                );
                None
            }
        }
    }

    /// Run one pass on the tokio runtime, publish the result, wake anyone
    /// blocked on it, and pick up whatever was queued behind it.
    fn spawn(&self, settings: PolishSettings, text: String) {
        let inner = Polisher {
            rt: self.rt.clone(),
            client: self.client.clone(),
            state: Arc::clone(&self.state),
            next_key: Arc::clone(&self.next_key),
        };
        let key = Self::take_key(&settings, &self.next_key);
        self.rt.spawn(async move {
            let started = Instant::now();
            let outcome = match key {
                Some(key) => request_edits(&inner.client, &settings, &key, &text).await,
                None => Err("no key configured".to_string()),
            };
            let polished = match outcome {
                Ok(edits) => apply_edits(&text, &edits),
                Err(e) => {
                    tracing::debug!("polish: request failed ({e}); leaving the text alone");
                    None
                }
            };
            tracing::debug!(
                "polish: pass over {} char(s) took {:?} and {} the text",
                text.chars().count(),
                started.elapsed(),
                if polished.is_some() {
                    "changed"
                } else {
                    "left"
                }
            );

            // Whatever came back, this input is answered: publish it (even
            // unchanged, stored as the text itself) so a later `resolve` for
            // the same text is instant instead of a second round trip.
            let settled = polished.unwrap_or_else(|| text.clone());
            let (waiters, next) = {
                let mut st = inner.state.lock();
                st.ready = Some((text.clone(), settled.clone()));
                if st.inflight.as_deref() == Some(text.as_str()) {
                    st.inflight = None;
                    (std::mem::take(&mut st.waiters), st.queued.take())
                } else {
                    // A newer pass superseded us and owns the waiters now.
                    // Our answer still goes in `ready`; it just isn't the one
                    // anybody is holding the paste for.
                    (Vec::new(), None)
                }
            };
            for tx in waiters {
                // The receiver may already have hit its deadline and gone;
                // that is the expected loss case, not an error.
                let _ = tx.try_send(settled.clone());
            }
            if let Some(next) = next {
                inner.speculate(&settings, &next);
            }
        });
    }
}

/// `Some(polished)` only when it actually differs, so the paste path can skip
/// the work (and the log line) when the model had nothing to say.
fn changed(original: &str, polished: String) -> Option<String> {
    (polished != original).then_some(polished)
}

/// Long enough to have context, short enough to be worth sending.
fn worth_polishing(text: &str) -> bool {
    let n = text.trim().chars().count();
    (MIN_CHARS..=MAX_CHARS).contains(&n)
}

async fn request_edits(
    client: &reqwest::Client,
    settings: &PolishSettings,
    key: &str,
    text: &str,
) -> Result<Vec<Edit>, String> {
    let mut body = json!({
        "model": settings.model,
        "temperature": 0,
        "max_completion_tokens": MAX_OUTPUT_TOKENS,
        "response_format": { "type": "json_object" },
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": text },
        ],
    });
    // Gemini models think before answering unless told otherwise, and thinking
    // is exactly what a millisecond budget cannot afford: the same model that
    // answers in 0.6 s at "low" takes 3 s at its default. Sent only for Gemini
    // because OpenAI's non-reasoning models reject the field outright.
    //
    // "low" rather than "none" on purpose. "none" is faster still on the
    // models that take it, but gemini-3.6-flash, gemini-3.5-flash-lite and
    // gemini-flash-lite-latest all 400 on it ("Request contains an invalid
    // argument"), and silently failing on the best model available would be a
    // poor trade for a couple hundred milliseconds. Measured 2026-08-13.
    if settings.model.starts_with("gemini") {
        body["reasoning_effort"] = json!("low");
    }
    let resp = client
        .post(&settings.endpoint)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let raw = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        // Truncated: an error body can be a full HTML error page.
        let head: String = raw.chars().take(200).collect();
        return Err(format!("HTTP {} {head}", status.as_u16()));
    }
    parse_reply(&raw)
}

/// Pull the edit list out of an OpenAI-shaped chat completion. Written
/// against the wire format rather than a typed client so any OpenAI-compatible
/// endpoint (Groq, Cerebras, a local server) works by changing one URL.
fn parse_reply(raw: &str) -> Result<Vec<Edit>, String> {
    let v: serde_json::Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("no message content")?;
    let list: EditList = serde_json::from_str(content).map_err(|e| e.to_string())?;
    Ok(list.edits)
}

/// Apply an edit list to `original`, or return `None` to leave it untouched.
///
/// Every edit is resolved against the ORIGINAL text and all of them are
/// spliced in one pass. Applying them sequentially over the growing output
/// would let one edit's replacement be matched and rewritten by the next --
/// the same cascade that once made two text-replacement rules undo each other
/// (see `TextProcessor::build_replacements`).
///
/// Anything suspicious rejects the WHOLE set rather than applying part of it:
/// a half-applied edit list is a sentence nobody wrote.
fn apply_edits(original: &str, edits: &[Edit]) -> Option<String> {
    if edits.is_empty() || edits.len() > MAX_EDITS {
        return None;
    }
    let mut spans: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for edit in edits {
        if edit.before.is_empty() || edit.before == edit.after {
            continue;
        }
        // Exactly once, or we cannot know which occurrence was meant.
        let mut hits = original.match_indices(&edit.before);
        let Some((at, _)) = hits.next() else {
            tracing::debug!("polish: dropping an edit whose `before` is not in the transcript");
            return None;
        };
        if hits.next().is_some() {
            tracing::debug!("polish: dropping an edit whose `before` is ambiguous");
            return None;
        }
        spans.push((at, at + edit.before.len(), edit.after.as_str()));
    }
    if spans.is_empty() {
        return None;
    }

    spans.sort_by_key(|(start, _, _)| *start);
    // Overlapping edits have no well-defined result.
    if spans.windows(2).any(|w| w[0].1 > w[1].0) {
        tracing::debug!("polish: dropping an overlapping edit set");
        return None;
    }

    let changed: usize = spans
        .iter()
        .map(|(start, end, after)| changed_extent(&original[*start..*end], after))
        .sum();
    let budget = original.chars().count() as f64 * MAX_CHANGED_FRACTION;
    if changed as f64 > budget {
        tracing::info!(
            "polish: rejecting an edit set that rewrites {changed} of {} char(s)",
            original.chars().count()
        );
        return None;
    }

    let mut out = String::with_capacity(original.len());
    let mut cursor = 0usize;
    for (start, end, after) in spans {
        out.push_str(&original[cursor..start]);
        out.push_str(after);
        cursor = end;
    }
    out.push_str(&original[cursor..]);

    (out != original && !out.trim().is_empty()).then_some(out)
}

/// How much of an edit is an actual change, ignoring the context the model
/// quoted around it to make `before` unique.
///
/// Trims the shared prefix and suffix and returns the longer of the two
/// remaining cores, so "…want to... significantly…" -> "…want to
/// significantly…" scores 3 (the deleted ellipsis) rather than the 57
/// characters it had to quote to point at it. Character-based, so a multi-byte
/// codepoint can never be split.
fn changed_extent(before: &str, after: &str) -> usize {
    let b: Vec<char> = before.chars().collect();
    let a: Vec<char> = after.chars().collect();
    let mut head = 0;
    while head < b.len() && head < a.len() && b[head] == a[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < b.len() - head
        && tail < a.len() - head
        && b[b.len() - 1 - tail] == a[a.len() - 1 - tail]
    {
        tail += 1;
    }
    (b.len() - head - tail).max(a.len() - head - tail)
}

/// Probe one cleanup key by asking the configured endpoint and model for a
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

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(before: &str, after: &str) -> Edit {
        Edit {
            before: before.to_string(),
            after: after.to_string(),
        }
    }

    #[test]
    fn applies_a_clean_edit_set_in_one_pass() {
        let original = "so I don't want to... Significantly slow down the process";
        let out = apply_edits(original, &[edit("to... Significantly", "to significantly")])
            .expect("edit applies");
        assert_eq!(
            out,
            "so I don't want to significantly slow down the process"
        );
    }

    #[test]
    fn edits_do_not_cascade_into_each_other() {
        // Resolved against the ORIGINAL: "cat"->"dog" must not then be seen
        // and rewritten by "dog"->"cat", which is what a sequential apply
        // would do.
        // Long enough that MAX_CHANGED_FRACTION is not the binding constraint
        // -- this test is about ordering, not about the rewrite budget.
        let out = apply_edits(
            "a cat wandered past the window and a dog barked at it from the yard",
            &[edit("cat", "dog"), edit("dog", "cat")],
        )
        .expect("both apply");
        assert_eq!(
            out,
            "a dog wandered past the window and a cat barked at it from the yard"
        );
    }

    #[test]
    fn a_before_that_is_not_verbatim_rejects_the_whole_set() {
        // The single most important guard: a model that paraphrases the text
        // it claims to be quoting gets dropped, not pasted.
        assert!(apply_edits("the quick brown fox", &[edit("the quick red fox", "x")]).is_none());
        // ...and it takes its otherwise-valid siblings with it, because a
        // partially applied list is a sentence nobody wrote.
        assert!(apply_edits(
            "the quick brown fox",
            &[edit("brown", "red"), edit("not present", "x")]
        )
        .is_none());
    }

    #[test]
    fn an_ambiguous_before_is_rejected() {
        assert!(apply_edits("go go go", &[edit("go", "stop")]).is_none());
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        assert!(apply_edits(
            "alpha beta gamma",
            &[edit("alpha beta", "x"), edit("beta gamma", "y")]
        )
        .is_none());
    }

    #[test]
    fn generously_quoted_edits_are_judged_on_what_they_actually_change() {
        // The verbatim gpt-4.1-mini reply to the session-717 transcript. Its
        // three edits quote 34% of the text in order to point unambiguously
        // at three deleted ellipses -- correct work that a span-length budget
        // would have rejected as a rewrite.
        let original = "One of the things I noticed, um, I default to using the ElevenLabs model \
right now, is often if I pause for too long, the AI will put a bunch of space between my \
sentences, even though it's quite clear that... the pause didn't intend to have a- ... uh, \
punctuation and spacing. Is there a way to have the AI more contextually aware? Like, just \
before pasting, run a final pass or something? Or just suggestions, 'cause I know we're all \
about speed and that's really important, so I don't want to... significantly slow down the \
process. But, for example, if I paste, it will...";
        let out = apply_edits(
            original,
            &[
                edit(
                    "it's quite clear that... the pause didn't intend to have a- ... uh, punctuation and spacing.",
                    "it's quite clear that the pause didn't intend to have a- uh, punctuation and spacing.",
                ),
                edit(
                    "so I don't want to... significantly slow down the process.",
                    "so I don't want to significantly slow down the process.",
                ),
                edit("if I paste, it will...", "if I paste, it will"),
            ],
        )
        .expect("a real reply must survive the guardrails");
        assert!(out.contains("quite clear that the pause didn't intend"));
        assert!(out.contains("don't want to significantly slow down"));
        assert!(!out.contains("want to... significantly"));
    }

    #[test]
    fn changed_extent_ignores_quoted_context() {
        assert_eq!(changed_extent("want to... slow", "want to slow"), 3);
        assert_eq!(changed_extent("same", "same"), 0);
        // No shared prefix or suffix: the whole thing is the change.
        assert_eq!(changed_extent("abc", "xyz"), 3);
        // Multi-byte codepoints are never split mid-character.
        assert_eq!(changed_extent("a\u{2026}b", "ab"), 1);
    }

    #[test]
    fn a_wholesale_rewrite_is_rejected() {
        let original = "I genuinely don't understand why you keep stopping mid-task like this";
        // One edit that swallows most of the sentence: fluent, plausible, and
        // exactly what must never reach the clipboard.
        let out = apply_edits(
            original,
            &[edit(
                "genuinely don't understand why you keep stopping mid-task like this",
                "would like to understand your reasoning",
            )],
        );
        assert!(out.is_none());
    }

    #[test]
    fn no_edits_and_no_op_edits_leave_the_text_alone() {
        assert!(apply_edits("nothing to do here", &[]).is_none());
        assert!(apply_edits("nothing to do here", &[edit("here", "here")]).is_none());
        assert!(apply_edits("nothing to do here", &[edit("", "x")]).is_none());
    }

    #[test]
    fn too_many_edits_is_a_rewrite_not_a_repair() {
        let many: Vec<Edit> = (0..MAX_EDITS + 1).map(|_| edit("a", "b")).collect();
        assert!(apply_edits("a".repeat(200).as_str(), &many).is_none());
    }

    #[test]
    fn only_useful_lengths_are_sent() {
        assert!(!worth_polishing("too short"));
        assert!(worth_polishing(
            "this one is comfortably long enough to carry context"
        ));
        assert!(!worth_polishing(&"x".repeat(MAX_CHARS + 1)));
    }

    #[test]
    fn parses_an_openai_shaped_reply() {
        let raw = r#"{"choices":[{"message":{"content":"{\"edits\":[{\"before\":\"a b\",\"after\":\"a, b\"}]}"}}]}"#;
        let edits = parse_reply(raw).expect("parses");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].after, "a, b");
        // An empty list is a valid "nothing to fix", not a parse failure.
        let none = r#"{"choices":[{"message":{"content":"{\"edits\":[]}"}}]}"#;
        assert!(parse_reply(none).expect("parses").is_empty());
    }

    /// An endpoint nothing can be listening on, so the request fails fast and
    /// deterministically without touching the network.
    fn dead_endpoint(deadline_ms: u64) -> PolishSettings {
        PolishSettings {
            endpoint: "http://127.0.0.1:1/v1/chat/completions".into(),
            model: "unused".into(),
            keys: vec!["unused".into()],
            deadline: Duration::from_millis(deadline_ms),
        }
    }

    const LONG_ENOUGH: &str = "this transcript is comfortably past the minimum length";

    #[tokio::test]
    async fn a_dead_endpoint_costs_the_deadline_and_nothing_more() {
        let p = Polisher::new(tokio::runtime::Handle::current());
        let settings = dead_endpoint(200);
        let started = Instant::now();
        assert!(p.resolve(&settings, LONG_ENOUGH).is_none());
        // The bound that matters: a broken cleanup pass can never cost more
        // than the budget the user set for it.
        assert!(
            started.elapsed() < Duration::from_millis(1_500),
            "resolve blocked for {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_short_pause_does_not_trigger_another_round_trip() {
        let p = Polisher::new(tokio::runtime::Handle::current());
        let settings = dead_endpoint(50);
        let base = "the first sentence of this dictation, long enough to be worth sending";
        p.state.lock().ready = Some((base.to_string(), base.to_string()));

        // A few more words after a pause: still covered, don't re-ask.
        p.speculate(&settings, &format!("{base} and a few more words"));
        assert_eq!(
            p.state.lock().inflight,
            None,
            "a small addition must not start a request"
        );

        // Well past the floor: now it is worth re-asking.
        let grown = format!(
            "{base} and then a genuinely substantial amount of further speech, comfortably \
             more than the growth floor, which really can change what the right edits are \
             because it introduces whole new sentences the earlier answer never saw"
        );
        assert!(
            grown.chars().count() - base.chars().count() > MIN_GROWTH_CHARS,
            "fixture must clear the floor it is testing"
        );
        p.speculate(&settings, &grown);
        assert_eq!(p.state.lock().inflight.as_deref(), Some(grown.as_str()));
    }

    #[tokio::test]
    async fn rewritten_text_always_re_asks_however_short() {
        // The growth floor only applies to text that EXTENDS what was already
        // answered. A retry or a superseded session produces different text,
        // and reusing a prefix answer there would be wrong, not just stale.
        let p = Polisher::new(tokio::runtime::Handle::current());
        let base = "the first sentence of this dictation, long enough to be worth sending";
        p.state.lock().ready = Some((base.to_string(), base.to_string()));
        let different = "a completely different utterance that shares no prefix at all";
        p.speculate(&dead_endpoint(50), different);
        assert_eq!(p.state.lock().inflight.as_deref(), Some(different));
    }

    #[test]
    fn keys_are_rotated_so_several_projects_share_the_load() {
        use std::sync::atomic::AtomicUsize;
        let settings = PolishSettings {
            endpoint: "x".into(),
            model: "x".into(),
            keys: vec!["a".into(), "b".into(), "c".into()],
            deadline: Duration::from_millis(1),
        };
        let cursor = AtomicUsize::new(0);
        let picked: Vec<String> = (0..7)
            .filter_map(|_| Polisher::take_key(&settings, &cursor))
            .collect();
        assert_eq!(picked, ["a", "b", "c", "a", "b", "c", "a"]);
        // The cursor wraps rather than overflowing after a long uptime.
        let wrapped = AtomicUsize::new(usize::MAX);
        assert!(Polisher::take_key(&settings, &wrapped).is_some());
        assert!(Polisher::take_key(&settings, &wrapped).is_some());
    }

    #[tokio::test]
    async fn text_too_short_to_polish_never_waits_at_all() {
        let p = Polisher::new(tokio::runtime::Handle::current());
        let started = Instant::now();
        assert!(p.resolve(&dead_endpoint(3_000), "too short").is_none());
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn an_unchanged_answer_is_reported_as_no_change() {
        // A completed pass that changed nothing is cached as the text itself;
        // `resolve` must report that as "nothing to do" rather than handing
        // back an identical string for the paste path to re-process.
        let p = Polisher::new(tokio::runtime::Handle::current());
        p.state.lock().ready = Some((LONG_ENOUGH.to_string(), LONG_ENOUGH.to_string()));
        assert!(p.resolve(&dead_endpoint(50), LONG_ENOUGH).is_none());
    }

    #[tokio::test]
    async fn a_cached_answer_for_different_text_is_never_applied() {
        let p = Polisher::new(tokio::runtime::Handle::current());
        p.state.lock().ready = Some(("some other dictation entirely".into(), "WRONG".into()));
        // Must fall through to a live pass (which fails) rather than pasting
        // the answer computed for a different utterance.
        assert!(p.resolve(&dead_endpoint(200), LONG_ENOUGH).is_none());
    }

    #[tokio::test]
    async fn a_speculated_answer_is_returned_without_waiting() {
        let p = Polisher::new(tokio::runtime::Handle::current());
        let polished = format!("{LONG_ENOUGH}, tidied");
        p.state.lock().ready = Some((LONG_ENOUGH.to_string(), polished.clone()));
        let started = Instant::now();
        assert_eq!(
            p.resolve(&dead_endpoint(3_000), LONG_ENOUGH),
            Some(polished)
        );
        // The whole point of speculating: the hit costs nothing.
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn reset_drops_the_previous_dictations_answer() {
        let p = Polisher::new(tokio::runtime::Handle::current());
        p.state.lock().ready = Some((LONG_ENOUGH.to_string(), "from an older press".into()));
        p.reset();
        assert!(p.resolve(&dead_endpoint(200), LONG_ENOUGH).is_none());
    }

    #[test]
    fn a_junk_reply_is_an_error_not_a_panic() {
        assert!(parse_reply("not json at all").is_err());
        assert!(parse_reply(r#"{"choices":[]}"#).is_err());
        assert!(parse_reply(r#"{"choices":[{"message":{"content":"sorry!"}}]}"#).is_err());
    }
}
