//! Tests for the pip's poll cadence and label helpers.

use super::*;
use crate::state::ErrorKind;

#[test]
fn error_glyph_covers_every_named_variant() {
    // One deliberate call per variant rather than a loop over `0..=6` --
    // this is the exhaustive `match` in `error_glyph` doing the real
    // work; the point of the test is to lock each cause to its glyph, not
    // to re-derive the mapping.
    assert_eq!(error_glyph(ErrorKind::Generic), ("!", false));
    assert_eq!(error_glyph(ErrorKind::DeadKeys), ("\u{E8D7}", true));
    assert_eq!(error_glyph(ErrorKind::Quota), ("$", false));
    assert_eq!(error_glyph(ErrorKind::RateLimited), ("429", false));
    assert_eq!(error_glyph(ErrorKind::Network), ("net", false));
    assert_eq!(error_glyph(ErrorKind::Elevated), ("UAC", false));
    assert_eq!(error_glyph(ErrorKind::HotkeyBlocked), ("hk", false));
}

#[test]
fn error_glyph_labels_are_all_distinguishable() {
    // The bug this replaces: every kind but DeadKeys collapsed to the
    // same bare "!". Guard against a future edit reintroducing a
    // duplicate by asserting every label is unique.
    let kinds = [
        ErrorKind::Generic,
        ErrorKind::DeadKeys,
        ErrorKind::Quota,
        ErrorKind::RateLimited,
        ErrorKind::Network,
        ErrorKind::Elevated,
        ErrorKind::HotkeyBlocked,
    ];
    let mut labels: Vec<&str> = kinds.iter().map(|k| error_glyph(*k).0).collect();
    let before = labels.len();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), before, "two ErrorKind variants share a glyph");
}

#[test]
fn only_dead_keys_uses_the_icon_font() {
    assert!(error_glyph(ErrorKind::DeadKeys).1);
    for kind in [
        ErrorKind::Generic,
        ErrorKind::Quota,
        ErrorKind::RateLimited,
        ErrorKind::Network,
        ErrorKind::Elevated,
        ErrorKind::HotkeyBlocked,
    ] {
        assert!(
            !error_glyph(kind).1,
            "{kind:?} should use the plain UI font"
        );
    }
}

#[test]
fn poll_interval_is_fast_only_while_active() {
    assert_eq!(poll_interval(true), ACTIVE_POLL_INTERVAL);
    assert_eq!(poll_interval(false), IDLE_POLL_INTERVAL);
    assert!(
        IDLE_POLL_INTERVAL > ACTIVE_POLL_INTERVAL,
        "idle sleep must actually be the long one"
    );
}
