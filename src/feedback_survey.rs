//! An occasional, cadence-gated "how's it going?" prompt: one question, asked rarely.
//!
//! Adapted from PostHog's Surveys product (`products/surveys/`, MIT) - specifically the idea of a
//! lightweight, cadence-gated in-app survey trigger, not any of PostHog's own code (their survey
//! builder is a Django/React/ClickHouse stack with nothing standalone to lift). What IS reused is
//! this app's own proven pattern for deciding WHEN to interrupt someone: [`crate::nudge_engine`]
//! already solved "don't ask too soon, don't ask too often, no dead end that traps someone" for
//! the sign-in banner, and that same shape - an age-and-usage gate, then a cooldown between asks,
//! forever, with no permanent off - is exactly right for a feedback prompt too.
//!
//! ## Why this is its own small state machine, not a new `Campaign` on `nudge_engine`
//!
//! `nudge_engine.rs` is a byte-for-byte vendored copy of LunarWerx's shared
//! `packages/connections-connect/ports/nudge.rs`, shared with SageThumbs 2K - see that file's own
//! header, `nudge.rs`'s module doc, and `CONTRIBUTING.md`'s module-split exception, all of which
//! say plainly: never edit it here. Adding a feedback `Campaign` variant there would mean editing
//! the vendored copy from inside this checkout, breaking the one property that file exists to
//! guarantee (a plain diff against upstream proving no local drift), and the change would need to
//! land in the shared package first regardless. So the CADENCE PATTERN is adapted fresh, in this
//! file, with its own tiny state, its own copy, and its own persistence - genuinely simpler than
//! the sign-in engine besides, since this survey has one question and one cadence, no
//! month-escalation ladder: "occasional" is the whole design here, not something a fourth
//! dismissal has to earn.
//!
//! ## What this asks, and why answering leaves the app
//!
//! There is no server here to receive a typed answer - QuickDictate keeps everything local by
//! design (see `stats.rs`) - so the one question is really "mind spending 30 seconds telling us
//! how it's going?", and saying yes opens the system browser to a new, pre-filled GitHub issue
//! against this project rather than pretending to collect free text nowhere stores. Declining
//! costs nothing and is not held against anyone; the gate simply waits its interval and offers
//! again later.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Days installed before the first ask. Longer than the sign-in nudge's week: someone asked to
/// opine on how a tool is working for them should have actually used it for a while first.
const MIN_AGE_MS: u64 = 14 * DAY_MS;
/// Dictation sessions before the first ask.
const MIN_SESSIONS: u32 = 5;
/// Gap between asks, including from the very first one to the next: a quarter, not a day. This
/// IS the cadence - "occasional" is the design, not a ladder that earns its way there over time.
const ASK_GAP_MS: u64 = 90 * DAY_MS;

/// Beside `quickdictate-nudge.json` and the other data files, for the same reason: a portable
/// install should carry the prompt's memory with it instead of resetting on every machine.
const STATE_FILE: &str = "quickdictate-feedback.json";
const STATE_VERSION: u32 = 1;

/// What the user did with the on-screen ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Clicked through to leave feedback. Does not retire anything - it is polite, not final, to
    /// offer again in a quarter even to someone who already spoke up once.
    Shared,
    /// Closed, "Not now", or left unanswered at the next launch.
    Dismissed,
}

/// An ask, handed to the settings UI to render however it likes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ask {
    pub(crate) headline: &'static str,
    pub(crate) body: &'static str,
    pub(crate) action_label: &'static str,
    /// Opens in the system browser: a pre-filled GitHub issue against this project.
    pub(crate) url: String,
}

/// The pure decision state - no file I/O, so it is unit-testable without a real data folder.
/// Mirrors the split `nudge_engine.rs`/`nudge.rs` already use, for the same reason.
///
/// `#[serde(default)]`: a file written by an older build is missing fields, and the correct
/// response is a default, never a refusal to load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
struct FeedbackState {
    v: u32,
    installed_at: u64,
    session_count: u32,
    last_ask_at: Option<u64>,
    ask_count: u32,
    /// An ask is on screen and unanswered.
    pending: bool,
}

