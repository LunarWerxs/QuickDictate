//! Tests for combo parsing, the re-arm streak, the forbidden buttons, and the
//! watch that catches a press Windows drops.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};

use crate::mouse_hook::{is_mouse_vk, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2};

use super::combo::vk_for;
use super::watch::{
    Dropped, KeyOutcome, Watch, DROPPED_AFTER, LATE_HOTKEY, LOST_UP_GAP, STALE_RELEASE_AFTER,
};
use super::*;

#[test]
fn parses_a_bare_function_key() {
    // No modifiers, but MOD_NOREPEAT is always set; f14 == VK 0x7D.
    assert_eq!(parse_combo("f14").unwrap(), (MOD_NOREPEAT.0, 0x7D));
}

#[test]
fn parses_modifiers_plus_a_letter() {
    let (mods, vk) = parse_combo("ctrl+shift+d").unwrap();
    assert_eq!(vk, 0x44); // 'd'
    assert_eq!(mods, MOD_CONTROL.0 | MOD_SHIFT.0 | MOD_NOREPEAT.0);
}

#[test]
fn parsing_ignores_case_and_surrounding_whitespace() {
    assert_eq!(
        parse_combo("  CTRL + Shift + D ").unwrap(),
        parse_combo("ctrl+shift+d").unwrap()
    );
}

#[test]
fn accepts_modifier_and_key_aliases() {
    // control==ctrl, menu==alt, del==delete
    assert_eq!(
        parse_combo("control+menu+del").unwrap(),
        (MOD_CONTROL.0 | MOD_ALT.0 | MOD_NOREPEAT.0, 0x2E)
    );
    // windows/super == win; return == enter
    assert_eq!(
        parse_combo("windows+return").unwrap(),
        (MOD_WIN.0 | MOD_NOREPEAT.0, 0x0D)
    );
    assert_eq!(
        parse_combo("super+esc").unwrap().0,
        MOD_WIN.0 | MOD_NOREPEAT.0
    );
}

#[test]
fn every_parsed_combo_sets_norepeat() {
    for combo in ["f13", "ctrl+a", "alt+shift+space"] {
        let (mods, _) = parse_combo(combo).unwrap();
        assert_ne!(mods & MOD_NOREPEAT.0, 0, "combo {combo} missing NOREPEAT");
    }
}

#[test]
fn parses_the_bindable_mouse_buttons() {
    // The whole point of the feature: these used to be unparsable, so a
    // mouse button could not be a hotkey even by hand-editing settings.json.
    assert_eq!(parse_combo("mouse3").unwrap(), (MOD_NOREPEAT.0, VK_MBUTTON));
    assert_eq!(
        parse_combo("mouse4").unwrap(),
        (MOD_NOREPEAT.0, VK_XBUTTON1)
    );
    assert_eq!(
        parse_combo("mouse5").unwrap(),
        (MOD_NOREPEAT.0, VK_XBUTTON2)
    );
    // Aliases land on the same VKs, so what a mouse vendor calls "Back"
    // and what Windows calls XBUTTON1 are the same binding.
    for alias in ["mouseback", "backmouse", "xbutton1", "x1"] {
        assert_eq!(parse_combo(alias).unwrap().1, VK_XBUTTON1, "alias {alias}");
    }
    for alias in ["mouseforward", "forwardmouse", "xbutton2", "x2"] {
        assert_eq!(parse_combo(alias).unwrap().1, VK_XBUTTON2, "alias {alias}");
    }
    for alias in [
        "middleclick",
        "middlemouse",
        "mousemiddle",
        "mmb",
        "mbutton",
    ] {
        assert_eq!(parse_combo(alias).unwrap().1, VK_MBUTTON, "alias {alias}");
    }
}

#[test]
fn mouse_buttons_take_modifiers_like_any_other_key() {
    let (mods, vk) = parse_combo("ctrl+shift+mouse4").unwrap();
    assert_eq!(vk, VK_XBUTTON1);
    assert_eq!(mods, MOD_CONTROL.0 | MOD_SHIFT.0 | MOD_NOREPEAT.0);
}

