//! App glue for the sign-in nudge: persistence, identity, and the one place that decides.
//!
//! [`crate::nudge_engine`] is a **verbatim** vendored copy of LunarWerx's shared engine
//! (`packages/connections-connect/ports/nudge.rs`). Keeping it byte-identical is the point: a
//! behavioural drift between QuickDictate and every other app that ships this prompt shows up as a
//! plain `diff` rather than as two products that quietly disagree about how often to ask. So the
//! engine is never edited here — everything QuickDictate-specific lives in this file instead.
//!
//! What this file owns:
//!
//!   * **Persistence.** The engine hands back plain fields and no opinion about storage. This
//!     writes them to `quickdictate-nudge.json` beside the other data files, through the same
//!     [`crate::paths::data_file`] the stats and sync blobs use, so a portable install carries the
//!     prompt's memory with it instead of resetting on every machine.
//!   * **Signed-in truth.** The engine asks the host; the host asks [`crate::sync::is_signed_in`].
//!   * **A save that never loses dictation.** Every write is best-effort. A read-only data folder
//!     costs the user nothing worse than being asked again later, and it must never be able to
//!     take down a dictation hotkey — so nothing in here returns an error upward or panics.
//!
//! ## Why the state is re-declared instead of `#[derive(Serialize)]`d on the engine
//!
//! Deriving serde on the vendored file would mean editing it, which is the one thing that makes
//! the diff-against-upstream check stop working. [`Stored`] is that boundary written out: it is
//! the on-disk shape, it is versioned, and it converts. It also means a hand-edited or truncated
//! file degrades to "start over, ask later" rather than to a parse error at startup.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::nudge_engine::{Ask, Cadence, Campaign, Config, NudgeState, Outcome, StopReason};

/// The slug the landing page keys off. Must match the `quickdictate` entry in Connections'
/// `nudge-apps.ts` registry — a mismatch is not an error anywhere, it just silently downgrades the
/// page the user lands on to its generic form, which is exactly the kind of failure nobody notices.
const APP_ID: &str = "quickdictate";
const APP_NAME: &str = "QuickDictate";

/// Beside `quickdictate-stats.json` and the sync credential blob, for the reason in the module doc.
const STATE_FILE: &str = "quickdictate-nudge.json";

// ===== on-disk shape =====

/// The persisted form of [`NudgeState`].
///
/// `#[serde(default)]` throughout: a file written by an older build is missing fields, and the
/// correct response to that is a default, never a refusal to load. Enums ride as lowercase strings
/// so the file stays readable to a human deciding whether to delete it.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Stored {
    v: u32,
    installed_at: u64,
    session_count: u32,
    last_ask_at: Option<u64>,
    ask_count: u32,
    consecutive_declines: u32,
    cadence: String,
    stopped: Option<String>,
    pending_ask: Option<StoredPending>,
    converted: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct StoredPending {
    at: u64,
    trigger: String,
    campaign: String,
}

fn cadence_name(c: Cadence) -> &'static str {
    match c {
        Cadence::Default => "default",
        Cadence::Monthly => "monthly",
    }
}

fn cadence_from(name: &str) -> Cadence {
    match name {
        "monthly" => Cadence::Monthly,
        // `"never"` was a real cadence until 2026-08-27 and the engine has no such state now.
        // It never shipped, so the only files carrying it are on our own machines — but somebody
        // there did press that button, and reading their explicit "leave me alone" as the DAILY
        // default would be the rudest possible interpretation. Monthly is the quietest cadence
        // that still exists, so that is what an old opt-out becomes.
        "never" => Cadence::Monthly,
        // Anything else — a hand-edit, a field from a future build — falls back to the default.
        _ => Cadence::Default,
    }
}

fn campaign_name(c: Campaign) -> &'static str {
    match c {
        Campaign::SignIn => "sign-in",
        Campaign::Discover => "discover",
    }
}

