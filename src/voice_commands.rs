//! Voice Commands, precision subset -- capped hard to **"scratch that"**.
//!
//! This is deliberately narrow: the roadmap warns against naive punctuation
//! matching on streamed transcripts, so the small pause-gated punctuation
//! command set (periods/commas/etc. spoken aloud) is **out of scope /
//! deferred** here, not built. The only thing this module does is recognize
//! a FINAL transcript that ends with (or equals) the phrase "scratch that"
//! and turn it into an undo-the-last-paste instruction for the output layer.

/// What to do with a committed transcript once voice-command handling has
/// looked at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScratchThat {
    /// The transcript wasn't a "scratch that" at all -- process normally.
    NotTriggered,
    /// The transcript was (possibly in part) a "scratch that" command.
    /// `remaining_raw` is whatever raw text preceded the command phrase
    /// (may be empty), to be run through normal text processing and pasted
    /// *after* the undo backspaces are sent.
    Triggered { remaining_raw: String },
}

/// Case/punctuation-insensitive check for the trailing "scratch that" (or
/// "scratch that.") command phrase. Only matches at the *end* of the
/// transcript (allowing at most one trailing `.`/`!`/`?`), so "scratch that
/// itch" or "let's scratch that idea and move on" do NOT trigger -- only a
/// transcript that actually *ends* with the command.
///
/// Returns `NotTriggered` when `voice_commands` is off or the phrase isn't
/// present at the end; otherwise `Triggered` with whatever text came before
/// the command phrase (trimmed of trailing whitespace).
pub fn detect(raw: &str, voice_commands_enabled: bool) -> ScratchThat {
    if !voice_commands_enabled {
        return ScratchThat::NotTriggered;
    }
    let trimmed = raw.trim_end();
    // Strip at most one trailing sentence-ending punctuation mark.
    let trimmed = trimmed
        .strip_suffix(['.', '!', '?'])
        .unwrap_or(trimmed)
        .trim_end();

    let Some(prefix_len) = strip_phrase_suffix(trimmed) else {
        return ScratchThat::NotTriggered;
    };

    let remaining_raw = trimmed[..prefix_len].trim_end().to_string();
    ScratchThat::Triggered { remaining_raw }
}

/// If `s` ends with the phrase "scratch that" as its own word-bounded
/// trailing phrase (case-insensitive), returns the byte length of `s` with
/// that trailing phrase (and any whitespace immediately before it) removed.
/// Otherwise `None`.
fn strip_phrase_suffix(s: &str) -> Option<usize> {
    const PHRASE: &str = "scratch that";
    let lower = s.to_ascii_lowercase();
    if !lower.ends_with(PHRASE) {
        return None;
    }
    let candidate_start = lower.len() - PHRASE.len();
    // Must be a whole-word match: the char immediately before the phrase
    // (if any) must not be a letter/digit, so "rescratch that" doesn't count.
    if candidate_start > 0 {
        let before = s[..candidate_start].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric()) {
            return None;
        }
    }
    Some(candidate_start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_off_never_triggers() {
        assert_eq!(detect("scratch that", false), ScratchThat::NotTriggered);
    }

    #[test]
    fn bare_phrase_triggers_with_empty_remainder() {
        assert_eq!(
            detect("scratch that", true),
            ScratchThat::Triggered {
                remaining_raw: String::new()
            }
        );
    }

    #[test]
    fn case_insensitive_variants_trigger() {
        assert_eq!(
            detect("Scratch that", true),
            ScratchThat::Triggered {
                remaining_raw: String::new()
            }
        );
        assert_eq!(
            detect("SCRATCH THAT", true),
            ScratchThat::Triggered {
                remaining_raw: String::new()
            }
        );
    }

    #[test]
    fn trailing_period_variant_triggers() {
        assert_eq!(
            detect("scratch that.", true),
            ScratchThat::Triggered {
                remaining_raw: String::new()
            }
        );
    }

    #[test]
    fn command_with_preceding_text_keeps_the_remainder() {
        assert_eq!(
            detect("The quick brown fox scratch that", true),
            ScratchThat::Triggered {
                remaining_raw: "The quick brown fox".to_string()
            }
        );
        assert_eq!(
            detect("The quick brown fox, scratch that.", true),
            ScratchThat::Triggered {
                remaining_raw: "The quick brown fox,".to_string()
            }
        );
    }

    #[test]
    fn mid_sentence_does_not_trigger() {
        assert_eq!(
            detect("let's scratch that idea and move on", true),
            ScratchThat::NotTriggered
        );
        assert_eq!(
            detect("scratch that itch please", true),
            ScratchThat::NotTriggered
        );
    }

    #[test]
    fn substring_word_boundary_is_respected() {
        // "rescratch that" must not match even though it ends with the phrase.
        assert_eq!(detect("rescratch that", true), ScratchThat::NotTriggered);
    }

    #[test]
    fn empty_input_does_not_trigger() {
        assert_eq!(detect("", true), ScratchThat::NotTriggered);
    }
}
