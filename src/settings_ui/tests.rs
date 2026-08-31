//! Tests for the Settings window's pure helpers.

use super::*;

#[test]
fn recorded_hotkeys_round_trip_through_the_parser() {
    // A bare F-key.
    let f14 = combo_from_event(egui::Key::F14, egui::Modifiers::default()).unwrap();
    assert_eq!(f14, "f14");
    assert!(crate::hotkeys::parse_combo(&f14).is_ok());

    // A modified letter.
    let mods = egui::Modifiers {
        ctrl: true,
        shift: true,
        ..Default::default()
    };
    let combo = combo_from_event(egui::Key::D, mods).unwrap();
    assert_eq!(combo, "ctrl+shift+d");
    assert!(crate::hotkeys::parse_combo(&combo).is_ok());

    // Keys the parser can't use are rejected up front.
    assert!(combo_from_event(egui::Key::F35, egui::Modifiers::default()).is_none());
}

#[test]
fn recorded_mouse_buttons_round_trip_through_the_parser() {
    // The bug this feature fixes: a mouse button pressed while a field was
    // recording produced nothing, because only `Key` events were read.
    // Every capturable button must now yield a combo the engine accepts.
    for (button, expected) in [
        (egui::PointerButton::Middle, "mouse3"),
        (egui::PointerButton::Extra1, "mouse4"),
        (egui::PointerButton::Extra2, "mouse5"),
    ] {
        let combo = combo_from_pointer(button, egui::Modifiers::default())
            .unwrap_or_else(|| panic!("{button:?} must be capturable"));
        assert_eq!(combo, expected);
        assert!(
            crate::hotkeys::parse_combo(&combo).is_ok(),
            "the capture UI and the hotkey engine must agree on {combo}"
        );
    }

    // Modifiers ride along exactly as they do for keys.
    let mods = egui::Modifiers {
        ctrl: true,
        shift: true,
        ..Default::default()
    };
    let combo = combo_from_pointer(egui::PointerButton::Extra1, mods).unwrap();
    assert_eq!(combo, "ctrl+shift+mouse4");
    assert!(crate::hotkeys::parse_combo(&combo).is_ok());
}

#[test]
fn left_and_right_click_are_never_captured() {
    // Two jobs at once: they are unsafe to bind (a bound button is a
    // suppressed button), and refusing Primary here is what stops the very
    // click that ARMS recording from being recorded as the binding.
    for button in [egui::PointerButton::Primary, egui::PointerButton::Secondary] {
        assert!(
            combo_from_pointer(button, egui::Modifiers::default()).is_none(),
            "{button:?} must not be capturable"
        );
        let mods = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert!(
            combo_from_pointer(button, mods).is_none(),
            "{button:?} must not become capturable just because a modifier is held"
        );
    }
}

#[test]
fn a_mouse_bound_toggle_and_hold_are_a_reported_conflict() {
    // Two identical mouse bindings are as unusable as two identical
    // keyboard ones, and must be caught by the same check.
    assert!(hotkeys_conflict("mouse4", "mouse4"));
    assert!(hotkeys_conflict("mouse4", "x1")); // same button, different spelling
    assert!(!hotkeys_conflict("mouse4", "mouse5"));
    assert!(!hotkeys_conflict("mouse4", "f14"));
}

#[test]
fn bulk_replacements_round_trip() {
    let rows = vec![
        ("Chat GPT".to_string(), "ChatGPT".to_string()),
        ("Github".to_string(), "GitHub".to_string()),
    ];
    assert_eq!(text_to_replacements(&replacements_to_text(&rows)), rows);

    // Lenient: `=` separator, blank lines and separator-less lines skipped.
    let parsed = text_to_replacements("a = b\n\n  c=d \nnosep");
    assert_eq!(
        parsed,
        vec![
            ("a".to_string(), "b".to_string()),
            ("c".to_string(), "d".to_string())
        ]
    );
}

#[test]
fn bulk_keys_trim_skip_blanks_and_stably_dedupe() {
    let mut rows = vec![KeyRow {
        value: "existing-key".into(),
        verdict: Verdict::Ok,
    }];
    let summary = merge_key_lines(
        &mut rows,
        " new-key-a \r\n\r\nexisting-key\nnew-key-b\nnew-key-a\n",
    )
    .unwrap();
    assert_eq!(
        summary,
        KeyMergeSummary {
            added: 2,
            duplicates: 2,
        }
    );
    assert_eq!(
        deduped_key_values(&rows),
        vec!["existing-key", "new-key-a", "new-key-b"]
    );
    assert_eq!(rows[0].verdict, Verdict::Ok);
    assert_eq!(rows[1].verdict, Verdict::Untested);
}

