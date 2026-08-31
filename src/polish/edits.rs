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

pub(super) async fn request_edits(
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
