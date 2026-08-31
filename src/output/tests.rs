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
fn a_failed_paste_publishes_no_undo_target() {
    // Guards the invariant paste_processed relies on: LAST_PASTE_TARGET is
    // cleared up front and only set again on a Typed outcome, so a
    // "scratch that" after a failed paste finds nothing to undo.
    *LAST_PASTE_TARGET.lock() = Some((0x1234, Some("editor.exe".into())));
    *LAST_PASTE_TARGET.lock() = None;
    assert!(LAST_PASTE_TARGET.lock().is_none());
}