impl FeedbackState {
    fn fresh(now: u64) -> Self {
        Self {
            v: STATE_VERSION,
            installed_at: now,
            session_count: 0,
            last_ask_at: None,
            ask_count: 0,
            pending: false,
        }
    }

    /// A file this app actually wrote always has `v > 0` and a real `installed_at`.
    /// `#[serde(default)]` happily parses `[]` or `{}` into an all-zero struct - the same footgun
    /// `nudge.rs`'s `Stored::is_plausible` doc describes - and `installed_at: 0` would satisfy the
    /// two-week gate instantly on a corrupted or hand-mangled file.
    fn is_plausible(&self) -> bool {
        self.v > 0 && self.installed_at > 0
    }

    /// Pull a clock that moved backwards back to `now` - the same repair
    /// `nudge_engine::NudgeState::sanitize` runs on every load. A rewound clock (a timezone fix, a
    /// VM snapshot restore, a dead CMOS battery) must never leave the gate held open forever.
    fn sanitize(&mut self, now: u64) {
        if self.installed_at > now {
            self.installed_at = now;
        }
        if self.last_ask_at.is_some_and(|last| last > now) {
            self.last_ask_at = Some(now);
        }
    }

    fn start_session(&mut self, now: u64) {
        self.sanitize(now);
        self.session_count = self.session_count.saturating_add(1);
        // An ask left on screen when the app quit is settled here, same as the sign-in nudge:
        // never answering is not different from clicking "Not now" - both just let the interval
        // run out and ask again later.
        self.pending = false;
    }

    /// Decide whether to ask right now. Returns `false` far more often than not.
    fn consider(&mut self, now: u64) -> bool {
        self.sanitize(now);
        if self.pending {
            return false;
        }
        if self.session_count < MIN_SESSIONS || now.saturating_sub(self.installed_at) < MIN_AGE_MS {
            return false;
        }
        if self
            .last_ask_at
            .is_some_and(|last| now.saturating_sub(last) < ASK_GAP_MS)
        {
            return false;
        }
        self.ask_count = self.ask_count.saturating_add(1);
        self.last_ask_at = Some(now);
        self.pending = true;
        true
    }

    /// Report what the user did. Both outcomes just settle `pending`; `last_ask_at` already
    /// advanced the gap in `consider`, so there is nothing left for the outcome to change today.
    /// Kept as a real match (not a bare assignment) so a third outcome added later is a compile
    /// error here until someone decides what it does, rather than a silent no-op.
    fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Shared | Outcome::Dismissed => self.pending = false,
        }
    }
}

/// Percent-encode a query value. Hand-rolled, same rule `nudge_engine::encode` uses (RFC 3986
/// unreserved characters pass through, everything else is escaped) - conservative and correct for
/// arbitrary title/body text, including spaces and newlines.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn issue_url(app_version: &str) -> String {
    let body = format!(
        "How's dictation working for you?\n\n\
         (Replace this with whatever you want to tell us - a bug, a rough edge, or just how it's \
         going. The more specific, the more useful.)\n\n\
         QuickDictate v{app_version}"
    );
    format!(
        "{}/issues/new?title={}&body={}",
        crate::about::REPO_URL,
        urlencode("Feedback"),
        urlencode(&body),
    )
}

// ===== persistence and the app-facing surface =====

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One process-wide state, loaded on first touch. See `nudge.rs`'s identical `STATE` for why a
/// plain `Mutex` is the right tool here: a few field writes and a small file write, at most a
/// handful of times per run, never held across anything that blocks on the network.
static STATE: Mutex<Option<FeedbackState>> = Mutex::new(None);

