//! Opt-in, local-only error/crash reports.
//!
//! WHY: QuickDictate already keeps rotating diagnostic files (`quickdictate.log`,
//! `quickdictate-panic.log` -- see `crate::logging`), but there was no way for a
//! user to turn a crash or an STT-provider failure they hit into something
//! LunarWerx could actually learn about. Nothing in this codebase runs a
//! LunarWerx error-reporting endpoint -- only the settings-sync store
//! (`crate::sync`) and the update-check proxy (`crate::update`) exist -- so,
//! per the same privacy posture `sync` already established (never audio,
//! never transcripts) and never a third-party service, this stays entirely
//! local: assemble a plain-text report, let the user review and edit it, and
//! only if they choose to save it, write a timestamped `.txt` file they can
//! attach to a GitHub issue themselves. Nothing in this module ever makes a
//! network call.
//!
//! Strictly opt-in (`Config::error_reporting_enabled`, off by default) -- see
//! `settings_ui::application` for the toggle and the "Create an error
//! report..." action that builds and previews one before anything is written
//! to disk.

use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;

/// Folder error reports are saved into, inside the configured data folder
/// (see `crate::paths::data_dir`) -- a sibling of `logs/`. `pub(crate)` so
/// `settings_ui`'s "Open folder" button names the same directory [`save_report`]
/// actually writes into, rather than a second hardcoded copy of the name.
pub(crate) const REPORTS_DIR_NAME: &str = "error-reports";

/// Compile a pattern that is a literal in this source file. Mirrors
/// `text.rs::literal_regex`: these are compile-time literals, so a bad one is
/// unreachable in a shipped build, and `every_redaction_pattern_compiles`
/// below forces every one of them at test time.
#[allow(
    clippy::expect_used,
    reason = "the pattern is a compile-time literal and every one is forced by a test"
)]
fn literal_regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a literal redaction pattern in error_report.rs failed to compile")
}

/// Things that look like a secret rather than ordinary diagnostic text.
/// Applied to every log line before it is ever shown to the user or written
/// to a report -- defense in depth on top of the fact that QuickDictate does
/// not log API key values by design (see `keys.rs`).
static SECRET_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"(?i)bearer\s+\S+",
        r"(?i)(api[_-]?key|apikey|access[_-]?token|refresh[_-]?token|authorization)\s*[:=]\s*\S+",
        r"\bsk-[A-Za-z0-9_\-]{10,}\b",
        r"\bAIza[A-Za-z0-9_\-]{10,}\b",
        r"\bgsk_[A-Za-z0-9_\-]{10,}\b",
        // A generic long opaque token, deliberately last and coarse: catches
        // whatever the specific patterns above miss (a raw key with no
        // recognizable prefix, a session id, and so on).
        r"\b[A-Za-z0-9_\-]{32,}\b",
    ]
    .iter()
    .map(|p| literal_regex(p))
    .collect()
});

/// Replace anything that looks like a secret in `line` with `[REDACTED]`.
pub(crate) fn redact(line: &str) -> String {
    let mut out = line.to_string();
    for re in SECRET_PATTERNS.iter() {
        out = re.replace_all(&out, "[REDACTED]").into_owned();
    }
    out
}

/// Last `max_lines` lines of `path`, each redacted, oldest first. Empty when
/// the file doesn't exist or can't be read -- a missing log is not an error
/// here, it just means there is nothing to include.
pub(crate) fn tail_redacted(path: &Path, max_lines: usize) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].iter().map(|l| redact(l)).collect()
}

/// Everything [`build_report`] needs, gathered by the caller (`settings_ui`)
/// so this module stays free of any egui/`App` dependency and is directly
/// unit-testable.
pub(crate) struct ReportInputs<'a> {
    pub app_version: &'a str,
    pub provider_label: &'a str,
    pub logging_enabled: bool,
    /// Already redacted (see [`tail_redacted`]).
    pub log_tail: &'a [String],
    /// Already redacted (see [`tail_redacted`]).
    pub panic_tail: &'a [String],
    pub user_note: &'a str,
}