#[test]
fn left_and_right_click_are_refused_through_every_alias() {
    // Binding one of these would suppress it system-wide, including the
    // clicks needed to get back into Settings and undo it. The refusal is
    // keyed on the VK, so no alias is a back door.
    for name in [
        "mouse1",
        "leftclick",
        "leftmouse",
        "lmb",
        "lbutton",
        "mouse2",
        "rightclick",
        "rightmouse",
        "rmb",
        "rbutton",
    ] {
        let err = parse_combo(name)
            .expect_err("left/right click must never bind")
            .to_string();
        assert!(
            err.contains("can't be used as a hotkey"),
            "{name} should explain itself, got: {err}"
        );
    }
    // Including with modifiers, and inside a longer combo.
    assert!(parse_combo("ctrl+alt+mouse1").is_err());
}

#[test]
fn mouse_and_keyboard_bindings_are_told_apart() {
    // This split is what routes a binding to the hook instead of
    // RegisterHotKey; getting it wrong means silently registering nothing.
    assert!(is_mouse_vk(parse_combo("mouse4").unwrap().1));
    assert!(is_mouse_vk(parse_combo("ctrl+mouse3").unwrap().1));
    assert!(!is_mouse_vk(parse_combo("f14").unwrap().1));
    assert!(!is_mouse_vk(parse_combo("ctrl+shift+d").unwrap().1));
}

#[test]
fn a_mouse_button_still_conflicts_with_itself() {
    // Toggle and hold both on mouse4 must be caught by the settings-window
    // conflict check, exactly as two identical keyboard combos are.
    assert_eq!(parse_combo("mouse4").unwrap(), parse_combo("x1").unwrap());
    assert_ne!(
        parse_combo("mouse4").unwrap(),
        parse_combo("mouse5").unwrap()
    );
}

#[test]
fn rejects_malformed_combos() {
    assert!(parse_combo("").is_err()); // nothing
    assert!(parse_combo("ctrl").is_err()); // modifier only, no main key
    assert!(parse_combo("a+b").is_err()); // two non-modifier keys
    assert!(parse_combo("ctrl+notakey").is_err()); // unknown key name
    assert!(parse_combo("f25").is_err()); // outside the F-key table
}

#[test]
fn blocked_streak_needs_threshold_consecutive_failures() {
    let (streak, blocked) = step_blocked_streak(0, false);
    assert_eq!(streak, 1);
    assert!(
        !blocked,
        "a single failure should not trip the blocked flag"
    );

    let (streak, blocked) = step_blocked_streak(streak, false);
    assert_eq!(streak, BLOCKED_STREAK_THRESHOLD);
    assert!(blocked, "threshold consecutive failures should trip it");
}

#[test]
fn blocked_streak_resets_on_any_success() {
    let (streak, blocked) = step_blocked_streak(BLOCKED_STREAK_THRESHOLD, true);
    assert_eq!(streak, 0, "a successful re-arm must clear the streak");
    assert!(!blocked);

    // A success right after the very first failure also resets cleanly.
    let (streak, _) = step_blocked_streak(1, true);
    assert_eq!(streak, 0);
}

#[test]
fn blocked_streak_stays_blocked_past_threshold() {
    // Once blocked, continued failures keep it blocked (no wraparound)
    // until a success clears it.
    let (streak, blocked) = step_blocked_streak(BLOCKED_STREAK_THRESHOLD + 5, false);
    assert!(streak > BLOCKED_STREAK_THRESHOLD);
    assert!(blocked);
}

#[test]
fn vk_table_maps_the_known_keys() {
    // Locks the hand-written VK lookup table — a typo here would silently
    // register the wrong physical key. (vk_for expects lowercase input, as
    // parse_combo feeds it.)
    assert_eq!(vk_for("a"), Some(0x41));
    assert_eq!(vk_for("z"), Some(0x5A));
    assert_eq!(vk_for("0"), Some(0x30));
    assert_eq!(vk_for("9"), Some(0x39));
    assert_eq!(vk_for("f1"), Some(0x70));
    assert_eq!(vk_for("f12"), Some(0x7B));
    assert_eq!(vk_for("f13"), Some(0x7C));
    assert_eq!(vk_for("f24"), Some(0x87));
    assert_eq!(vk_for("space"), Some(0x20));
    assert_eq!(vk_for("enter"), Some(0x0D));
    assert_eq!(vk_for("up"), Some(0x26));
    assert_eq!(vk_for("numpad0"), Some(0x60));
    assert_eq!(vk_for("nope"), None);
    assert_eq!(vk_for("A"), None); // case-sensitive: expects lowercase
}