/// Split out from `load` so the degradation can actually be tested - a truncated file, a
/// hand-edit, and a blob with a future version, none of which a test can exercise through a
/// function that reads a fixed path out of the user's real data folder.
fn parse_or_fresh(raw: &str, now: u64) -> FeedbackState {
    match serde_json::from_str::<FeedbackState>(raw) {
        Ok(state) if !state.is_plausible() => {
            tracing::debug!("feedback survey: implausible state on disk, starting fresh");
            FeedbackState::fresh(now)
        }
        Ok(mut state) => {
            if state.v > STATE_VERSION {
                // A future build's shape: discarded rather than guessed at, same rule
                // `NudgeState::sanitize` applies to its own `version` field.
                return FeedbackState::fresh(now);
            }
            state.sanitize(now);
            state
        }
        Err(e) => {
            tracing::debug!("feedback survey: unreadable state, starting fresh ({e})");
            FeedbackState::fresh(now)
        }
    }
}

fn load() -> FeedbackState {
    let path = crate::paths::data_file(STATE_FILE);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return FeedbackState::fresh(now_ms());
    };
    parse_or_fresh(&raw, now_ms())
}

fn persist(state: &FeedbackState) {
    let path = crate::paths::data_file(STATE_FILE);
    let Ok(json) = serde_json::to_string_pretty(state) else {
        return;
    };
    if let Err(e) = std::fs::write(&path, json) {
        // Best-effort by design, same reasoning as `nudge::persist`: a read-only data folder
        // costs the user nothing worse than being asked again another day, and this must never
        // be able to take down a dictation hotkey.
        tracing::debug!(
            "feedback survey: could not save state to {}: {e}",
            path.display()
        );
    }
}

/// Run `f` against the live state, persisting whatever it changed.
fn with_state<T>(f: impl FnOnce(&mut FeedbackState) -> T) -> T {
    let mut guard = match STATE.lock() {
        Ok(guard) => guard,
        // A poisoned lock means another thread panicked mid-update. This state is a prompt
        // schedule, not user data: recovering the value is the right call, matching `nudge.rs`.
        Err(poisoned) => poisoned.into_inner(),
    };
    let state = guard.get_or_insert_with(load);
    let out = f(state);
    persist(state);
    out
}

/// Count this launch. Call once, early, alongside `nudge::start_session`.
pub(crate) fn start_session() {
    with_state(|s| s.start_session(now_ms()));
}

/// Decide whether to ask right now. Returns `None` far more often than not.
pub(crate) fn consider() -> Option<Ask> {
    let asked = with_state(|s| s.consider(now_ms()));
    if !asked {
        return None;
    }
    Some(Ask {
        headline: "Got 30 seconds?",
        body: "You've been dictating with QuickDictate a while now \u{2014} mind sharing quick \
               feedback on how it's going?",
        action_label: "Share feedback",
        url: issue_url(env!("CARGO_PKG_VERSION")),
    })
}

