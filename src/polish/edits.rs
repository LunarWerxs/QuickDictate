//! Asking the model for an edit list and deciding whether to trust it.
//!
//! Every edit must quote a verbatim, unique substring of the input, and the
//! total change is capped, so a hallucinating model is dropped rather than
//! pasted.

use serde_json::json;

use super::*;

/// `Some(polished)` only when it actually differs, so the paste path can skip
/// the work (and the log line) when the model had nothing to say.
pub(super) fn changed(original: &str, polished: String) -> Option<String> {
    (polished != original).then_some(polished)
}

/// Long enough to have context, short enough to be worth sending.
pub(super) fn worth_polishing(text: &str) -> bool {
    let n = text.trim().chars().count();
    (MIN_CHARS..=MAX_CHARS).contains(&n)
}

/// OpenAI's reasoning families (GPT-5, GPT-6, the o-series). They think
/// before answering unless told not to, and many reject a non-default
/// `temperature` (measured 2026-10-02: gpt-5, gpt-5.5, gpt-5.6-*, gpt-6-*
/// and o4-mini all 400 on `temperature: 0`), so they get neither the
/// temperature nor the thinking.
pub(super) fn is_openai_reasoning_model(model: &str) -> bool {
    ["gpt-5", "gpt-6", "o1", "o3", "o4"]
        .iter()
        .any(|family| model.starts_with(family))
}

/// Reasoning models that refused `reasoning_effort: "none"` in this process;
/// they are asked for `"low"` from then on. Measured 2026-10-02: gpt-5.1 and
/// later plus gpt-6-luna/-sol take "none" (the fastest), while the original
/// gpt-5 family, gpt-6-astra and o4-mini take only "low" -- which every
/// reasoning model accepts. Learning it per model keeps a future model
/// working without a release.
fn low_effort_only() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static MODELS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    MODELS.get_or_init(Default::default)
}

/// The `reasoning_effort` to send, if any.
///
/// Gemini models think before answering unless told otherwise, and thinking
/// is exactly what a millisecond budget cannot afford: the same model that
/// answers in 0.6 s at "low" takes 3 s at its default. "low" rather than
/// "none" on purpose for Gemini: gemini-3.6-flash, gemini-3.5-flash-lite,
/// gemini-flash-latest and gemini-flash-lite-latest all 400 on "none"
/// (measured 2026-08-13). OpenAI's non-reasoning models (gpt-4.1-*, gpt-4o-*)
/// reject the field outright, so they get none.
pub(super) fn reasoning_effort(model: &str) -> Option<&'static str> {
    if model.starts_with("gemini") {
        Some("low")
    } else if is_openai_reasoning_model(model) {
        let low = low_effort_only()
            .lock()
            .map(|m| m.contains(model))
            .unwrap_or(false);
        Some(if low { "low" } else { "none" })
    } else {
        None
    }
}

pub(super) fn request_body(model: &str, text: &str, effort: Option<&str>) -> serde_json::Value {
    let mut body = json!({
        "model": model,
        "max_completion_tokens": MAX_OUTPUT_TOKENS,
        "response_format": { "type": "json_object" },
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": text },
        ],
    });
    if !is_openai_reasoning_model(model) {
        body["temperature"] = json!(0);
    }
    if let Some(effort) = effort {
        body["reasoning_effort"] = json!(effort);
    }
    body
}

pub(super) async fn request_edits(
    client: &reqwest::Client,
    settings: &PolishSettings,
    key: &str,
    text: &str,
) -> Result<Vec<Edit>, String> {
    let effort = reasoning_effort(&settings.model);
    let (status, raw) = post(
        client,
        settings,
        key,
        &request_body(&settings.model, text, effort),
    )
    .await?;
    let (status, raw) =
        if status == 400 && effort == Some("none") && raw.contains("reasoning_effort") {
            if let Ok(mut models) = low_effort_only().lock() {
                models.insert(settings.model.clone());
            }
            tracing::info!(
                "polish: {} refused reasoning_effort \"none\"; using \"low\"",
                settings.model
            );
            post(
                client,
                settings,
                key,
                &request_body(&settings.model, text, Some("low")),
            )
            .await?
        } else {
            (status, raw)
        };
    if !(200..300).contains(&status) {
        // Truncated: an error body can be a full HTML error page.
        let head: String = raw.chars().take(200).collect();
        return Err(format!("HTTP {status} {head}"));
    }
    parse_reply(&raw)
}

async fn post(
    client: &reqwest::Client,
    settings: &PolishSettings,
    key: &str,
    body: &serde_json::Value,
) -> Result<(u16, String), String> {
    let resp = client
        .post(&settings.endpoint)
        .bearer_auth(key)
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let raw = resp.text().await.map_err(|e| e.to_string())?;
    Ok((status, raw))
}

/// Pull the edit list out of an OpenAI-shaped chat completion. Written
/// against the wire format rather than a typed client so any OpenAI-compatible
/// endpoint (Groq, Cerebras, a local server) works by changing one URL.
pub fn parse_reply(raw: &str) -> Result<Vec<Edit>, String> {
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
pub(super) fn apply_edits(original: &str, edits: &[Edit]) -> Option<String> {
    if edits.is_empty() || edits.len() > MAX_EDITS {
        return None;
    }
    let mut spans: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for edit in edits {
        if edit.before.is_empty() || edit.before == edit.after {
            continue;
        }
        // Exactly once, or we cannot know which occurrence was meant.
        let Some(at) = original.find(edit.before.as_str()) else {
            tracing::debug!("polish: dropping an edit whose `before` is not in the transcript");
            return None;
        };
        // Search again from one char past the hit, not from its end:
        // `match_indices` skips overlapping matches, so "no no" in
        // "no no no" used to count as unique.
        let next = at + original[at..].chars().next().map_or(1, char::len_utf8);
        if original[next..].contains(edit.before.as_str()) {
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
pub(super) fn changed_extent(before: &str, after: &str) -> usize {
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
