//! Shaping raw decoder output into a usable transcript.
//!
//! Quiet-boundary clipping for Cohere's 35-second long-form window, and the
//! conservative guards that collapse a decoder that has fallen into a loop.

use std::ops::Range;

// Cohere's own long-form processor never sends the model more than 35 seconds
// at once. It searches the final five seconds for a quiet boundary, then starts
// a fresh decode. The native runtime accepts a much larger positional window,
// but a multi-minute greedy decode can fall into a sentence loop long before
// that hard limit (the supplied field log reproduced this at 240.9 seconds).
pub(super) const COHERE_CLIP_MAX_SECONDS: usize = 35;
const COHERE_BOUNDARY_SEARCH_SECONDS: usize = 5;
pub(super) const COHERE_MIN_TAIL_SECONDS: usize = 5;
const COHERE_ENERGY_WINDOW_MS: usize = 100;
const COHERE_ENERGY_STEP_MS: usize = 10;
const PATHOLOGICAL_SENTENCE_RUN: usize = 4;
const PATHOLOGICAL_SENTENCES_TO_KEEP: usize = 2;
const PATHOLOGICAL_CYCLE_RUN: usize = 4;
const PATHOLOGICAL_CYCLES_TO_KEEP: usize = 2;
const PATHOLOGICAL_MIN_REPEATED_TOKENS: usize = 8;
const PATHOLOGICAL_MAX_CYCLE_TOKENS: usize = 24;

pub(super) fn cohere_chunk_ranges(pcm: &[i16], sample_rate: usize) -> Vec<Range<usize>> {
    if pcm.is_empty() || sample_rate == 0 {
        return Vec::new();
    }
    let max_clip = sample_rate.saturating_mul(COHERE_CLIP_MAX_SECONDS);
    if pcm.len() <= max_clip {
        return std::iter::once(0..pcm.len()).collect();
    }

    let search_span = sample_rate.saturating_mul(COHERE_BOUNDARY_SEARCH_SECONDS);
    let min_tail = sample_rate.saturating_mul(COHERE_MIN_TAIL_SECONDS);
    let mut ranges = Vec::new();
    let mut start = 0usize;
    while pcm.len().saturating_sub(start) > max_clip {
        let search_start = start + max_clip.saturating_sub(search_span);
        // Avoid manufacturing a tiny final fragment for recordings only a
        // little longer than 35 seconds.
        let search_end = (start + max_clip).min(pcm.len().saturating_sub(min_tail));
        let cut = quietest_cut(pcm, search_start, search_end, sample_rate)
            .unwrap_or(search_end.max(search_start));
        // Defensive progress guard; normal inputs always advance by >=30 s.
        let cut = cut.clamp(start + 1, pcm.len());
        ranges.push(start..cut);
        start = cut;
    }
    if start < pcm.len() {
        ranges.push(start..pcm.len());
    }
    ranges
}

/// Find the lowest-energy 100 ms window in `[start, end]` and return its
/// midpoint. A 10 ms step is fine-grained enough to land between spoken words
/// without doing meaningful work compared with inference.
pub(super) fn quietest_cut(
    pcm: &[i16],
    start: usize,
    end: usize,
    sample_rate: usize,
) -> Option<usize> {
    if start >= end || end > pcm.len() || sample_rate == 0 {
        return None;
    }
    let window = (sample_rate.saturating_mul(COHERE_ENERGY_WINDOW_MS) / 1_000).max(1);
    let step = (sample_rate.saturating_mul(COHERE_ENERGY_STEP_MS) / 1_000).max(1);
    let half = window / 2;
    let first = start.saturating_add(half).min(end);
    let last = end.saturating_sub(window.saturating_sub(half));
    if first > last {
        return Some(start + (end - start) / 2);
    }

    let mut best: Option<(u64, usize)> = None;
    let mut cut = first;
    while cut <= last {
        let window_start = cut.saturating_sub(half);
        let window_end = (window_start + window).min(pcm.len());
        let energy = pcm[window_start..window_end]
            .iter()
            .map(|sample| i64::from(*sample).unsigned_abs())
            .sum::<u64>();
        if best
            .map(|(best_energy, _)| energy < best_energy)
            .unwrap_or(true)
        {
            best = Some((energy, cut));
        }
        let next = cut.saturating_add(step);
        if next <= cut {
            break;
        }
        cut = next;
    }
    best.map(|(_, cut)| cut)
}

fn sentence_units(text: &str) -> Vec<&str> {
    let mut units = Vec::new();
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        if !matches!(ch, '.' | '!' | '?') {
            continue;
        }
        let end = index + ch.len_utf8();
        let next = text[end..].chars().next();
        if next.is_none_or(char::is_whitespace) {
            let unit = text[start..end].trim();
            if !unit.is_empty() {
                units.push(unit);
            }
            start = end;
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        units.push(tail);
    }
    units
}

