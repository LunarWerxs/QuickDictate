//! Tests for edit application, the trust rules, and the change caps.

use std::time::Instant;

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
fn an_overlapping_second_occurrence_is_still_ambiguous() {
    // The second "no no" starts inside the first one; a non-overlapping
    // search never sees it.
    assert!(apply_edits(
        "I said no no no to that whole plan today",
        &[edit("no no", "no")]
    )
    .is_none());
    // Multi-byte text right after the hit must not split a char.
    assert!(apply_edits("é é é and then some more words", &[edit("é é", "é")]).is_none());
    // A genuinely unique `before` still applies.
    assert_eq!(
        apply_edits(
            "I said no no to that whole plan today",
            &[edit("no no", "no")]
        )
        .as_deref(),
        Some("I said no to that whole plan today")
    );
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

/// The fastest of `rounds` timings of `work`, so one stall on a loaded
/// machine cannot decide a ratio built from it.
fn best_of(rounds: usize, mut work: impl FnMut()) -> Duration {
    (0..rounds)
        .map(|_| {
            let started = Instant::now();
            work();
            started.elapsed()
        })
        .min()
        .unwrap_or(Duration::MAX)
}

#[tokio::test]
async fn a_dead_endpoint_costs_the_deadline_and_nothing_more() {
    // The bound that matters: a broken cleanup pass can never cost more
    // than the budget the user set for it. Measured as a RATIO, never a
    // budget: `resolve` against a bare wait of the same deadline timed beside
    // it, best of five rounds on both sides, so the machine's load cancels
    // out. On this single-threaded test runtime the spawned pass cannot run
    // while `resolve` blocks, so every round is a deadline expiring.
    let p = Polisher::new(tokio::runtime::Handle::current());
    let settings = dead_endpoint(50);
    let resolving = best_of(5, || assert!(p.resolve(&settings, LONG_ENOUGH).is_none()));
    let deadline_alone = best_of(5, || {
        let (_tx, rx) = std::sync::mpsc::sync_channel::<String>(1);
        let _ = rx.recv_timeout(settings.deadline);
    });
    assert!(
        resolving < deadline_alone * 2,
        "resolve blocked for {resolving:?} against a {deadline_alone:?} deadline"
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
    // Counted, not timed: the only wait in `resolve` is on a pass it started
    // or joined, so a call that leaves no pass in flight and no waiter behind
    // never waited, however long a busy machine took to run it.
    let p = Polisher::new(tokio::runtime::Handle::current());
    assert!(p.resolve(&dead_endpoint(3_000), "too short").is_none());
    let st = p.state.lock();
    assert!(
        st.inflight.is_none() && st.waiters.is_empty(),
        "a text too short to polish started or joined a pass"
    );
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
    assert_eq!(
        p.resolve(&dead_endpoint(3_000), LONG_ENOUGH),
        Some(polished)
    );
    // The whole point of speculating: the hit costs nothing. Counted, not
    // timed: no pass was started or joined, so nothing was waited for.
    let st = p.state.lock();
    assert!(
        st.inflight.is_none() && st.waiters.is_empty(),
        "a speculated answer started or joined a pass"
    );
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

// Contract: each model family gets the request its API accepts. Regression
// (each measured live 2026-10-02): `temperature: 0` makes gpt-5.5, gpt-5.6-*,
// gpt-6-* and o4-mini answer 400, `reasoning_effort` makes gpt-4.1-* answer
// 400, and a reasoning model left at its default effort thinks for seconds,
// so any of these silently ends the cleanup pass for that model.
#[test]
fn request_shape_follows_the_model_family() {
    let shape = |model: &str| {
        let body = request_body(model, "text", reasoning_effort(model));
        (
            body.get("temperature").is_some(),
            body.get("reasoning_effort")
                .and_then(|v| v.as_str())
                .map(String::from),
        )
    };
    assert_eq!(shape("gpt-4.1-mini"), (true, None));
    assert_eq!(
        shape("gemini-flash-lite-latest"),
        (true, Some("low".into()))
    );
    assert_eq!(shape("gpt-5.4-mini"), (false, Some("none".into())));
    assert_eq!(shape("gpt-6-luna"), (false, Some("none".into())));
    assert_eq!(shape("o4-mini"), (false, Some("none".into())));
}

/// Keys from the gitignored `my.keys.env` (the live STT tests' file).
fn live_openai_keys() -> Vec<String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/my.keys.env");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.trim().strip_prefix("OPENAI_KEYS=").map(String::from))
        .map(|v| {
            v.split(',')
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

// The real API is the only proof the shapes above are accepted, including
// the "none" -> "low" fallback a low-only model (gpt-6-astra) needs.
#[tokio::test]
#[ignore = "live network + real OpenAI key"]
async fn live_reasoning_models_answer_the_cleanup_request() {
    let client = reqwest::Client::new();
    let text = "So I was looking at the numbers and. I think we should push the launch.";
    for model in ["gpt-5.4-mini", "gpt-6-astra", "gpt-4.1-mini"] {
        let mut last = String::from("no key in my.keys.env");
        let mut ok = false;
        for key in live_openai_keys() {
            let settings = PolishSettings {
                endpoint: "https://api.openai.com/v1/chat/completions".into(),
                model: model.into(),
                keys: vec![key.clone()],
                deadline: Duration::from_secs(10),
            };
            match request_edits(&client, &settings, &key, text).await {
                Ok(edits) => {
                    eprintln!(
                        "{model}: {} edit(s), effort now {:?}",
                        edits.len(),
                        reasoning_effort(model)
                    );
                    ok = true;
                    break;
                }
                // A dead or unfunded key is the key's problem; try the next.
                Err(e) if e.starts_with("HTTP 401") || e.starts_with("HTTP 429") => last = e,
                Err(e) => panic!("{model}: {e}"),
            }
        }
        assert!(ok, "{model}: every key failed; last: {last}");
    }
    assert_eq!(reasoning_effort("gpt-6-astra"), Some("low"));
    assert_eq!(reasoning_effort("gpt-5.4-mini"), Some("none"));
}
