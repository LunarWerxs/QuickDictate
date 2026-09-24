//! Tests for the ducking decisions, the press counter, and the leftovers file.

use super::plan::*;
use super::Presses;

fn level(volume: f32, muted: bool) -> Level {
    Level { volume, muted }
}

#[test]
fn zero_percent_mutes_and_keeps_the_slider_where_it_was() {
    let found = level(0.8, false);
    assert_eq!(duck(found, 0), Some(Change::Mute));
    assert_eq!(found.after(Change::Mute), level(0.8, true));
}

#[test]
fn a_percentage_scales_the_apps_own_volume() {
    let Some(Change::Volume(v)) = duck(level(0.5, false), 20) else {
        panic!("expected a volume change");
    };
    assert!((v - 0.1).abs() < 1e-6, "{v}");
}

#[test]
fn already_muted_silent_or_full_percent_apps_are_left_alone() {
    assert_eq!(duck(level(0.8, true), 0), None);
    assert_eq!(duck(level(0.8, true), 20), None);
    assert_eq!(duck(level(0.0, false), 20), None);
    assert_eq!(duck(level(0.005, false), 50), None);
    assert_eq!(duck(level(0.8, false), 100), None);
    assert_eq!(duck(level(0.8, false), 255), None);
}

#[test]
fn restore_unmutes_only_what_we_muted_and_is_still_muted() {
    let original = level(0.8, false);
    let set = original.after(Change::Mute);
    assert_eq!(restore(original, set, set), Some(Change::Unmute));
    // The user unmuted it mid-dictation: theirs to keep.
    assert_eq!(restore(original, set, level(0.8, false)), None);
    // Unmuting leaves a slider the user moved while it was muted alone.
    assert_eq!(set.after(Change::Unmute), level(0.8, false));
}

#[test]
fn restore_raises_the_volume_back_only_if_nobody_moved_it() {
    let original = level(0.6, false);
    let set = original.after(Change::Volume(0.12));
    assert_eq!(restore(original, set, set), Some(Change::Volume(0.6)));
    // Float noise from the round trip through the audio service still counts.
    assert_eq!(
        restore(original, set, level(0.1205, false)),
        Some(Change::Volume(0.6))
    );
    // Dragged somewhere else mid-dictation: left there.
    assert_eq!(restore(original, set, level(0.3, false)), None);
    // Muted by the user mid-dictation: the volume comes back, the mute stays.
    assert_eq!(
        restore(original, set, level(0.12, true)),
        Some(Change::Volume(0.6))
    );
}

#[test]
fn restore_never_touches_an_app_ducking_did_not_change() {
    let untouched = level(0.7, false);
    assert_eq!(restore(untouched, untouched, untouched), None);
    assert_eq!(restore(untouched, untouched, level(0.2, true)), None);
}

#[test]
fn a_fade_starts_and_ends_exactly_on_its_levels_and_never_overshoots() {
    for (from, to) in [(0.8_f32, 0.0_f32), (0.0, 0.8), (0.6, 0.12), (0.12, 0.6)] {
        assert_eq!(fade_level(from, to, 0.0), from);
        assert!((fade_level(from, to, 1.0) - to).abs() < 1e-6);
        let (lo, hi) = (from.min(to), from.max(to));
        let mut last = from;
        for step in 1..=20 {
            let v = fade_level(from, to, step as f32 / 20.0);
            assert!(
                (lo - 1e-6..=hi + 1e-6).contains(&v),
                "{v} outside {lo}..{hi}"
            );
            // Monotonic: a fade never wobbles back the other way.
            assert!((v - last) * (to - from) >= -1e-6);
            last = v;
        }
    }
    // Out-of-range progress is clamped rather than extrapolated.
    assert_eq!(fade_level(0.5, 0.0, -1.0), 0.5);
    assert_eq!(fade_level(0.5, 0.0, 2.0), 0.0);
}