/// Report what the user did with the ask that is on screen.
pub(crate) fn record(outcome: Outcome) {
    with_state(|s| s.record(outcome));
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: u64 = 1_760_000_000_000;

    /// Walks the state to the far side of its gate: five sessions and fifteen days.
    fn open_gate(state: &mut FeedbackState) -> u64 {
        for _ in 0..4 {
            state.start_session(START);
        }
        let now = START + 15 * DAY_MS;
        state.start_session(now);
        now
    }

    #[test]
    fn stays_silent_on_first_run() {
        let mut state = FeedbackState::fresh(START);
        state.start_session(START);
        assert!(!state.consider(START));
    }

    #[test]
    fn stays_silent_for_an_app_installed_long_ago_but_barely_used() {
        let mut state = FeedbackState::fresh(START);
        let now = START + 400 * DAY_MS;
        state.start_session(now);
        assert!(!state.consider(now));
    }

    #[test]
    fn stays_silent_for_heavy_use_on_the_first_afternoon() {
        let mut state = FeedbackState::fresh(START);
        for _ in 0..20 {
            state.start_session(START);
        }
        assert!(!state.consider(START));
    }

    #[test]
    fn asks_once_both_halves_of_the_gate_are_satisfied() {
        let mut state = FeedbackState::fresh(START);
        let now = open_gate(&mut state);
        assert!(state.consider(now));
        assert_eq!(state.ask_count, 1);
    }

    #[test]
    fn never_asks_twice_before_the_gap_elapses() {
        let mut state = FeedbackState::fresh(START);
        let mut now = open_gate(&mut state);
        assert!(state.consider(now));
        state.record(Outcome::Dismissed);

        now += 89 * DAY_MS;
        state.start_session(now);
        assert!(!state.consider(now), "a day short of the quarter");
    }

    #[test]
    fn asks_again_once_the_gap_has_elapsed_and_never_stops_for_good() {
        let mut state = FeedbackState::fresh(START);
        let mut now = open_gate(&mut state);
        for expected in 1..=6u32 {
            assert!(state.consider(now), "ask {expected}");
            assert_eq!(state.ask_count, expected);
            state.record(Outcome::Dismissed);
            now += ASK_GAP_MS + 1;
            state.start_session(now);
        }
    }

    #[test]
    fn sharing_feedback_does_not_retire_the_prompt_either() {
        let mut state = FeedbackState::fresh(START);
        let mut now = open_gate(&mut state);
        assert!(state.consider(now));
        state.record(Outcome::Shared);

        now += ASK_GAP_MS + 1;
        state.start_session(now);
        assert!(
            state.consider(now),
            "answering once should not be the last time we ever ask"
        );
    }

    #[test]
    fn an_unanswered_ask_is_settled_as_a_dismissal_at_the_next_session() {
        let mut state = FeedbackState::fresh(START);
        let now = open_gate(&mut state);
        assert!(state.consider(now));
        assert!(state.pending);
        // App quits with the prompt on screen. Nothing reported.
        state.start_session(now + DAY_MS);
        assert!(!state.pending);
    }

    #[test]
    fn a_clock_that_jumps_backwards_does_not_jam_the_gate() {
        let mut state = FeedbackState::fresh(START);
        let now = open_gate(&mut state);
        assert!(state.consider(now));
        state.record(Outcome::Dismissed);

        let rewound = START - 500 * DAY_MS;
        state.start_session(rewound);
        assert!(!state.consider(rewound));
        assert!(state.installed_at <= rewound);
        assert!(state.last_ask_at.unwrap() <= rewound);
    }

    #[test]
    fn unreadable_state_degrades_to_fresh_never_a_panic() {
        for raw in [
            "",
            "{",
            "[]",
            "not json at all",
            "{\"v\":\"not a number\"}",
            // Parses fine and is still not ours: `installed_at: 0` would claim a 1970 install and
            // satisfy the two-week gate on the spot.
            "{\"v\":1,\"installed_at\":0}",
            "{\"v\":0,\"installed_at\":5000}",
        ] {
            let state = parse_or_fresh(raw, 5_000);
            assert_eq!(state, FeedbackState::fresh(5_000), "input {raw:?}");
        }
    }

    #[test]
    fn state_from_a_future_version_is_discarded_rather_than_guessed_at() {
        let mut state = FeedbackState::fresh(START);
        state.v = 99;
        state.ask_count = 7;
        let raw = serde_json::to_string(&state).expect("serialize");
        let back = parse_or_fresh(&raw, START);
        assert_eq!(back, FeedbackState::fresh(START));
    }

    #[test]
    fn state_round_trips_through_json() {
        let mut state = FeedbackState::fresh(1_000);
        state.session_count = 7;
        state.ask_count = 2;
        state.last_ask_at = Some(900);
        state.pending = true;
        let json = serde_json::to_string(&state).expect("serialize");
        let back = serde_json::from_str::<FeedbackState>(&json).expect("deserialize");
        assert_eq!(back, state);
    }

    #[test]
    fn the_issue_link_points_at_this_projects_repo_and_carries_the_version() {
        let url = issue_url("2.1.0");
        assert!(url.starts_with(crate::about::REPO_URL), "{url}");
        assert!(url.contains("/issues/new?title=Feedback"), "{url}");
        assert!(url.contains("2.1.0"), "{url}");
        // Spaces and punctuation in the body must be encoded, never sent raw into a query string.
        assert!(!url.contains(' '), "{url}");
    }

    #[test]
    fn urlencode_passes_through_unreserved_and_escapes_everything_else() {
        assert_eq!(urlencode("abc-XYZ_012.~"), "abc-XYZ_012.~");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a\nb"), "a%0Ab");
    }
}