#[test]
fn bulk_key_validation_is_atomic_and_keys_are_case_sensitive() {
    let mut rows = vec![KeyRow {
        value: "KeyABC".into(),
        verdict: Verdict::Untested,
    }];
    let before = deduped_key_values(&rows);
    assert_eq!(
        merge_key_lines(&mut rows, "valid-key\nnot a key\nalso-valid"),
        Err(vec![2])
    );
    assert_eq!(deduped_key_values(&rows), before);

    let summary = merge_key_lines(&mut rows, "keyabc\nKeyABC").unwrap();
    assert_eq!(summary.added, 1);
    assert_eq!(summary.duplicates, 1);
    assert_eq!(deduped_key_values(&rows), vec!["KeyABC", "keyabc"]);
}

#[test]
fn key_mask_never_displays_the_whole_secret() {
    assert_eq!(mask("abcd"), "\u{2022}\u{2022}\u{2022}\u{2022}");
    assert_eq!(mask("abcdef"), "ab\u{2026}ef");
    assert_eq!(mask("abcdefghijklmnop"), "abcdef\u{2026}klmnop");
}

#[test]
fn stats_numbers_are_human_readable() {
    assert_eq!(grouped_number(0), "0");
    assert_eq!(grouped_number(1_234_567), "1,234,567");
    assert_eq!(format_audio_time(42_900), "42s");
    assert_eq!(format_audio_time(125_000), "2m 5s");
    assert_eq!(format_audio_time(7_500_000), "2h 5m");
}

// ---- Hotkey-conflict validation (change 4) -----------------------------

#[test]
fn hotkeys_conflict_flags_identical_combos_case_insensitively() {
    assert!(hotkeys_conflict("ctrl+shift+d", "Ctrl+Shift+D"));
    // Order of modifiers shouldn't matter either -- both normalize to the
    // same (modifiers, vk) pair.
    assert!(hotkeys_conflict("ctrl+shift+d", "shift+ctrl+d"));
    assert!(!hotkeys_conflict("f13", "f14"));
}

#[test]
fn hotkeys_conflict_ignores_unparsable_combos() {
    // `validate` already surfaces the parse error for these on its own;
    // this helper only ever runs once both sides are known-good.
    assert!(!hotkeys_conflict("not a key", "f13"));
    assert!(!hotkeys_conflict("f13", "also not a key"));
}

// ---- Unsaved-changes dirty check (change 3) ----------------------------

#[test]
fn configs_differ_detects_a_changed_field_and_is_false_for_equal_configs() {
    let a = Config::default();
    let b = Config::default();
    assert!(!configs_differ(&a, &b));

    let mut c = Config::default();
    c.auto_punct = !c.auto_punct;
    assert!(configs_differ(&a, &c));
}

// ---- Custom vocabulary editor parsing (change 6) -----------------------

#[test]
fn parse_vocabulary_trims_and_drops_blank_lines() {
    let parsed = parse_vocabulary("  Supabase  \n\n\tCloudflare\n   \nGitHub\n");
    assert_eq!(
        parsed,
        vec![
            "Supabase".to_string(),
            "Cloudflare".to_string(),
            "GitHub".to_string(),
        ]
    );
    assert!(parse_vocabulary("\n   \n\t\n").is_empty());
}

// ---- History search filter (change 7) ----------------------------------

#[test]
fn history_matches_is_case_insensitive_and_empty_filter_matches_everything() {
    assert!(history_matches("", "anything at all"));
    assert!(history_matches("   ", "anything at all"));
    assert!(history_matches("hello", "Well, HELLO there"));
    assert!(history_matches("  Hello  ", "hello world")); // filter itself is trimmed
    assert!(!history_matches("zzz", "Well, HELLO there"));
}

#[test]
fn truncate_preview_keeps_short_text_and_ellipsizes_long_text() {
    assert_eq!(truncate_preview("short", 10), "short");
    assert_eq!(truncate_preview("exactly ten", 11), "exactly ten");
    assert_eq!(truncate_preview("this is too long", 7), "this is\u{2026}");
}

// ---- History card cache invalidation (adversarial-review fix 3) -------

#[test]
fn history_cache_stale_is_false_when_neither_version_nor_filter_moved() {
    assert!(!history_cache_stale(3, 3, "hello", "hello"));
    assert!(!history_cache_stale(0, 0, "", ""));
}

#[test]
fn history_cache_stale_detects_a_version_bump_with_the_filter_unchanged() {
    assert!(history_cache_stale(3, 4, "hello", "hello"));
}

#[test]
fn history_cache_stale_detects_a_filter_edit_with_the_version_unchanged() {
    assert!(history_cache_stale(3, 3, "hello", "hell"));
}

#[test]
fn history_cache_stale_detects_both_moving_at_once() {
    assert!(history_cache_stale(3, 5, "hello", "world"));
}