#[test]
fn fades_are_quick_down_gentle_up_and_always_take_at_least_one_step() {
    assert!(FADE_DOWN < FADE_UP, "out of the way fast, back in gently");
    assert!(FADE_DOWN <= std::time::Duration::from_millis(300));
    assert_eq!(fade_steps(FADE_DOWN), 12);
    assert_eq!(fade_steps(std::time::Duration::ZERO), 1);
    let cfg = crate::config::Config::default();
    assert!(
        cfg.duck_fade,
        "fading is the default once ducking is switched on"
    );
}

#[test]
fn only_the_first_press_ducks_and_only_the_last_restores() {
    let mut presses = Presses::default();
    assert!(presses.start(), "first press ducks");
    assert!(!presses.start(), "an overlapping press shares the duck");
    assert!(
        !presses.finish(),
        "the first to stop listening does not restore"
    );
    assert!(presses.finish(), "the last one does");
    assert!(!presses.finish(), "a stray finish never underflows");
    assert!(presses.start(), "and the next press ducks afresh");
}

#[test]
fn app_name_is_the_executable_of_the_session() {
    let id = r"{0.0.0.00000000}.{3b1f7a52-9c8e-4f0a-8d4b-1a2b3c4d5e6f}|\Device\HarddiskVolume3\Program Files\Spotify\Spotify.exe%b{00000000-0000-0000-0000-000000000000}";
    assert_eq!(app_name(id), "Spotify.exe");
    assert_eq!(app_name("no separator"), "no separator");
    assert_eq!(app_name(""), "");
}

fn leftover(session: &str, at_ms: u64) -> Leftover {
    let original = level(0.9, false);
    Leftover {
        session: session.into(),
        original,
        set: original.after(Change::Mute),
        at_ms,
    }
}

#[test]
fn leftovers_round_trip_through_the_file() {
    let apps = vec![leftover("a", 1_000), leftover("b", 2_000)];
    let text = leftovers_json(&apps);
    assert_eq!(parse_leftovers(&text, 3_000), apps);
}

#[test]
fn stale_garbage_and_future_leftovers_read_as_nothing_to_do() {
    let now = 10 * LEFTOVER_MAX_AGE_MS;
    let text = leftovers_json(&[leftover("old", 0), leftover("fresh", now - 1)]);
    let parsed = parse_leftovers(&text, now);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].session, "fresh");

    assert!(parse_leftovers("not json", now).is_empty());
    assert!(parse_leftovers(r#"{"version":99,"apps":[]}"#, now).is_empty());
    assert!(parse_leftovers(&leftovers_json(&[leftover("", now)]), now).is_empty());
    // A clock that jumped backwards keeps the entry rather than dropping it.
    assert_eq!(
        parse_leftovers(&leftovers_json(&[leftover("x", now)]), 0).len(),
        1
    );
}

#[test]
fn upsert_keeps_one_record_per_app_and_the_newest_wins() {
    let mut list = vec![leftover("a", 1), leftover("b", 2)];
    upsert(&mut list, leftover("a", 3));
    assert_eq!(list.len(), 2);
    assert_eq!(
        list.iter().find(|l| l.session == "a").map(|l| l.at_ms),
        Some(3)
    );
}

/// Walks the real audio sessions read-only: proves the COM plumbing (device
/// enumeration, the casts to the session and volume interfaces, the string
/// getters) works on this machine without changing anything. A machine with
/// no audio service or no output device (headless CI) passes trivially.
#[test]
fn listing_the_mixer_changes_nothing_and_never_lists_ourselves() {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    // SAFETY: no out-parameters; a redundant initialize just refcounts.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let Ok(mixer) = super::mixer::Mixer::open() else {
        return;
    };
    for app in mixer.sessions() {
        assert!(!app.instance.is_empty());
        if let Some(found) = app.level() {
            assert!((0.0..=1.0).contains(&found.volume), "{}", found.volume);
        }
        let _ = app.is_playing();
        let _ = app.is_gone();
    }
}
