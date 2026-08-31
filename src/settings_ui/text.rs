//! Pure transforms over the settings the window edits: the replacement and
//! vocabulary editors' text form, the config diff behind the unsaved-changes
//! prompt, and the History list's filtering.

use crate::config::Config;

// Split out of this file so each surface can be reviewed on its own; the

// ---- Text-replacement bulk editor ------------------------------------------

/// Serialize replacements to `from => to` lines for the bulk text editor.
pub(super) fn replacements_to_text(rows: &[(String, String)]) -> String {
    rows.iter()
        .map(|(f, t)| format!("{f} => {t}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse `from => to` (or `from = to`) lines back into rows. Blank lines and
/// lines with no separator are skipped.
pub(super) fn text_to_replacements(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (f, t) = line.split_once("=>").or_else(|| line.split_once('='))?;
            let f = f.trim().to_string();
            (!f.is_empty()).then(|| (f, t.trim().to_string()))
        })
        .collect()
}

/// Structural equality for two [`Config`]s via their JSON shape. `Config`
/// doesn't derive `PartialEq` (its key lists and nested `Profile`s make a
/// hand-written impl a maintenance trap that silently goes stale as fields
/// are added), so this compares serialized form instead — correct by
/// construction, at the cost of a serialize round-trip. Backs the "unsaved
/// changes" close-confirm (see `SettingsApp::draft_is_dirty`).
pub(super) fn configs_differ(a: &Config, b: &Config) -> bool {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(a), Ok(b)) => a != b,
        // Serialization failing is not expected; fail toward "differs" so an
        // unsaved edit is never silently discarded on the strength of a
        // could-not-compare.
        _ => true,
    }
}

/// Parse the multi-line custom-vocabulary editor into the list of terms sent
/// to the provider: one non-blank line per term, trimmed. Only run at save
/// time (see `SettingsApp::fold_vocabulary_into_draft`) — the raw text stays
/// untouched while you're still typing, so a blank line mid-edit is never
/// silently swallowed out from under you.
pub(super) fn parse_vocabulary(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Case-insensitive substring match for the History section's filter box. An
/// empty (or whitespace-only) filter matches everything.
pub(super) fn history_matches(filter: &str, text: &str) -> bool {
    let filter = filter.trim();
    filter.is_empty() || text.to_lowercase().contains(&filter.to_lowercase())
}

/// Whether `history_card`'s cached, pre-filtered rows (see [`HistoryCache`](super::app::HistoryCache))
/// need to be rebuilt: true when the underlying history changed
/// (`TranscriptHistory::version()` bumps on every push/pop) or the filter
/// text itself changed since the cache was last built. Pulled out as a pure
/// function so the invalidation rule is testable without a live history.
pub(super) fn history_cache_stale(
    cached_version: u64,
    current_version: u64,
    cached_filter: &str,
    current_filter: &str,
) -> bool {
    cached_version != current_version || cached_filter != current_filter
}

/// Shorten a history entry for its one-line list row; the full text stays
/// reachable via the row's hover tooltip. Truncates on a `char` boundary
/// (never splits a multi-byte character) and appends an ellipsis when cut.
pub(super) fn truncate_preview(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}\u{2026}")
    } else {
        head
    }
}
