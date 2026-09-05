//! On-launch crash detection for the Settings window banner strip.
//!
//! WHY: `error_report.rs` can already turn `quickdictate-panic.log` into a redacted, reviewable
//! report (see `settings_ui::application::error_report_section`), but nothing ever told a user
//! that log had something new in it -- they had to remember to go looking. This module is the
//! missing "we noticed" step: at every real launch it compares the panic log's size against what
//! it saw last time, and if the previous run left a fresh entry, it hands `settings_ui` an [`Ask`]
//! to show as a non-modal banner offering to open the (already-redacted) report or dismiss it.
//! Strictly opt-in like the rest of error reporting -- see [`note_launch`] -- and this never makes
//! a network call or reads log contents itself; it only ever compares a file length.
//!
//! Mirrors the split `nudge.rs`/`feedback_survey.rs` already use: a small on-disk state record,
//! `#[serde(default)]` so an old or hand-edited file degrades to "start over" rather than a parse
//! error, and the actual decision kept in a plain function so it is testable without touching
//! `paths::data_dir()` (a process-global) or a real panic log.

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Beside `quickdictate-nudge.json` and `quickdictate-feedback.json`, for the same reason: a
/// portable install carries this prompt's memory with it instead of resetting on every machine.
const STATE_FILE: &str = "quickdictate-crash-banner.json";
const STATE_VERSION: u32 = 1;

/// The on-disk record of the panic log length we last accounted for.
///
/// `#[serde(default)]`: a file written by an older build (or none at all, or a hand-edit) is
/// missing fields, and the correct response is a default, never a refusal to load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
struct CrashBannerState {
    v: u32,
    last_seen_panic_len: u64,
}

/// A file this app actually wrote always has `v > 0`. A missing/corrupt/never-written file (`v ==
/// 0`) must never be read as "the panic log grew since we last checked" -- that would surface
/// every historical crash from before this feature (or this install) existed as if it just
/// happened. `v == 0` means "we have no baseline yet", so [`decide`] always treats it as quiet.
impl CrashBannerState {
    fn is_plausible(&self) -> bool {
        self.v > 0
    }
}

/// An ask, handed to the settings UI to render as a banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ask {
    pub(crate) headline: &'static str,
    pub(crate) body: &'static str,
}

/// The pending ask from the most recent [`note_launch`], if any. A plain `Mutex`, same reasoning
/// as `update::PENDING_UPDATE`: a couple of field reads/writes, never held across anything that
/// blocks.
static PENDING: Mutex<Option<Ask>> = Mutex::new(None);

/// Pure decision, split out of [`note_launch`] so it is unit-testable with no real files: does
/// `current_len` bytes of panic log, against a `previous` baseline, count as a fresh crash worth
/// surfacing? Off entirely unless `error_reporting_enabled` -- the whole point is to point at the
/// opt-in error-report feature, so it stays quiet for anyone who hasn't turned that on.
fn decide(previous: CrashBannerState, current_len: u64, error_reporting_enabled: bool) -> bool {
    error_reporting_enabled && previous.is_plausible() && current_len > previous.last_seen_panic_len
}

fn load(path: &Path) -> CrashBannerState {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return CrashBannerState::default();
    };
    match serde_json::from_str::<CrashBannerState>(&raw) {
        Ok(state) if state.is_plausible() && state.v <= STATE_VERSION => state,
        // Implausible, unreadable, or from a future build: never guessed at, just reset to "no
        // baseline yet" -- see the doc on `CrashBannerState::is_plausible`.
        _ => CrashBannerState::default(),
    }
}

fn persist(path: &Path, current_len: u64) {
    let state = CrashBannerState {
        v: STATE_VERSION,
        last_seen_panic_len: current_len,
    };
    let Ok(json) = serde_json::to_string_pretty(&state) else {
        return;
    };
    if let Err(e) = std::fs::write(path, json) {
        // Best-effort by design, same reasoning as `nudge::persist`: a read-only data folder
        // costs the user nothing worse than being asked again next launch, and this must never be
        // able to take down a dictation hotkey.
        tracing::debug!(
            "crash banner: could not save state to {}: {e}",
            path.display()
        );
    }
}

