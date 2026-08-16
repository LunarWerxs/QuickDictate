use std::collections::BTreeMap;

use once_cell::sync::Lazy;
use regex::{Regex, Replacer};

/// Compile a pattern that is a literal in this source file.
///
/// These are not user input: they are baked into the binary, so "does it
/// compile" is fixed at build time, not at paste time. The panic is therefore
/// unreachable in a shipped build -- and `every_static_pattern_compiles` below
/// forces every one of them, so a bad pattern fails `cargo test` rather than
/// reaching a user. (User-supplied patterns do NOT come through here: text
/// replacements are escaped literals, see `TextProcessor::new`.)
#[allow(
    clippy::expect_used,
    reason = "the pattern is a compile-time literal and every one is forced by a test"
)]
fn literal_regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a literal pattern in text.rs failed to compile")
}

static SPACE_BEFORE_PUNCT: Lazy<Regex> = Lazy::new(|| literal_regex(r"\s+([,.;:?!])"));
// Match a punctuation char followed by a letter; we'll splice a space between
// them via capture groups. (The previous `(?=...)` look-ahead form is not
// supported by the `regex` crate and panicked at runtime.)
static AFTER_PUNCT: Lazy<Regex> = Lazy::new(|| literal_regex(r"([,.;:?!])([A-Za-z])"));
// Capitalize the first letter of the next sentence. `regex` has no
// look-behind, so the character BEFORE the terminator is captured and
// re-emitted verbatim; excluding `.` there stops the LAST dot of an ellipsis
// from reading as a sentence end. Streaming providers (ElevenLabs Scribe
// especially) render a speaker's mid-thought pause as a trailing "...", so
// without that exclusion every pause turned into "trailed off... New Sentence".
// `[^.]` rather than `[^.?!]` so a genuine "Really?! ok" still capitalizes.
static SENTENCE_GAP: Lazy<Regex> = Lazy::new(|| literal_regex(r"(^|[^.])([.?!]\s+)([a-z])"));
static SENTENCE_GLUE: Lazy<Regex> = Lazy::new(|| literal_regex(r"([.?!])([A-Z])"));
static LONE_I: Lazy<Regex> = Lazy::new(|| literal_regex(r"\b(i)\b"));
static FILLER_PHRASES: Lazy<Regex> = Lazy::new(|| {
    literal_regex(r"(?i)\b(?:m+[-\s]?h+m+|uh[-\s]?huh|um+|uh+|erm+)\b(?:[,.!?;:]+\s*|\s+|$)")
});

/// Acronyms that should always be uppercased.
/// Stored as (precompiled regex, uppercased replacement). Built once at first
/// use, not once per paste -- a ~15-regex rebuild was happening on every commit.
static DEV_TERMS_UPPER: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    let terms = [
        "json", "api", "url", "http", "https", "sql", "css", "html", "jwt", "aws",
    ];
    let uppered: [&'static str; 10] = [
        "JSON", "API", "URL", "HTTP", "HTTPS", "SQL", "CSS", "HTML", "JWT", "AWS",
    ];
    terms
        .iter()
        .zip(uppered.iter())
        .map(|(t, u)| (literal_regex(&format!(r"(?i)\b{}\b", regex::escape(t))), *u))
        .collect()
});

/// Mixed-case proper-noun substitutions.
static DEV_TERMS_MIXED: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        (literal_regex(r"(?i)\bjavascript\b"), "JavaScript"),
        (literal_regex(r"(?i)\btypescript\b"), "TypeScript"),
        (literal_regex(r"(?i)\bpython\b"), "Python"),
        (literal_regex(r"(?i)\bvs ?code\b"), "VS Code"),
    ]
});

/// Smart punctuation, capitalization, replacement, etc.
pub struct TextProcessor {
    /// A single alternation regex over every rule's pattern (longest
    /// pattern first) paired with each pattern's literal replacement
    /// value, indexed by capture group. `None` when the map is empty, so
    /// the common "no rules configured" case skips regex work entirely.
    /// See [`Self::build_replacements`] for why this is one combined
    /// regex instead of one regex per rule.
    replacements: Option<(Regex, Vec<String>)>,
    auto_punct: bool,
    auto_space: bool,
    auto_newline: bool,
}