#[test]
fn every_function_key_maps_and_only_its_canonical_spelling_does() {
    // The F-key codes are computed, not tabled, so pin all 24 against the
    // Windows VK_F1..VK_F24 range and make sure no number-shaped spelling
    // that was never a key name slips through the parse.
    for n in 1..=24u32 {
        assert_eq!(vk_for(&format!("f{n}")), Some(0x6F + n), "f{n}");
    }
    for not_a_key in [
        "f0",
        "f25",
        "f01",
        "f+1",
        "f-1",
        "f1a",
        "fx",
        "f99999999999",
    ] {
        assert_eq!(vk_for(not_a_key), None, "{not_a_key}");
    }
}

#[test]
fn modifiers_are_order_independent_and_a_repeat_is_harmless() {
    assert_eq!(
        parse_combo("shift+ctrl+f5").unwrap(),
        parse_combo("ctrl+shift+f5").unwrap()
    );
    assert_eq!(
        parse_combo("ctrl+ctrl+f5").unwrap(),
        (MOD_CONTROL.0 | MOD_NOREPEAT.0, 0x74)
    );
}

const F14: u32 = 0x7D;
const F13: u32 = 0x7C;
const TOGGLE: i32 = 1;
const HOLD: i32 = 2;
/// Windows had the key up before the press: the normal case.
const UP: bool = false;
/// Windows still counted the key as down: the stuck key.
const STUCK: bool = true;

/// F14 on toggle and F13 on hold, both registered, as on the PC that lost F14.
fn watching() -> Watch {
    let mut w = Watch::new(
        &[
            (TOGGLE, MOD_NOREPEAT.0, F14, false),
            (HOLD, MOD_NOREPEAT.0, F13, true),
        ],
        LOST_UP_GAP,
    );
    w.set_registered(TOGGLE, true);
    w.set_registered(HOLD, true);
    w
}

fn ms(n: u64) -> std::time::Duration {
    std::time::Duration::from_millis(n)
}

fn armed() -> KeyOutcome {
    KeyOutcome {
        arm_timer: true,
        ..KeyOutcome::default()
    }
}

fn dropped(id: i32, vk: u32, hold: bool, handle: bool, still_down: bool) -> Dropped {
    Dropped {
        id,
        vk,
        hold,
        handle,
        still_down,
    }
}

#[test]
fn a_press_windows_answers_is_left_to_windows() {
    let mut w = watching();
    let t = std::time::Instant::now();
    assert_eq!(w.on_key(F14, true, 0, UP, t), armed());
    assert!(w.on_hotkey(TOGGLE, t + ms(2)));
    assert_eq!(
        w.on_key(F14, false, 0, UP, t + ms(80)),
        KeyOutcome::default()
    );
    assert_eq!(w.on_timer(t + ms(250)), (vec![], false));
}

#[test]
fn a_press_dropped_on_a_stuck_key_is_handled_once() {
    let mut w = watching();
    let t = std::time::Instant::now();
    w.on_key(F14, true, 0, STUCK, t);
    w.on_key(F14, false, 0, UP, t + ms(70));
    assert_eq!(
        w.on_timer(t + ms(250)),
        (vec![dropped(TOGGLE, F14, false, true, false)], false)
    );
    // Windows' own message for that press, arriving late, must not toggle
    // the dictation straight back off.
    assert!(!w.on_hotkey(TOGGLE, t + ms(400)));
    // The next press is a press again.
    let t2 = t + ms(3000);
    assert_eq!(w.on_key(F14, true, 0, UP, t2), armed());
    assert!(w.on_hotkey(TOGGLE, t2 + ms(1)));
}

#[test]
fn a_press_another_program_took_is_left_alone() {
    // Remote Desktop, a key remapper or a game swallowed the key, so Windows
    // never counted it as down and sent nothing, on purpose.
    let mut w = watching();
    let t = std::time::Instant::now();
    w.on_key(F14, true, 0, UP, t);
    let (drops, _) = w.on_timer(t + ms(250));
    assert_eq!(drops, vec![dropped(TOGGLE, F14, false, false, true)]);
    // Still cleared once the key is up, and owes no release.
    assert_eq!(
        w.on_key(F14, false, 0, UP, t + ms(300)),
        KeyOutcome {
            clear: Some(F14),
            ..KeyOutcome::default()
        }
    );
    // A slow hook chain's late WM_HOTKEY is then Windows answering after
    // all, and is acted on.
    assert!(w.on_hotkey(TOGGLE, t + ms(320)));
}

