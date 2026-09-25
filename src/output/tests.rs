//! Tests for the paste paths and the clipboard snapshot rules.

use super::*;
use unicode_segmentation::UnicodeSegmentation;

#[test]
fn unicode_input_preserves_non_bmp_characters_as_surrogate_pairs() {
    assert_eq!(
        unicode_code_units("A😀Z"),
        vec![0x0041, 0xD83D, 0xDE00, 0x005A]
    );
}

#[test]
fn snapshot_allowlist_keeps_valuable_formats_and_drops_exotic_ones() {
    // Standard formats a user notices losing.
    for fmt in [8u32, 13, 15, 16, 17] {
        assert!(should_snapshot_format(fmt), "format {fmt} should be kept");
    }
    // The delayed-render-prone exotica an Excel/browser copy advertises
    // must NOT be fetched: GetClipboardData on them forces the owning app
    // to synthesize data synchronously and froze normal pastes.
    for fmt in [2u32, 3, 129, 0xC000 + 7777] {
        assert!(
            !should_snapshot_format(fmt),
            "format {fmt} should be skipped"
        );
    }
    // The registered names resolve and are kept (HTML/RTF/PNG).
    for id in registered_snapshot_formats() {
        assert!(should_snapshot_format(*id));
    }
    assert_eq!(registered_snapshot_formats().len(), 3);
}

#[test]
fn hglobal_formats_exclude_the_handle_based_ones() {
    // CF_UNICODETEXT, CF_HDROP, CF_DIB and registered formats are memory
    // blocks we can copy.
    for fmt in [1u32, 13, 8, 17, 15, 0xC000, 0xC123] {
        assert!(is_hglobal_format(fmt), "format {fmt} should be HGLOBAL");
    }
    // GDI handles and owner-display are not.
    for fmt in [
        2u32, 3, 9, 14, 0x0080, 0x0083, 0x008E, 0x0300, 0x0350, 0x03FF,
    ] {
        assert!(!is_hglobal_format(fmt), "format {fmt} is handle-based");
    }
}

#[test]
fn undo_counts_grapheme_clusters_not_scalars() {
    // A ZWJ family emoji is one glyph but many Unicode scalars; counting
    // scalars would send extra backspaces into preceding text.
    let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    assert!(family.chars().count() > 1);
    assert_eq!(family.graphemes(true).count(), 1);
    assert_eq!("hi \u{1F44B}".graphemes(true).count(), 4);
}

#[test]
fn clipboard_text_is_nul_terminated_little_endian_utf16() {
    assert_eq!(unicode_clipboard_bytes(""), vec![0, 0]);
    assert_eq!(
        unicode_clipboard_bytes("A😀"),
        vec![0x41, 0x00, 0x3D, 0xD8, 0x00, 0xDE, 0x00, 0x00]
    );
}

fn target(window: isize, focus: isize, exe: &str) -> PasteTarget {
    PasteTarget {
        window,
        focus,
        exe: Some(exe.to_string()),
    }
}

#[test]
fn only_the_newest_paste_is_undoable() {
    let mut targets = UndoTargets::new();
    targets.push(1, target(0x10, 0x11, "outlook.exe"));
    targets.push(2, target(0x20, 0x21, "slack.exe"));
    // Entry 1 is behind entry 2: its length is no longer what backspaces
    // would remove.
    assert!(targets.target_for(1).is_none());
    assert_eq!(
        targets.target_for(2),
        Some(&target(0x20, 0x21, "slack.exe"))
    );
}

#[test]
fn a_second_scratch_that_checks_the_older_paste_target_not_the_undone_one() {
    // Outlook email, then "ok" in Slack. After undoing "ok", the next undo
    // must be judged against OUTLOOK, so saying it again while still in
    // Slack is refused instead of sending the email's length into Slack.
    let mut targets = UndoTargets::new();
    targets.push(1, target(0x10, 0x11, "outlook.exe"));
    targets.push(2, target(0x20, 0x21, "slack.exe"));
    targets.pop();
    let older = targets.target_for(1);
    assert_eq!(older, Some(&target(0x10, 0x11, "outlook.exe")));
    assert_ne!(older, Some(&target(0x20, 0x21, "slack.exe")));
}

#[test]
fn a_history_entry_without_a_recorded_paste_is_not_undoable() {
    // A failed paste still records history but no target; a restart keeps
    // history but not targets. Either way the newest entry has no target.
    let mut targets = UndoTargets::new();
    targets.push(1, target(0x10, 0x11, "editor.exe"));
    assert!(targets.target_for(2).is_none());
    targets.clear();
    assert!(targets.target_for(1).is_none());
}

#[test]
fn another_field_of_the_same_window_is_a_different_target() {
    assert_ne!(target(0x10, 0x11, "app.exe"), target(0x10, 0x12, "app.exe"));
    assert_ne!(
        target(0x10, 0x11, "app.exe"),
        target(0x10, 0x11, "other.exe")
    );
}

#[test]
fn the_undo_stack_is_capped_oldest_first() {
    let mut targets = UndoTargets::new();
    for id in 0..200u64 {
        targets.push(id, target(0x10, 0x11, "editor.exe"));
    }
    assert!(targets.target_for(199).is_some());
    // Pop back through everything that is kept; the oldest ones were dropped.
    let mut reachable = 0;
    for id in (0..200u64).rev() {
        if targets.target_for(id).is_none() {
            break;
        }
        reachable += 1;
        targets.pop();
    }
    assert_eq!(reachable, 50);
}

/// An app-compatibility entry that forces a delivery must beat the length
/// rule both ways: a console ignores Ctrl+V however long the text is, and a
/// window that drops keystrokes drops short text too.
#[test]
fn forced_delivery_overrides_the_length_threshold() {
    use crate::app_compat::Delivery;
    let long = CLIPBOARD_THRESHOLD + 10;
    assert!(!uses_clipboard(Delivery::Keystrokes, long));
    assert!(uses_clipboard(Delivery::Clipboard, 1));
    assert!(!uses_clipboard(Delivery::Auto, 1));
    assert!(uses_clipboard(Delivery::Auto, long));
}
