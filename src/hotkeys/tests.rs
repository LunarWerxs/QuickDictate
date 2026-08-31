//! Tests for combo parsing, the re-arm streak, and the forbidden buttons.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};

use crate::mouse_hook::{is_mouse_vk, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2};

use super::combo::vk_for;
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