impl TextProcessor {
    pub fn new(
        map: &BTreeMap<String, String>,
        auto_punct: bool,
        auto_space: bool,
        auto_newline: bool,
    ) -> Self {
        Self {
            replacements: Self::build_replacements(map),
            auto_punct,
            auto_space,
            auto_newline,
        }
    }

    /// Builds one alternation regex over every rule's pattern instead of a
    /// regex per rule. The previous approach ran each rule as its own
    /// sequential `replace_all` pass over the progressively-rewritten
    /// output, in `BTreeMap` (alphabetical) key order. That let an
    /// EARLIER rule's replacement text be matched and rewritten again by
    /// a LATER rule, so two independent user rules could silently chain
    /// (e.g. "cat" -> "dog" then "dog" -> "cat" flips "cat" right back).
    /// Matching everything in one simultaneous pass over the ORIGINAL
    /// text can't do that: every match position is decided once, against
    /// text no earlier rule has touched.
    ///
    /// Patterns are sorted longest-first (by character count, ties broken
    /// by the map's existing alphabetical order for determinism) before
    /// being joined with `|`. The `regex` crate's alternation is
    /// leftmost-first: when two patterns could both match starting at the
    /// same position, whichever alternative is listed first wins.
    /// Longest-first makes that deterministic and matches the intuitive
    /// rule that a more specific (longer) phrase should win over a
    /// shorter one it happens to contain.
    fn build_replacements(map: &BTreeMap<String, String>) -> Option<(Regex, Vec<String>)> {
        if map.is_empty() {
            return None;
        }
        let mut rules: Vec<(&String, &String)> = map.iter().collect();
        rules.sort_by_key(|(k, _)| std::cmp::Reverse(k.chars().count()));

        let mut alt = String::new();
        let mut values = Vec::with_capacity(rules.len());
        for (k, v) in rules {
            if !alt.is_empty() {
                alt.push('|');
            }
            // Each pattern gets its own capture group so the replacement
            // closure in `apply_replacements` can tell which rule fired.
            alt.push('(');
            alt.push_str(&regex::escape(k));
            alt.push(')');
            values.push(v.clone());
        }
        // Word-boundary, case-insensitive -- same semantics each per-rule
        // regex used to have on its own; wrapping the whole alternation in
        // one shared `\b...\b` is equivalent because a `\b` assertion only
        // depends on the characters at that position, not on which
        // alternative matched there.
        let pattern = format!(r"(?i)\b(?:{alt})\b");
        Regex::new(&pattern).ok().map(|re| (re, values))
    }

    /// Shorthand for the standalone case. Production always knows whether the
    /// chunk continues the previous one and calls [`Self::process_chunk`]
    /// directly, so this exists for the tests that predate that flag.
    #[cfg(test)]
    pub fn process(&self, raw: &str) -> String {
        self.process_chunk(raw, false)
    }

    /// Turn one raw transcript chunk into the text that gets pasted.
    ///
    /// `continuing` says this chunk carries on the sentence the PREVIOUS chunk
    /// left unfinished, so its first letter is left exactly as the provider
    /// transcribed it.
    ///
    /// The hybrid paste flow delivers every post-release commit as its own
    /// paste, and each one used to be capitalized as if it opened a sentence.
    /// A single spoken thought broken by a pause ("...so I don't want to" /
    /// "significantly slow down the process") therefore came out as
    /// "...so I don't want to... Significantly slow down the process."
    /// See [`ends_mid_sentence`], which is how the caller decides this flag.
    pub fn process_chunk(&self, raw: &str, continuing: bool) -> String {
        if raw.is_empty() {
            return String::new();
        }
        let mut t = raw.to_string();
        t = self.remove_fillers(&t);
        t = self.apply_replacements(&t);
        t = self.fix_formatting(&t);
        t = self.fix_developer_terms(&t);
        t = self.cleanup_punctuation(&t);
        if self.auto_punct {
            t = self.smart_punctuation(&t, continuing);
        }
        if self.auto_newline {
            t.push('\n');
        } else if self.auto_space && !t.is_empty() && !t.ends_with(' ') {
            t.push(' ');
        }
        t
    }