fn normalized_sentence(text: &str) -> String {
    text.chars()
        .filter_map(|ch| {
            if ch.is_alphanumeric() || ch == '\'' {
                Some(ch.to_ascii_lowercase())
            } else if ch.is_whitespace() {
                Some(' ')
            } else {
                None
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Conservative last line of defense for decoder degeneration. Only runs of
/// four or more identical full sentences are touched, and two copies remain so
/// deliberate emphasis is preserved.
pub(super) fn collapse_pathological_sentence_runs(text: &str) -> (String, usize) {
    let units = sentence_units(text);
    let mut output = Vec::with_capacity(units.len());
    let mut dropped = 0usize;
    let mut index = 0usize;
    while index < units.len() {
        let normalized = normalized_sentence(units[index]);
        let mut end = index + 1;
        while end < units.len()
            && !normalized.is_empty()
            && normalized_sentence(units[end]) == normalized
        {
            end += 1;
        }
        let run = end - index;
        let keep = if run >= PATHOLOGICAL_SENTENCE_RUN {
            dropped = dropped.saturating_add(run - PATHOLOGICAL_SENTENCES_TO_KEEP);
            PATHOLOGICAL_SENTENCES_TO_KEEP
        } else {
            run
        };
        output.extend_from_slice(&units[index..index + keep]);
        index = end;
    }
    (output.join(" "), dropped)
}

#[derive(Debug)]
struct WordSpan {
    normalized: String,
    end: usize,
}

fn word_spans(text: &str) -> Vec<WordSpan> {
    let mut spans = Vec::new();
    let mut current: Option<String> = None;
    for (index, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            let normalized = current.get_or_insert_with(String::new);
            normalized.extend(ch.to_lowercase());
        } else if let Some(normalized) = current.take() {
            spans.push(WordSpan {
                normalized,
                end: index,
            });
        }
    }
    if let Some(normalized) = current {
        spans.push(WordSpan {
            normalized,
            end: text.len(),
        });
    }
    spans
}

/// Catch punctuation-free or alternating decoder cycles that the full-sentence
/// guard cannot see (for example, "and here, and here, and here..."). Four
/// cycles and at least eight repeated tokens are required; two cycles remain.
fn collapse_pathological_token_cycles(text: &str) -> (String, usize) {
    let tokens = word_spans(text);
    if tokens.len() < PATHOLOGICAL_MIN_REPEATED_TOKENS {
        return (text.to_string(), 0);
    }

    let mut removals = Vec::new();
    let mut index = 0usize;
    let mut dropped_tokens = 0usize;
    while index < tokens.len() {
        let max_cycle =
            PATHOLOGICAL_MAX_CYCLE_TOKENS.min((tokens.len() - index) / PATHOLOGICAL_CYCLE_RUN);
        let mut found = None;
        for cycle_len in 1..=max_cycle {
            let motif = &tokens[index..index + cycle_len];
            let mut cycles = 1usize;
            while index + (cycles + 1) * cycle_len <= tokens.len()
                && tokens[index + cycles * cycle_len..index + (cycles + 1) * cycle_len]
                    .iter()
                    .map(|token| token.normalized.as_str())
                    .eq(motif.iter().map(|token| token.normalized.as_str()))
            {
                cycles += 1;
            }
            if cycles >= PATHOLOGICAL_CYCLE_RUN
                && cycles * cycle_len >= PATHOLOGICAL_MIN_REPEATED_TOKENS
            {
                found = Some((cycle_len, cycles));
                break;
            }
        }

        if let Some((cycle_len, cycles)) = found {
            let keep_end = index + PATHOLOGICAL_CYCLES_TO_KEEP * cycle_len - 1;
            let run_end = index + cycles * cycle_len - 1;
            removals.push((tokens[keep_end].end, tokens[run_end].end));
            dropped_tokens =
                dropped_tokens.saturating_add((cycles - PATHOLOGICAL_CYCLES_TO_KEEP) * cycle_len);
            index += cycles * cycle_len;
        } else {
            index += 1;
        }
    }

    if removals.is_empty() {
        return (text.to_string(), 0);
    }
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (start, end) in removals {
        output.push_str(&text[cursor..start]);
        cursor = end;
    }
    output.push_str(&text[cursor..]);
    (output, dropped_tokens)
}

pub(super) fn collapse_pathological_repetitions(text: &str) -> (String, usize) {
    let (sentences_cleaned, sentence_drops) = collapse_pathological_sentence_runs(text);
    let (tokens_cleaned, token_drops) = collapse_pathological_token_cycles(&sentences_cleaned);
    (tokens_cleaned, sentence_drops.saturating_add(token_drops))
}

pub(super) fn join_transcript_parts(parts: impl IntoIterator<Item = String>) -> Option<String> {
    let joined = parts
        .into_iter()
        .filter_map(|part| {
            let part = part.trim().to_string();
            (!part.is_empty()).then_some(part)
        })
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}