#[test]
fn a_hotkey_long_after_a_rescue_is_acted_on() {
    let mut w = watching();
    let t = std::time::Instant::now();
    w.on_key(F14, true, 0, STUCK, t);
    w.on_key(F14, false, 0, UP, t + ms(70));
    w.on_timer(t + ms(250));
    // The watch missed this press's key-down, but Windows saw it.
    assert!(w.on_hotkey(TOGGLE, t + ms(250) + LATE_HOTKEY + ms(1)));
}

#[test]
fn a_handled_hold_press_ends_and_clears_on_its_key_up() {
    let mut w = watching();
    let t = std::time::Instant::now();
    w.on_key(F13, true, 0, STUCK, t);
    assert_eq!(
        w.on_timer(t + ms(250)).0,
        vec![dropped(HOLD, F13, true, true, true)]
    );
    assert_eq!(
        w.on_key(F13, false, 0, UP, t + ms(4000)),
        KeyOutcome {
            arm_timer: false,
            release: Some(HOLD),
            clear: Some(F13),
        }
    );
    // Owed once, not on every later key-up.
    assert_eq!(
        w.on_key(F13, false, 0, UP, t + ms(4100)),
        KeyOutcome::default()
    );
}

#[test]
fn a_hold_left_alone_never_releases() {
    let mut w = watching();
    let t = std::time::Instant::now();
    w.on_key(F13, true, 0, UP, t);
    w.on_timer(t + ms(250));
    assert_eq!(
        w.on_key(F13, false, 0, UP, t + ms(900)),
        KeyOutcome {
            clear: Some(F13),
            ..KeyOutcome::default()
        }
    );
}

#[test]
fn a_lost_key_up_is_settled_by_the_next_press() {
    // The device sends key-downs with no key-ups: Windows and the watch both
    // think the key is still down.
    let mut w = watching();
    let t = std::time::Instant::now();
    w.on_key(F13, true, 0, STUCK, t);
    assert!(w.on_timer(t + ms(250)).0[0].still_down);
    // No key-up. The next press settles the hold dictation and clears the
    // key, rather than leaving both waiting for ever...
    let t2 = t + LOST_UP_GAP + ms(500);
    assert_eq!(
        w.on_key(F13, true, 0, STUCK, t2),
        KeyOutcome {
            arm_timer: true,
            release: Some(HOLD),
            clear: Some(F13),
        }
    );
    // ...and is itself a press.
    assert_eq!(w.on_timer(t2 + ms(250)).0.len(), 1);
}

#[test]
fn the_rearm_ends_a_hold_whose_key_up_the_hook_missed() {
    let mut w = watching();
    let t = std::time::Instant::now();
    w.on_key(F13, true, 0, STUCK, t);
    w.on_timer(t + ms(250));
    // Still held as far as Windows knows: nothing to do yet.
    assert_eq!(
        w.on_rearm(|_| true, t + STALE_RELEASE_AFTER),
        KeyOutcome::default()
    );
    // Too soon after the press, even if Windows says up.
    assert_eq!(w.on_rearm(|_| false, t + ms(500)), KeyOutcome::default());
    assert_eq!(
        w.on_rearm(|_| false, t + STALE_RELEASE_AFTER),
        KeyOutcome {
            arm_timer: false,
            release: Some(HOLD),
            clear: Some(F13),
        }
    );
    // Settled once.
    assert_eq!(
        w.on_rearm(|_| false, t + STALE_RELEASE_AFTER * 2),
        KeyOutcome::default()
    );
    assert_eq!(
        w.on_key(F13, false, 0, UP, t + ms(9000)),
        KeyOutcome::default()
    );
}

#[test]
fn a_dropped_toggle_held_down_is_cleared_but_not_released() {
    let mut w = watching();
    let t = std::time::Instant::now();
    w.on_key(F14, true, 0, STUCK, t);
    assert!(w.on_timer(t + ms(250)).0[0].still_down);
    assert_eq!(
        w.on_key(F14, false, 0, UP, t + ms(600)),
        KeyOutcome {
            arm_timer: false,
            release: None,
            clear: Some(F14),
        }
    );
}