    fn apply_replacements(&self, t: &str) -> String {
        let Some((re, values)) = &self.replacements else {
            return t.to_string();
        };
        re.replace_all(t, |caps: &regex::Captures| -> String {
            // Group 0 is the whole match; groups 1..=values.len() line up
            // 1:1 with `values` in the same order `build_replacements`
            // built them, and exactly one of those groups is `Some` per
            // match. Find which rule's pattern fired and use its value.
            for (i, value) in values.iter().enumerate() {
                if caps.get(i + 1).is_some() {
                    // `regex::NoExpand` inserts `value` byte-for-byte
                    // instead of treating it as a capture-group template.
                    // Without this, a bare `&str` Replacer interprets a
                    // `$` in the replacement text as `$1`/`$name` capture
                    // syntax, so a rule whose value contains a literal
                    // `$` (e.g. "$50") silently ate the `$` and whatever
                    // looked like a group reference after it.
                    let mut buf = String::new();
                    regex::NoExpand(value.as_str()).replace_append(caps, &mut buf);
                    return buf;
                }
            }
            // Unreachable: the outer regex only matches when some inner
            // group matched too.
            String::new()
        })
        .into_owned()
    }

    fn remove_fillers(&self, t: &str) -> String {
        FILLER_PHRASES.replace_all(t, "").into_owned()
    }

    fn fix_formatting(&self, t: &str) -> String {
        let no_space_before_punct = SPACE_BEFORE_PUNCT.replace_all(t, "$1");
        LONE_I.replace_all(&no_space_before_punct, "I").into_owned()
    }

    fn fix_developer_terms(&self, t: &str) -> String {
        let mut s = t.to_string();
        for (re, repl) in DEV_TERMS_UPPER.iter() {
            s = re.replace_all(&s, *repl).into_owned();
        }
        for (re, repl) in DEV_TERMS_MIXED.iter() {
            s = re.replace_all(&s, *repl).into_owned();
        }
        s
    }

    fn cleanup_punctuation(&self, t: &str) -> String {
        // Collapse ".." (exactly two) -> "." but leave "..." (and longer runs) alone.
        // `regex` doesn't support look-around so we scan codepoints manually.
        let mut out = String::with_capacity(t.len());
        let mut iter = t.chars().peekable();
        while let Some(c) = iter.next() {
            if c == '.' {
                let mut run = 1usize;
                while iter.peek() == Some(&'.') {
                    iter.next();
                    run += 1;
                }
                if run == 2 {
                    out.push('.');
                } else {
                    for _ in 0..run {
                        out.push('.');
                    }
                }
            } else if c == ',' {
                // Collapse runs of ',' into a single ','.
                out.push(',');
                while iter.peek() == Some(&',') {
                    iter.next();
                }
            } else {
                out.push(c);
            }
        }
        // Insert a space between sentence-ending punct and an immediately-following capital.
        SENTENCE_GLUE.replace_all(&out, "$1 $2").into_owned()
    }

