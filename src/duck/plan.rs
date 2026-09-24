//! The pure decisions behind ducking -- what an app is set to, and whether it
//! is still ours to put back -- plus the leftovers file that outlives a crash.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// With fading on (`Config::duck_fade`), how long other apps take to glide
/// down when a press starts. Short on purpose: the music has to be out of the
/// way before the first word lands.
pub(super) const FADE_DOWN: Duration = Duration::from_millis(250);
/// ...and how long they take to swell back once the microphone stops. Longer,
/// so the music eases back in rather than jumping on the last word.
pub(super) const FADE_UP: Duration = Duration::from_millis(600);
/// One volume step per this much time during a fade: about 50 a second, far
/// finer than an ear can pick apart as steps.
pub(super) const FADE_STEP: Duration = Duration::from_millis(20);

/// The volume `t` of the way through a fade from `from` to `to` (`t` in
/// 0..=1, clamped). A smoothstep curve, so the change eases in and out
/// instead of lurching at either end.
pub(super) fn fade_level(from: f32, to: f32, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let eased = t * t * (3.0 - 2.0 * t);
    from + (to - from) * eased
}

/// How many steps a fade of `over` takes: never zero, so even a zero-length
/// fade lands on its target.
pub(super) fn fade_steps(over: Duration) -> u32 {
    let steps = over.as_millis() / FADE_STEP.as_millis().max(1);
    u32::try_from(steps).unwrap_or(u32::MAX).max(1)
}

/// How close a read-back volume must be to the one we set to count as
/// "still as we left it". The audio service stores the level as a float that
/// has been through a round trip, so `==` is the wrong test; one percent is
/// far below any change a person makes by dragging a slider.
pub(super) const SAME_LEVEL: f32 = 0.01;

/// The file, in the data folder, listing every app ducked and not yet put
/// back. Present only while a press is ducking, or when a previous run left
/// something down.
pub(crate) const LEFTOVERS_FILE: &str = "quickdictate-ducked-apps.json";
const LEFTOVERS_VERSION: u32 = 1;
/// A leftover older than this is dropped unrestored. By then the user has
/// long since set that app's volume the way they want it, and unmuting it a
/// week later would undo their choice rather than ours.
pub(super) const LEFTOVER_MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// One app's volume, as found or as left.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Level {
    /// The per-app slider, 0.0 to 1.0.
    pub(super) volume: f32,
    pub(super) muted: bool,
}

/// One change to an app's volume.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Change {
    Mute,
    Unmute,
    Volume(f32),
}

impl Level {
    /// What the app reads after `change`.
    pub(super) fn after(self, change: Change) -> Level {
        match change {
            Change::Mute => Level {
                muted: true,
                ..self
            },
            Change::Unmute => Level {
                muted: false,
                ..self
            },
            Change::Volume(volume) => Level { volume, ..self },
        }
    }
}

/// What ducking does to an app found at `found`, or `None` to leave it be.
///
/// `percent` is how loud it may stay, as a share of its own volume. `0` mutes
/// rather than dragging the slider to zero: the slider keeps its place, and an
/// app that stays muted after a hard exit is one click on the mixer's speaker
/// icon to fix. An app that is already muted, or so quiet that lowering it
/// would change nothing audible, is left alone, which also means restoring
/// never touches it.
pub(super) fn duck(found: Level, percent: u8) -> Option<Change> {
    if found.muted || percent >= 100 {
        return None;
    }
    if percent == 0 {
        return Some(Change::Mute);
    }
    let target = found.volume * f32::from(percent) / 100.0;
    (found.volume - target >= SAME_LEVEL).then_some(Change::Volume(target))
}

/// What puts an app back, given how it was found (`original`), how ducking
/// left it (`set`) and how it reads now; `None` when there is nothing to undo.
///
/// Only a change of ours that is still in place is undone. If the user
/// unmuted the app, or moved its slider, while they were dictating, that is
/// their decision and it stands.
pub(super) fn restore(original: Level, set: Level, now: Level) -> Option<Change> {
    if set.muted && !original.muted {
        return now.muted.then_some(Change::Unmute);
    }
    let lowered = (original.volume - set.volume).abs() >= SAME_LEVEL;
    let still_lowered = (now.volume - set.volume).abs() < SAME_LEVEL;
    (lowered && still_lowered).then_some(Change::Volume(original.volume))
}

/// The app a session identifier names, for the log: the file name of its
/// executable. An identifier looks like
/// `{0.0.0.00000000}.{device}|\Device\HarddiskVolume3\...\Spotify.exe%b{grouping}`;
/// anything else is returned whole rather than guessed at.
pub(super) fn app_name(session: &str) -> &str {
    let Some((_, path)) = session.split_once('|') else {
        return session;
    };
    let path = path.split("%b").next().unwrap_or(path);
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// One app ducked and not yet put back, as written to [`LEFTOVERS_FILE`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Leftover {
    /// The app's session identifier. Unlike the instance identifier it stays
    /// the same across launches: it is the key Windows stores the app's
    /// volume under, so it still finds the app after either side restarts.
    pub(super) session: String,
    pub(super) original: Level,
    pub(super) set: Level,
    /// When it was ducked, in ms since the Unix epoch.
    pub(super) at_ms: u64,
}

#[derive(Deserialize)]
struct LeftoversIn {
    version: u32,
    #[serde(default)]
    apps: Vec<Leftover>,
}

#[derive(Serialize)]
struct LeftoversOut<'a> {
    version: u32,
    apps: &'a [Leftover],
}

/// The leftovers in a file's text, minus any too old to act on. An
/// unreadable file, or one from a newer version, reads as empty: the worst
/// that costs is an app staying quiet, never a wrong change.
pub(super) fn parse_leftovers(text: &str, now_ms: u64) -> Vec<Leftover> {
    match serde_json::from_str::<LeftoversIn>(text) {
        Ok(file) if file.version == LEFTOVERS_VERSION => file
            .apps
            .into_iter()
            .filter(|left| {
                !left.session.is_empty() && now_ms.saturating_sub(left.at_ms) <= LEFTOVER_MAX_AGE_MS
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The file text for `apps`.
pub(super) fn leftovers_json(apps: &[Leftover]) -> String {
    serde_json::to_string_pretty(&LeftoversOut {
        version: LEFTOVERS_VERSION,
        apps,
    })
    .unwrap_or_default()
}

/// Add `entry`, replacing any older record of the same app on the same
/// device: the newest duck is the one that describes how it was found.
pub(super) fn upsert(list: &mut Vec<Leftover>, entry: Leftover) {
    list.retain(|left| left.session != entry.session);
    list.push(entry);
}
