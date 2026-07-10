use std::collections::BTreeMap;

use once_cell::sync::Lazy;
use regex::Regex;

static SPACE_BEFORE_PUNCT: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+([,.;:?!])").unwrap());
// Match a punctuation char followed by a letter; we'll splice a space between
// them via capture groups. (The previous `(?=...)` look-ahead form is not
// supported by the `regex` crate and panicked at runtime.)
static AFTER_PUNCT: Lazy<Regex> = Lazy::new(|| Regex::new(r"([,.;:?!])([A-Za-z])").unwrap());
static SENTENCE_GAP: Lazy<Regex> = Lazy::new(|| Regex::new(r"([.?!]\s+)([a-z])").unwrap());
static SENTENCE_GLUE: Lazy<Regex> = Lazy::new(|| Regex::new(r"([.?!])([A-Z])").unwrap());
static LONE_I: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(i)\b").unwrap());
static FILLER_PHRASES: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:m+[-\s]?h+m+|uh[-\s]?huh|um+|uh+|erm+)\b(?:[,.!?;:]+\s*|\s+|$)").unwrap()
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
        .map(|(t, u)| {
            (
                Regex::new(&format!(r"(?i)\b{}\b", regex::escape(t))).unwrap(),
                *u,
            )
        })
        .collect()
});

/// Mixed-case proper-noun substitutions.
static DEV_TERMS_MIXED: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        (Regex::new(r"(?i)\bjavascript\b").unwrap(), "JavaScript"),
        (Regex::new(r"(?i)\btypescript\b").unwrap(), "TypeScript"),
        (Regex::new(r"(?i)\bpython\b").unwrap(), "Python"),
        (Regex::new(r"(?i)\bvs ?code\b").unwrap(), "VS Code"),
    ]
});

/// Smart punctuation, capitalization, replacement, etc.
pub struct TextProcessor {
    replacements: Vec<(Regex, String)>,
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
        let mut replacements = Vec::with_capacity(map.len());
        for (k, v) in map {
            let escaped = regex::escape(k);
            // Word-boundary, case-insensitive.
            if let Ok(re) = Regex::new(&format!(r"(?i)\b{}\b", escaped)) {
                replacements.push((re, v.clone()));
            }
        }
        Self {
            replacements,
            auto_punct,
            auto_space,
            auto_newline,
        }
    }

    pub fn process(&self, raw: &str) -> String {
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
            t = self.smart_punctuation(&t);
        }
        if self.auto_newline {
            t.push('\n');
        } else if self.auto_space && !t.is_empty() && !t.ends_with(' ') {
            t.push(' ');
        }
        t
    }

    fn apply_replacements(&self, t: &str) -> String {
        let mut out = t.to_string();
        for (re, repl) in &self.replacements {
            out = re.replace_all(&out, repl.as_str()).into_owned();
        }
        out
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

    fn smart_punctuation(&self, t: &str) -> String {
        let mut s = SPACE_BEFORE_PUNCT.replace_all(t, "$1").into_owned();
        s = AFTER_PUNCT.replace_all(&s, "$1 $2").into_owned();
        // Capitalize first letter.
        if let Some(first) = s.chars().next() {
            if first.is_lowercase() {
                let mut chars = s.chars();
                let upper: String = chars.next().unwrap().to_uppercase().collect();
                s = format!("{upper}{}", chars.as_str());
            }
        }
        // Capitalize letter after sentence-ending punct.
        s = SENTENCE_GAP
            .replace_all(&s, |c: &regex::Captures| {
                let punct = c.get(1).unwrap().as_str();
                let letter = c.get(2).unwrap().as_str().to_ascii_uppercase();
                format!("{punct}{letter}")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn processor() -> TextProcessor {
        TextProcessor::new(&BTreeMap::new(), true, false, false)
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