    fn smart_punctuation(&self, t: &str, continuing: bool) -> String {
        let mut s = SPACE_BEFORE_PUNCT.replace_all(t, "$1").into_owned();
        s = AFTER_PUNCT.replace_all(&s, "$1 $2").into_owned();
        // Capitalize first letter -- unless this chunk resumes a sentence the
        // previous one left hanging, in which case it is mid-sentence and a
        // capital would invent a sentence break the speaker never made.
        if !continuing {
            if let Some(first) = s.chars().next() {
                if first.is_lowercase() {
                    let mut chars = s.chars();
                    chars.next();
                    let upper: String = first.to_uppercase().collect();
                    s = format!("{upper}{}", chars.as_str());
                }
            }
        }
        // Capitalize letter after sentence-ending punct. Group 1 is the
        // character preceding the terminator (possibly empty at the start of
        // the string) and is passed through untouched; see `SENTENCE_GAP`.
        s = SENTENCE_GAP
            .replace_all(&s, |c: &regex::Captures| {
                // All three groups are non-optional in SENTENCE_GAP, so each is
                // always present; `map_or("")` says that without a panic path.
                let before = c.get(1).map_or("", |m| m.as_str());
                let punct = c.get(2).map_or("", |m| m.as_str());
                let letter = c.get(3).map_or("", |m| m.as_str()).to_ascii_uppercase();
                format!("{before}{punct}{letter}")
            })
            .into_owned();
        // Append a period if the sentence looks finished but has no closer.
        if let Some(last) = s.chars().last() {
            if !matches!(last, '.' | '?' | '!' | ',' | ';' | ':') {
                let word_count = s.split_whitespace().count();
                if word_count > 3 || s.len() > 15 {
                    s.push('.');
                }
            }
        }
        s
    }
}