#[test]
fn a_press_still_in_its_grace_keeps_the_timer_running() {
    let mut w = watching();
    let t = std::time::Instant::now();
    w.on_key(F14, true, 0, STUCK, t);
    assert_eq!(w.on_timer(t + DROPPED_AFTER - ms(1)), (vec![], true));
    assert_eq!(w.on_timer(t + DROPPED_AFTER).0.len(), 1);
}

#[test]
fn auto_repeat_is_not_a_new_press() {
    let mut w = watching();
    let t = std::time::Instant::now();
    w.on_key(F14, true, 0, UP, t);
    assert!(w.on_hotkey(TOGGLE, t + ms(1)));
    // The slowest keyboard delay, then the repeats. Windows counts the key
    // as down for every one of them.
    for n in [1000, 1033, 1066, 1100] {
        assert_eq!(
            w.on_key(F14, true, 0, STUCK, t + ms(n)),
            KeyOutcome::default()
        );
    }
    assert_eq!(w.on_timer(t + ms(1400)), (vec![], false));
}

#[test]
fn a_slow_filterkeys_repeat_is_not_a_new_press() {
    // FilterKeys set to repeat every 3 s widens the gap to match.
    let mut w = Watch::new(&[(TOGGLE, MOD_NOREPEAT.0, F14, false)], ms(3500));
    w.set_registered(TOGGLE, true);
    let t = std::time::Instant::now();
    w.on_key(F14, true, 0, UP, t);
    assert!(w.on_hotkey(TOGGLE, t + ms(1)));
    assert_eq!(
        w.on_key(F14, true, 0, STUCK, t + ms(3000)),
        KeyOutcome::default()
    );
}

#[test]
fn a_lost_key_up_cannot_wedge_the_key() {
    let mut w = watching();
    let t = std::time::Instant::now();
    w.on_key(F14, true, 0, UP, t);
    assert!(w.on_hotkey(TOGGLE, t + ms(1)));
    // No key-up ever arrives; the next press is still a press.
    assert_eq!(
        w.on_key(F14, true, 0, STUCK, t + LOST_UP_GAP + ms(1)),
        armed()
    );
}

#[test]
fn modifiers_must_match_the_combo_exactly() {
    const D: u32 = 0x44;
    let ctrl_shift = MOD_CONTROL.0 | MOD_SHIFT.0;
    let mut w = Watch::new(
        &[(TOGGLE, ctrl_shift | MOD_NOREPEAT.0, D, false)],
        LOST_UP_GAP,
    );
    w.set_registered(TOGGLE, true);
    let t = std::time::Instant::now();
    // D alone is typing, not the hotkey: nothing to wait for, so typing can
    // never be taken for a dropped press.
    assert_eq!(w.on_key(D, true, 0, UP, t), KeyOutcome::default());
    w.on_key(D, false, 0, UP, t + ms(50));
    assert_eq!(w.on_key(D, true, ctrl_shift, UP, t + ms(120)), armed());
    assert!(w.on_hotkey(TOGGLE, t + ms(121)));
    w.on_key(D, false, 0, UP, t + ms(200));
    // An extra Alt makes it a different combo, as it does for Windows.
    assert_eq!(
        w.on_key(D, true, ctrl_shift | MOD_ALT.0, UP, t + ms(310)),
        KeyOutcome::default()
    );
}

#[test]
fn a_binding_we_do_not_hold_is_not_watched() {
    let mut w = Watch::new(&[(TOGGLE, MOD_NOREPEAT.0, F14, false)], LOST_UP_GAP);
    let t = std::time::Instant::now();
    assert!(w.watches(F14));
    assert!(!w.watches(F13));
    assert_eq!(w.on_key(F14, true, 0, STUCK, t), KeyOutcome::default());
    assert_eq!(w.on_timer(t + ms(250)), (vec![], false));
    w.on_key(F14, false, 0, UP, t + ms(50));
    w.set_registered(TOGGLE, true);
    assert_eq!(w.on_key(F14, true, 0, UP, t + ms(2000)), armed());
}

#[test]
fn a_hotkey_the_watch_never_saw_is_acted_on() {
    let mut w = watching();
    assert!(w.on_hotkey(TOGGLE, std::time::Instant::now()));
    assert!(w.on_hotkey(42, std::time::Instant::now()));
}