/// Call once per real launch (see `startup::bring_up_app`, beside `nudge::start_session` and
/// `feedback_survey::start_session`) -- never on a Save & Restart hand-off, which is a graceful
/// restart of a process that never panicked, not "since the last run".
///
/// Always re-records the panic log's current size, opt-in or not, so turning "Enable local error
/// reports" on later never dredges up a crash from before it was switched on -- only ones from
/// here forward ever produce an [`Ask`].
pub(crate) fn note_launch(error_reporting_enabled: bool) {
    let panic_path = crate::logging::panic_log_path();
    let current_len = std::fs::metadata(&panic_path).map(|m| m.len()).unwrap_or(0);

    let state_path = crate::paths::data_file(STATE_FILE);
    let previous = load(&state_path);
    persist(&state_path, current_len);

    let ask = decide(previous, current_len, error_reporting_enabled).then_some(Ask {
        headline: "QuickDictate ran into a problem last time",
        body: "A crash was recorded since your last session. Nothing is sent anywhere \u{2014} \
               review the redacted report and decide whether to save or share it yourself.",
    });
    if let Ok(mut guard) = PENDING.lock() {
        *guard = ask;
    }
}

/// The pending ask, if any, for the settings window to render. Read fresh every frame (not cached
/// per-window) so it reflects whatever the most recent launch decided -- see `update::pending_update`
/// for the identical pattern.
pub(crate) fn pending_ask() -> Option<Ask> {
    PENDING.lock().ok().and_then(|g| *g)
}

/// Clear the pending ask: called once the user opens the report or dismisses the banner, either
/// way the offer has been acted on for this launch.
pub(crate) fn dismiss() {
    if let Ok(mut guard) = PENDING.lock() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_baseline_never_asks_even_if_the_log_is_nonempty() {
        // A first-ever launch (or a state file this build has never written): `v == 0` means "no
        // baseline", and that must stay quiet no matter how large the panic log already is --
        // otherwise upgrading into this feature would surface every historical crash at once.
        let previous = CrashBannerState::default();
        assert!(!decide(previous, 4_096, true));
    }

    #[test]
    fn growth_past_a_real_baseline_asks_when_enabled() {
        let previous = CrashBannerState {
            v: STATE_VERSION,
            last_seen_panic_len: 100,
        };
        assert!(decide(previous, 250, true));
    }

    #[test]
    fn growth_past_a_real_baseline_stays_quiet_when_opted_out() {
        let previous = CrashBannerState {
            v: STATE_VERSION,
            last_seen_panic_len: 100,
        };
        assert!(!decide(previous, 250, false));
    }

    #[test]
    fn no_growth_never_asks() {
        let previous = CrashBannerState {
            v: STATE_VERSION,
            last_seen_panic_len: 100,
        };
        assert!(!decide(previous, 100, true));
        assert!(!decide(previous, 40, true)); // shrunk (e.g. legacy migration) -- still quiet
    }

    #[test]
    fn load_returns_default_for_a_missing_file() {
        let missing =
            std::env::temp_dir().join("qd-crash-banner-tests-missing-does-not-exist.json");
        assert_eq!(load(&missing), CrashBannerState::default());
    }

    #[test]
    fn persist_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "qd-crash-banner-tests-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(STATE_FILE);

        persist(&path, 12_345);
        let loaded = load(&path);
        assert!(loaded.is_plausible());
        assert_eq!(loaded.last_seen_panic_len, 12_345);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_treats_a_hand_mangled_file_as_no_baseline() {
        let dir = std::env::temp_dir().join(format!(
            "qd-crash-banner-tests-garbage-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("garbage.json");
        std::fs::write(&path, "{ not json").unwrap();

        assert_eq!(load(&path), CrashBannerState::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_ask_round_trips_through_the_static_and_clears_on_dismiss() {
        // Exercises `pending_ask`/`dismiss` directly rather than through `note_launch` (which
        // touches the real, process-wide data directory via `paths::data_file`) -- this still
        // proves the static plumbing the settings banner actually reads from.
        if let Ok(mut guard) = PENDING.lock() {
            *guard = Some(Ask {
                headline: "test",
                body: "test",
            });
        }
        assert!(pending_ask().is_some());
        dismiss();
        assert!(pending_ask().is_none());
    }
}