fn campaign_from(name: &str) -> Option<Campaign> {
    match name {
        "sign-in" => Some(Campaign::SignIn),
        "discover" => Some(Campaign::Discover),
        _ => None,
    }
}

fn stop_name(r: StopReason) -> &'static str {
    match r {
        StopReason::LadderExhausted => "ladder-exhausted",
    }
}

impl From<&NudgeState> for Stored {
    fn from(s: &NudgeState) -> Self {
        Self {
            v: s.version,
            installed_at: s.installed_at,
            session_count: s.session_count,
            last_ask_at: s.last_ask_at,
            ask_count: s.ask_count,
            consecutive_declines: s.consecutive_declines,
            cadence: cadence_name(s.cadence).into(),
            stopped: s.stopped.map(|r| stop_name(r).into()),
            pending_ask: s.pending_ask.as_ref().map(|p| StoredPending {
                at: p.at,
                trigger: p.trigger.clone(),
                campaign: campaign_name(p.campaign).into(),
            }),
            converted: s
                .converted
                .iter()
                .map(|c| campaign_name(*c).into())
                .collect(),
        }
    }
}

impl Stored {
    /// Whether this looks like something QuickDictate actually wrote.
    ///
    /// Needed because `#[serde(default)]` is more permissive than it looks: serde accepts a
    /// struct's sequence form too, so a file containing `[]` parses cleanly into an all-zero
    /// `Stored`. That is not a harmless empty state — `installed_at: 0` claims an install in 1970,
    /// which satisfies the engine's one-week age gate instantly and would let a corrupted or
    /// hand-mangled file open a prompt that should have waited a week. Found by the round-trip
    /// test below, which is the only reason it is not still in here.
    ///
    /// Both checks are things a real file can never be: this app writes [`STATE_VERSION`] (1) and
    /// stamps `installed_at` from the wall clock, which has not been zero since 1970.
    fn is_plausible(&self) -> bool {
        self.v > 0 && self.installed_at > 0
    }

    fn into_state(self) -> NudgeState {
        NudgeState {
            version: self.v,
            installed_at: self.installed_at,
            session_count: self.session_count,
            last_ask_at: self.last_ask_at,
            ask_count: self.ask_count,
            consecutive_declines: self.consecutive_declines,
            cadence: if self.stopped.is_some() {
                Cadence::Monthly
            } else {
                cadence_from(&self.cadence)
            },
            // A stored stop describes a promise the engine no longer makes, and there are only
            // two honest readings: keep them silenced forever, which the owner has ruled out, or
            // resurrect them into daily prompts, which is the rudest reading of somebody who
            // pressed "don't ask again". Neither. It becomes the monthly cadence above.
            stopped: None,
            pending_ask: self.pending_ask.and_then(|p| {
                campaign_from(&p.campaign).map(|campaign| crate::nudge_engine::PendingAsk {
                    at: p.at,
                    trigger: p.trigger,
                    campaign,
                })
            }),
            converted: self
                .converted
                .iter()
                .filter_map(|c| campaign_from(c))
                .collect(),
        }
    }
}

// ===== the live state =====

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn config() -> Config {
    let mut cfg = Config::new(APP_ID, APP_NAME);
    cfg.app_version = Some(env!("CARGO_PKG_VERSION").to_string());
    cfg
}

/// One process-wide state, loaded on first touch.
///
/// A `Mutex` rather than a channel or an `ArcSwap`: every operation here is a few field writes
/// followed by a small file write, it happens at most a handful of times per run, and the lock is
/// never held across anything that blocks on the network.
static STATE: Mutex<Option<NudgeState>> = Mutex::new(None);