/// Assemble the plain-text report a user reviews before saving. Never
/// includes audio or dictated text, and the log tails passed in are expected
/// to already be redacted by [`tail_redacted`].
pub(crate) fn build_report(inputs: &ReportInputs) -> String {
    let mut out = String::new();
    out.push_str("QuickDictate error report\n");
    out.push_str("=========================\n");
    out.push_str("Generated locally. Nothing here is sent anywhere automatically -- save it\n");
    out.push_str("yourself and attach it to a GitHub issue if you want to share it.\n");
    out.push_str("No audio or dictated text is included.\n\n");

    if !inputs.user_note.trim().is_empty() {
        out.push_str("What happened (from you):\n");
        out.push_str(inputs.user_note.trim());
        out.push_str("\n\n");
    }

    out.push_str(&format!("QuickDictate version: {}\n", inputs.app_version));
    out.push_str("Platform: Windows 10/11 x64\n");
    out.push_str(&format!("Active provider: {}\n", inputs.provider_label));
    out.push_str(&format!(
        "Diagnostic logging (\"Write quickdictate.log\"): {}\n\n",
        if inputs.logging_enabled { "on" } else { "off" }
    ));

    if inputs.panic_tail.is_empty() {
        out.push_str("Recent crashes: none recorded.\n\n");
    } else {
        out.push_str("Recent crashes (quickdictate-panic.log, most recent last):\n");
        for line in inputs.panic_tail {
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }

    if inputs.log_tail.is_empty() {
        if inputs.logging_enabled {
            out.push_str("Recent log lines: none yet.\n");
        } else {
            out.push_str(
                "Recent log lines: none -- \"Write quickdictate.log\" is off, so there was \
                 nothing to include. Turn it on in Settings for more detail next time.\n",
            );
        }
    } else {
        out.push_str("Recent log lines (quickdictate.log, most recent last):\n");
        for line in inputs.log_tail {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Write `contents` to a new timestamped file under
/// `<data_dir>/error-reports/`, creating the folder if needed. Returns the
/// path written. This is the only place an error report ever leaves memory,
/// and it is a local file write -- there is no network call anywhere in this
/// module.
pub(crate) fn save_report(data_dir: &Path, contents: &str) -> std::io::Result<PathBuf> {
    let dir = data_dir.join(REPORTS_DIR_NAME);
    std::fs::create_dir_all(&dir)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("quickdictate-error-report-{now}.txt"));
    std::fs::write(&path, contents)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_redaction_pattern_compiles() {
        // Forcing the Lazy is the assertion: construction is where a bad
        // pattern panics. Each pattern should also actually match its own
        // motivating example.
        assert!(!SECRET_PATTERNS.is_empty());
        assert!(redact("Authorization: Bearer abc123").contains("[REDACTED]"));
    }

    #[test]
    fn redact_masks_bearer_and_key_assignments() {
        // Several patterns can legitimately overlap and cascade (a "Bearer
        // ..." redaction can itself sit inside an "Authorization: ..."
        // match), so this checks the secret is gone rather than pinning the
        // exact surviving punctuation.
        let bearer = redact("sent request with Authorization: Bearer sk-abcdef1234567890");
        assert!(bearer.contains("[REDACTED]"));
        assert!(!bearer.contains("sk-abcdef1234567890"));

        assert_eq!(redact("api_key=sk-abcdef1234567890abcdef"), "[REDACTED]");
        assert_eq!(
            redact("using key AIzaSyD_1234567890abcdefghijklmno"),
            "using key [REDACTED]"
        );
    }

    #[test]
    fn redact_leaves_ordinary_lines_alone() {
        let line = "2026-09-04T12:00:00Z INFO stt: session started provider=elevenlabs";
        assert_eq!(redact(line), line);
    }

    #[test]
    fn redact_masks_a_long_opaque_token_with_no_known_prefix() {
        let line = "session token qWeRtYuIoPaSdFgHjKlZxCvBnM0123456789";
        assert!(redact(line).contains("[REDACTED]"));
        assert!(!redact(line).contains("qWeRtYuIoP"));
    }

    #[test]
    fn tail_redacted_returns_empty_for_a_missing_file() {
        let missing = std::env::temp_dir().join("quickdictate-error-report-tests-missing.log");
        assert!(tail_redacted(&missing, 10).is_empty());
    }

    #[test]
    fn tail_redacted_keeps_only_the_last_n_lines_in_order_and_redacts_them() {
        let dir =
            std::env::temp_dir().join(format!("qd-error-report-tests-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tail.log");
        std::fs::write(
            &path,
            "line1\nline2\napi_key=sk-abcdef1234567890abcdef\nline4\n",
        )
        .unwrap();

        let tail = tail_redacted(&path, 2);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0], "[REDACTED]");
        assert_eq!(tail[1], "line4");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_report_never_mentions_audio_or_transcripts_and_includes_context() {
        let log_tail = vec!["log line one".to_string()];
        let panic_tail = vec!["[123] PANIC thread='main' at src/x.rs:1: boom".to_string()];
        let inputs = ReportInputs {
            app_version: "9.9.9",
            provider_label: "ElevenLabs",
            logging_enabled: true,
            log_tail: &log_tail,
            panic_tail: &panic_tail,
            user_note: "It crashed after I plugged in headphones.",
        };
        let report = build_report(&inputs);
        assert!(report.contains("9.9.9"));
        assert!(report.contains("ElevenLabs"));
        assert!(report.contains("boom"));
        assert!(report.contains("log line one"));
        assert!(report.contains("headphones"));
        assert!(report.contains("No audio or dictated text is included."));
        assert!(!report.to_ascii_lowercase().contains("microphone"));
    }

    #[test]
    fn build_report_says_so_when_there_is_nothing_to_include() {
        let inputs = ReportInputs {
            app_version: "9.9.9",
            provider_label: "Deepgram",
            logging_enabled: false,
            log_tail: &[],
            panic_tail: &[],
            user_note: "",
        };
        let report = build_report(&inputs);
        assert!(report.contains("Recent crashes: none recorded."));
        assert!(report.contains("Write quickdictate.log\" is off"));
    }

    #[test]
    fn save_report_writes_a_readable_file_under_error_reports() {
        let dir = std::env::temp_dir().join(format!(
            "qd-error-report-save-tests-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let path = save_report(&dir, "hello report").expect("save_report should succeed");
        assert!(path.starts_with(dir.join(REPORTS_DIR_NAME)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello report");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
