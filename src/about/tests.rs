//! Tests for the About box's pure helpers.

use super::*;
use crate::theme::rgb;

// ---- scale_dpi ----

#[test]
fn scale_dpi_at_96_is_a_no_op() {
    assert_eq!(scale_dpi(96, 96), 96);
    assert_eq!(scale_dpi(440, 96), 440);
}

#[test]
fn scale_dpi_scales_up_at_common_dpi_steps() {
    // 120/144/192 dpi are the standard 125%/150%/200% Windows scale steps.
    assert_eq!(scale_dpi(10, 120), 13);
    assert_eq!(scale_dpi(10, 144), 15);
    assert_eq!(scale_dpi(10, 192), 20);
}

#[test]
fn scale_dpi_treats_zero_dpi_as_96() {
    assert_eq!(scale_dpi(200, 0), scale_dpi(200, 96));
}

// ---- wide ----

#[test]
fn wide_null_terminates_the_utf16_buffer() {
    assert_eq!(wide("ab"), vec!['a' as u16, 'b' as u16, 0]);
}

#[test]
fn wide_of_empty_string_is_only_the_terminator() {
    assert_eq!(wide(""), vec![0]);
}

// ---- colour channel extraction ----

#[test]
fn color_channels_round_trip_through_rgb() {
    let c = rgb(0x12, 0x34, 0x56);
    assert_eq!(color_r(c), 0x12);
    assert_eq!(color_g(c), 0x34);
    assert_eq!(color_b(c), 0x56);
}

#[test]
fn color_channels_mask_out_unrelated_high_bits() {
    // COLORREF only defines the low 24 bits; a stray high bit must not
    // leak into any channel.
    let c = COLORREF(0xFF12_3456);
    assert_eq!(color_r(c), 0x56);
    assert_eq!(color_g(c), 0x34);
    assert_eq!(color_b(c), 0x12);
}

// ---- premultiply ----

#[test]
fn premultiply_at_full_coverage_keeps_the_channel() {
    assert_eq!(premultiply(255, 255), 255);
}

#[test]
fn premultiply_at_zero_coverage_zeroes_the_channel() {
    assert_eq!(premultiply(255, 0), 0);
}

#[test]
fn premultiply_scales_by_the_coverage_fraction() {
    // (200 * 128 + 127) / 255 == 100 (integer division).
    assert_eq!(premultiply(200, 128), 100);
}

// ---- blend ----

#[test]
fn blend_at_zero_alpha_is_pure_background() {
    assert_eq!(blend(10, 200, 0), 10);
}

#[test]
fn blend_at_full_alpha_is_pure_foreground() {
    assert_eq!(blend(10, 200, 255), 200);
}

#[test]
fn blend_at_half_alpha_is_a_weighted_average() {
    // (200 * 128 + 10 * 127) / 255 == 105 (integer division).
    assert_eq!(blend(10, 200, 128), 105);
}

// ---- version_label ----

#[test]
fn version_label_prefixes_the_crate_version_with_v() {
    let label = version_label();
    assert!(label.starts_with('v'));
    assert_eq!(&label[1..], env!("CARGO_PKG_VERSION"));
}

// ---- center_offset ----

#[test]
fn center_offset_splits_the_remaining_space_evenly() {
    assert_eq!(center_offset(440, 200), 120);
    assert_eq!(center_offset(100, 100), 0);
}

#[test]
fn center_offset_truncates_an_odd_remainder_toward_zero() {
    // (11 - 4) / 2 == 3, not 4 — integer division, matching the inline
    // arithmetic this replaced.
    assert_eq!(center_offset(11, 4), 3);
}

// ---- aspect_width ----

#[test]
fn aspect_width_scales_by_the_ratio() {
    assert_eq!(aspect_width(10, 2.0), 20);
}

#[test]
fn aspect_width_rounds_to_the_nearest_pixel() {
    // 3 * 2.5 == 7.5, rounds away from zero to 8.
    assert_eq!(aspect_width(3, 2.5), 8);
}

// ---- status_label ----

#[test]
fn status_label_checking_shows_ellipsis() {
    let (_, text) = status_label(&Status::Checking);
    assert_eq!(text, "Checking\u{2026}");
}

#[test]
fn status_label_up_to_date_is_green() {
    let (color, text) = status_label(&Status::UpToDate);
    assert_eq!(text, "Up to date");
    assert_eq!(color, rgb(63, 185, 80));
}

#[test]
fn status_label_available_names_the_tag() {
    let (_, text) = status_label(&Status::Available("1.2.3".to_string()));
    assert_eq!(text, "Update to 1.2.3");
}

#[test]
fn status_label_updating_shows_ellipsis() {
    let (_, text) = status_label(&Status::Updating);
    assert_eq!(text, "Updating\u{2026}");
}

#[test]
fn status_label_failed_is_red() {
    let (color, text) = status_label(&Status::Failed);
    assert_eq!(text, "Check failed");
    assert_eq!(color, rgb(190, 110, 110));
}