/// Turn whatever is on disk into a usable state. Never fails.
///
/// Split out from [`load`] so the degradation can actually be tested — the interesting inputs here
/// are a truncated file, a hand-edit and a blob from a future build, none of which a test can
/// exercise through a function that reads a fixed path out of the user's real data folder.
fn parse_or_fresh(raw: &str, now: u64) -> NudgeState {
    match serde_json::from_str::<Stored>(raw) {
        Ok(stored) if !stored.is_plausible() => {
            tracing::debug!("nudge: implausible state on disk, starting fresh");
            NudgeState::new(now)
        }
        Ok(stored) => {
            let mut state = stored.into_state();
            // `sanitize` is the engine's own repair pass — a moved clock, a rolled-back release, a
            // hand-edit. Every branch of it degrades toward asking LESS, so running it on load is
            // strictly safer than trusting the file.
            state.sanitize(now);
            state
        }
        Err(e) => {
            tracing::debug!("nudge: unreadable state, starting fresh ({e})");
            NudgeState::new(now)
        }
    }
}

fn load() -> NudgeState {
    let path = crate::paths::data_file(STATE_FILE);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return NudgeState::new(now_ms());
    };
    parse_or_fresh(&raw, now_ms())
}

fn persist(state: &NudgeState) {
    let path = crate::paths::data_file(STATE_FILE);
    let Ok(json) = serde_json::to_string_pretty(&Stored::from(state)) else {
        return;
    };
    if let Err(e) = std::fs::write(&path, json) {
        // Best-effort by design: see the module doc. A read-only data folder means the user gets
        // asked again another day, which is a far better outcome than a failed dictation.
        tracing::debug!("nudge: could not save state to {}: {e}", path.display());
    }
}

/// Run `f` against the live state, persisting whatever it changed.
fn with_state<T>(f: impl FnOnce(&mut NudgeState) -> T) -> T {
    let mut guard = match STATE.lock() {
        Ok(guard) => guard,
        // A poisoned lock means another thread panicked mid-update. The state is a prompt
        // schedule, not user data: recovering the value is the right call, and the alternative
        // (propagating the panic) would take out whatever thread happened to touch it next.
        Err(poisoned) => poisoned.into_inner(),
    };
    let state = guard.get_or_insert_with(load);
    let out = f(state);
    persist(state);
    out
}

// ===== the app-facing surface =====

/// Count this launch. Call once, early, before any window exists.
///
/// This is also what settles an ask the last run left on screen when the user quit: the engine
/// counts that as a decline, so forgetting to report an outcome makes QuickDictate ask *less*.
pub(crate) fn start_session() {
    let cfg = config();
    with_state(|s| s.start_session(&cfg, now_ms()));
}

/// Decide whether to ask right now. Returns `None` far more often than not.
///
/// `trigger` names the moment — it selects the copy and rides the attribution link. The one
/// QuickDictate fires is `settings-changed`, because settings are exactly what the account keeps.
pub(crate) fn consider(trigger: &str) -> Option<Ask> {
    let cfg = config();
    let signed_in = crate::sync::is_signed_in();
    with_state(|s| s.consider(&cfg, trigger, signed_in, now_ms()))
}

/// Report what the user did with the ask that is on screen.
pub(crate) fn record(outcome: Outcome) {
    let cfg = config();
    with_state(|s| s.record(&cfg, outcome));
}