/// Did this already-processed chunk stop mid-thought, so whatever is pasted
/// next continues the same sentence?
///
/// Streaming providers commit a segment at every pause, and ElevenLabs Scribe
/// spells a mid-thought pause out as a trailing "..." -- an explicit "I wasn't
/// finished" marker, not a full stop. A dangling comma, colon, semicolon or
/// hyphen means the same thing. Anything else (a period, a question mark, a
/// bare word the auto-period already closed) is treated as finished, so the
/// next chunk opens a new sentence exactly as before.
///
/// Deliberately conservative: a false negative costs one wrong capital, which
/// is the behavior that shipped before this existed, while a false positive
/// would swallow the capital on a genuinely new sentence.
pub fn ends_mid_sentence(processed: &str) -> bool {
    // `auto_space` / `auto_newline` append a trailer, so compare on the text.
    let t = processed.trim_end();
    if t.ends_with("...") || t.ends_with('\u{2026}') {
        return true;
    }
    matches!(
        t.chars().last(),
        Some(',') | Some(';') | Some(':') | Some('-')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn processor() -> TextProcessor {
        TextProcessor::new(&BTreeMap::new(), true, false, false)
    }

    /// `literal_regex` is allowed to panic only because every pattern it is
    /// handed is a literal in this file that something forces at build time.
    /// This is that something. Each static is `Lazy`, so a broken pattern would
    /// otherwise stay dormant until the first paste that reached it -- on a
    /// background thread, in a build with no console.
    ///
    /// Add a static above, add it here.
    #[test]
    fn every_static_pattern_compiles() {
        // Forcing the Lazy is the assertion: construction is where a bad
        // pattern panics.
        assert!(SPACE_BEFORE_PUNCT.is_match(" ,"));
        assert!(AFTER_PUNCT.is_match(",a"));
        assert!(SENTENCE_GAP.is_match("a. b"));
        assert!(SENTENCE_GLUE.is_match(".A"));
        assert!(LONE_I.is_match("i"));
        assert!(FILLER_PHRASES.is_match("um "));
        assert_eq!(DEV_TERMS_UPPER.len(), 10);
        assert!(DEV_TERMS_UPPER
            .iter()
            .all(|(re, _)| re.is_match("json") || re.is_match("api") || !re.as_str().is_empty()));
        assert_eq!(DEV_TERMS_MIXED.len(), 4);
        assert!(DEV_TERMS_MIXED[0].0.is_match("javascript"));
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(processor().process(""), "");
    }

    #[test]
    fn replacements_are_case_insensitive_and_word_bounded() {
        let p = TextProcessor::new(&map(&[("github", "GitHub")]), true, false, false);
        // Lowercased in the input, matched case-insensitively, then the sentence
        // gets its leading capital (auto_punct) but no trailing period (short).
        assert_eq!(p.process("push to github"), "Push to GitHub");
    }

    #[test]
    fn developer_terms_are_normalized() {
        // json -> JSON, api -> API; 7 words + a finished look -> trailing period.
        assert_eq!(
            processor().process("let's parse the json from the api"),
            "Let's parse the JSON from the API."
        );
    }

    #[test]
    fn strips_space_before_punctuation_and_capitalizes() {
        assert_eq!(
            processor().process("hello world , this is a test"),
            "Hello world, this is a test."
        );
    }

    #[test]
    fn splits_and_capitalizes_run_together_sentences() {
        assert_eq!(
            processor().process("first sentence.second sentence here"),
            "First sentence. Second sentence here."
        );
    }

    #[test]
    fn auto_punct_off_leaves_case_and_terminal_period_alone() {
        // With auto_punct disabled we don't capitalize the first word or append
        // a period, even for a long, sentence-shaped input.
        let p = TextProcessor::new(&BTreeMap::new(), false, false, false);
        assert_eq!(
            p.process("this is a longer sentence with many words"),
            "this is a longer sentence with many words"
        );
    }

    #[test]
    fn auto_space_and_auto_newline_append_the_right_trailer() {
        // auto_space: a single trailing space (and only one).
        let space = TextProcessor::new(&BTreeMap::new(), false, true, false);
        assert_eq!(space.process("hello"), "hello ");
        // auto_newline wins over auto_space and appends a newline.
        let newline = TextProcessor::new(&BTreeMap::new(), false, false, true);
        assert_eq!(newline.process("hello"), "hello\n");
    }

    #[test]
    fn dollar_sign_replacement_values_survive_verbatim() {
        // FIX 1: a bare `&str` Replacer treats `$` in the replacement text
        // as capture-group syntax, so a rule's value containing a literal
        // `$` used to vanish along with whatever followed it.
        let p = TextProcessor::new(
            &map(&[("price tag", "$50"), ("first item", "$1 special")]),
            false,
            false,
            false,
        );
        assert_eq!(p.process("the price tag is set"), "the $50 is set");
        assert_eq!(
            p.process("this is the first item"),
            "this is the $1 special"
        );
    }

    #[test]
    fn independent_rules_do_not_cascade() {
        // FIX 2: the old code applied rules sequentially over the growing
        // output in BTreeMap (alphabetical) key order, so "cat" -> "dog"
        // ran first and then "dog" -> "cat" ran second and found the
        // "dog" the FIRST rule had just produced, flipping it straight
        // back to "cat". A single simultaneous pass over the original
        // text must leave both rules independent.
        let p = TextProcessor::new(&map(&[("cat", "dog"), ("dog", "cat")]), false, false, false);
        assert_eq!(p.process("cat and dog"), "dog and cat");
    }

    #[test]
    fn longest_pattern_wins_on_overlap() {
        // "new york" and "new york city" both start matching at the same
        // position in "new york city". Longest-pattern-first ordering
        // means the longer, more specific rule wins instead of the
        // shorter one consuming its prefix first.
        let p = TextProcessor::new(
            &map(&[("new york", "NYC"), ("new york city", "The Big Apple")]),
            false,
            false,
            false,
        );
        assert_eq!(p.process("i love new york city"), "I love The Big Apple");
    }

    #[test]
    fn default_replacement_map_entries_still_work() {
        // Mirrors two entries from config::default_replacements (chat gpt
        // and github) to spot-check that the FIX 2 rewrite didn't change
        // behavior for the shipped default replacement map.
        let defaults = map(&[("chat gpt", "ChatGPT"), ("github", "GitHub")]);
        let p = TextProcessor::new(&defaults, false, false, false);
        assert_eq!(
            p.process("ask chat gpt about github"),
            "ask ChatGPT about GitHub"
        );
    }

    #[test]
    fn empty_replacement_map_is_a_no_op() {
        let p = TextProcessor::new(&BTreeMap::new(), false, false, false);
        assert_eq!(
            p.process("nothing to replace here"),
            "nothing to replace here"
        );
    }

    #[test]
    fn an_ellipsis_is_a_pause_not_a_sentence_end() {
        // ElevenLabs Scribe commits a segment at every pause and writes the
        // pause as a trailing "...". `SENTENCE_GAP` used to see that last dot
        // as a full stop and capitalize straight through the speaker's
        // mid-sentence breath.
        assert_eq!(
            processor().process("it's quite clear that... the pause was not a full stop"),
            "It's quite clear that... the pause was not a full stop."
        );
        // A real full stop still capitalizes, and so does "?!".
        assert_eq!(
            processor().process("that is done. next we ship it"),
            "That is done. Next we ship it."
        );
        assert_eq!(
            processor().process("really?! ok then let's go"),
            "Really?! Ok then let's go."
        );
    }

    #[test]
    fn a_continuing_chunk_keeps_its_lower_case_opening() {
        let p = processor();
        // The hybrid paste flow delivers each post-release commit separately,
        // so the continuation flag is the only thing that can tell this chunk
        // it is mid-sentence.
        assert_eq!(
            p.process_chunk("significantly slow down the process", true),
            "significantly slow down the process."
        );
        // Same text with no preceding fragment still opens a sentence.
        assert_eq!(
            p.process_chunk("significantly slow down the process", false),
            "Significantly slow down the process."
        );
        // Continuation never suppresses capitalization LATER in the chunk.
        assert_eq!(
            p.process_chunk("way over there. then we stop", true),
            "way over there. Then we stop."
        );
    }

    #[test]
    fn mid_sentence_endings_are_recognized_but_finished_ones_are_not() {
        // Trailer-insensitive: auto_space/auto_newline append one before this
        // ever runs.
        assert!(ends_mid_sentence("so I don't want to... "));
        assert!(ends_mid_sentence("and I also was just fascinated\u{2026}"));
        assert!(ends_mid_sentence("the pause didn't intend to have a-"));
        assert!(ends_mid_sentence("first, "));
        assert!(!ends_mid_sentence("That is the whole point."));
        assert!(!ends_mid_sentence("Is that right?"));
        assert!(!ends_mid_sentence(""));
    }

    #[test]
    fn a_paused_dictation_reads_as_one_sentence_end_to_end() {
        // Verbatim ElevenLabs commits from a real session (log epoch 717),
        // exactly as the hybrid paste flow hands them over: the first four
        // were held until release and joined, the fifth arrived after release
        // and was pasted on its own.
        let p = processor();
        let held = [
            "One of the things I noticed, um, I default to using the ElevenLabs model right now, is often if I pause for too long, the AI will put a bunch of space between my sentences, even though it's quite clear that...",
            "the pause didn't intend to have a-",
            "... uh, punctuation and spacing.",
            "But, for example, if I paste, it will...",
        ];
        let first = p.process(&held.join(" "));
        // The three pause boundaries stay lower-case; the real sentence break
        // after "spacing." still gets its capital.
        assert!(first.contains("quite clear that... the pause didn't intend"));
        assert!(first.contains("to have a-... punctuation and spacing."));
        assert!(first.contains("spacing. But, for example"));

        // The trailing "..." is what tells the caller the next paste continues.
        assert!(ends_mid_sentence(&first));
        let second = p.process_chunk("Immediately paste, but if I wait an extra.", true);
        assert!(second.starts_with("Immediately"), "{second}");
        let third = p.process_chunk("immediately paste, but if I wait an extra.", true);
        assert!(third.starts_with("immediately"), "{third}");
    }

    #[test]
    fn removes_standalone_backchannel_fillers() {
        let p = processor();
        assert_eq!(
            p.process("Let's begin working through. Mm-hmm. The first steps of this."),
            "Let's begin working through. The first steps of this."
        );
        assert_eq!(
            p.process("uh-huh, we can start with the API."),
            "We can start with the API."
        );
        assert_eq!(
            p.process("Uh-oh, that should stay."),
            "Uh-oh, that should stay."
        );
    }
}
