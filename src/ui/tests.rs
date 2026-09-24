//! Tests for the pip's poll cadence, disc painting and label helpers.

use super::*;
use crate::state::{ErrorKind, Status};

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

#[test]
fn disc_color_draws_nothing_while_idle_and_shares_blue_for_both_busy_states() {
    assert_eq!(disc_color(Status::Idle), None);
    assert_eq!(disc_color(Status::Error), Some((0xEF, 0x44, 0x44)));
    assert_eq!(
        disc_color(Status::Processing),
        disc_color(Status::Finalizing)
    );
    assert_ne!(disc_color(Status::Starting), disc_color(Status::Listening));
}

#[test]
fn fill_disc_is_opaque_inside_clear_outside_and_premultiplied_at_the_rim() {
    let size = PIP_SIZE;
    let mut pixels = vec![0u32; (size * size) as usize];
    let c = (size as f32 - 1.0) / 2.0;
    fill_disc(&mut pixels, size, c, c, (0x22, 0xC5, 0x5E));

    let center = (size / 2 * size + size / 2) as usize;
    assert_eq!(
        pixels[center], 0xFF22_C55E,
        "interior is the color, fully opaque"
    );
    assert_eq!(pixels[0], 0, "the corner lies outside the disc");
    for px in pixels {
        let alpha = px >> 24;
        for shift in [0, 8, 16] {
            assert!(
                (px >> shift) & 0xFF <= alpha,
                "{px:#010x} is not premultiplied"
            );
        }
    }
}

#[test]
fn pip_label_shows_the_count_except_in_error() {
    assert_eq!(
        pip_label(Status::Listening, ErrorKind::Network, 12),
        ("12".to_string(), false)
    );
    assert_eq!(
        pip_label(Status::Error, ErrorKind::Network, 12),
        ("net".to_string(), false)
    );
    assert!(pip_label(Status::Error, ErrorKind::DeadKeys, 0).1);
}