/// The user signed in some other way (the sync card, a fresh install restoring credentials).
/// Retires the sign-in campaign so it is never asked again.
pub(crate) fn mark_signed_in() {
    with_state(|s| s.mark_signed_in());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every enum the on-disk shape carries must survive a round trip. A silent mismatch here
    /// would reset the ladder on every launch — the exact failure that made the web version ask
    /// forever, and which nothing in the UI would show.
    #[test]
    fn enum_names_round_trip() {
        for c in [Cadence::Default, Cadence::Monthly] {
            assert_eq!(cadence_from(cadence_name(c)), c);
        }
        for c in [Campaign::SignIn, Campaign::Discover] {
            assert_eq!(campaign_from(campaign_name(c)), Some(c));
        }
        // StopReason has one variant and this app can no longer reach it, so there is nothing
        // left to round-trip.
    }

    /// A file written by a build that still had a permanent opt-out comes back as MONTHLY:
    /// quiet, but not silenced forever. Both halves matter, so both are asserted.
    #[test]
    fn a_legacy_permanent_stop_becomes_monthly() {
        for (cadence, stopped) in [
            ("never", Some("user-opted-out")),
            ("default", Some("declined")),
            ("never", None),
        ] {
            let stored = Stored {
                v: 1,
                installed_at: 1_000,
                session_count: 9,
                cadence: cadence.into(),
                stopped: stopped.map(str::to_string),
                ..Default::default()
            };
            let state = stored.into_state();
            assert_eq!(state.cadence, Cadence::Monthly, "{cadence} / {stopped:?}");
            assert_eq!(state.stopped, None);
        }
    }

    /// A full state must survive serialization unchanged. Written as one round trip rather than
    /// field-by-field assertions so that ADDING a field to the engine and forgetting it here fails
    /// this test instead of silently dropping it on every save.
    #[test]
    fn state_round_trips_through_json() {
        let mut state = NudgeState::new(1_000);
        state.session_count = 7;
        state.ask_count = 2;
        state.last_ask_at = Some(900);
        state.consecutive_declines = 1;
        state.cadence = Cadence::Monthly;
        // `stopped` is deliberately NOT round-tripped - see `into_state`. Left `None` so this
        // stays a test of the fields that DO survive; the conversion has its own test above.
        state.pending_ask = Some(crate::nudge_engine::PendingAsk {
            at: 950,
            trigger: "settings-changed".into(),
            campaign: Campaign::SignIn,
        });
        state.converted.insert(Campaign::Discover);

        let json = serde_json::to_string(&Stored::from(&state)).expect("serialize");
        let back = serde_json::from_str::<Stored>(&json)
            .expect("deserialize")
            .into_state();
        assert_eq!(back, state);
    }

    /// Corrupt, truncated and empty files must all mean "start over", never a panic and never an
    /// error a caller has to handle. `"{"` is the shape a half-written file takes after a power
    /// cut; the rest are hand-edits and a wrong-typed field from some future build.
    ///
    /// "Start over" is checked as `session_count == 0`, i.e. that the returned state really is a
    /// fresh one rather than a half-populated struct carrying garbage forward.
    #[test]
    fn unreadable_state_degrades_to_fresh() {
        for raw in [
            "",
            "{",
            "[]",
            "not json at all",
            "{\"v\":\"not a number\"}",
            // Parses fine and is still not ours: `installed_at: 0` would claim a 1970 install and
            // satisfy the week-old gate on the spot.
            "{\"v\":1,\"installed_at\":0}",
            "{\"v\":0,\"installed_at\":5000}",
        ] {
            let state = parse_or_fresh(raw, 5_000);
            assert_eq!(state, NudgeState::new(5_000), "input {raw:?}");
        }
    }

    /// A state whose clock is in the future — a timezone fix, a VM snapshot, a dead CMOS battery —
    /// must be pulled back rather than left to hold the gate shut forever. The engine's `sanitize`
    /// does this; the assertion here is that loading actually RUNS it, which is easy to drop.
    #[test]
    fn loading_runs_the_engines_repair_pass() {
        let mut future = NudgeState::new(9_000);
        future.last_ask_at = Some(9_000);
        let raw = serde_json::to_string(&Stored::from(&future)).expect("serialize");

        let state = parse_or_fresh(&raw, 5_000);
        assert_eq!(state.installed_at, 5_000);
        assert_eq!(state.last_ask_at, Some(5_000));
    }

    /// The slug is the join with the landing page. Asserting it here means renaming the app cannot
    /// silently send users to the generic page.
    #[test]
    fn app_id_matches_the_landing_page_slug() {
        assert_eq!(APP_ID, "quickdictate");
        let cfg = config();
        assert!(cfg.link_base.contains("connections.icu"));
    }
}
