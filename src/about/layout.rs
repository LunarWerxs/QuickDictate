//! Where every control sits, and the pure arithmetic that places it.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::theme::{BTN_FACE, DARK_TEXT};

use super::*;

// ---- Pure layout helpers (extracted from build_about / draw_*_pill) -----

/// The version-pill label, e.g. "v1.2.3" (from the crate's own Cargo.toml).
pub(super) fn version_label() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

/// Centered top-left offset for placing `content` inside a `container` span
/// that starts at 0 (add the span's own origin to get the final coordinate).
/// Integer division truncates toward zero, same as the inline arithmetic this
/// replaces.
pub(super) fn center_offset(container: i32, content: i32) -> i32 {
    (container - content) / 2
}

/// Width that keeps `aspect` (width / height) when scaled to `height`,
/// rounded to the nearest pixel.
pub(super) fn aspect_width(height: i32, aspect: f32) -> i32 {
    (height as f32 * aspect).round() as i32
}

// ---- Layout --------------------------------------------------------------

pub(super) unsafe fn build_about(hwnd: HWND) {
    // The GitHub mark, built first (composited on the resting pill face) so
    // the very first pill paint already has it.
    let icon_px = s(hwnd, ICON).max(1) as u32;
    let icon = github_icon_hbitmap(icon_px, BTN_FACE(), DARK_TEXT());
    let st = about_state(hwnd);
    if !st.is_null() {
        (*st).gh_icon = icon;
    }

    // Product logo, centered near the top (same disc as the tray/exe icon).
    let logo = ctl(hwnd, "", SS_BITMAP, (CW - 72) / 2, 20, 72, 72, -1);
    let logo_px = s(hwnd, 72).max(1) as u32;
    if let Some(hbmp) = rgba_to_hbitmap(logo_px, logo_px, &qd_logo_rgba(logo_px)) {
        set_static_bitmap(logo, hbmp);
    }

    // Product title — big + bold — then the muted subtitle.
    let title = ctl(hwnd, "QuickDictate", SS_CENTER, 20, 100, CW - 40, 34, -1);
    SendMessageW(
        title,
        WM_SETFONT,
        WPARAM(gui_font_sized(hwnd, 26, 700).0 as usize),
        LPARAM(1),
    );
    ctl(
        hwnd,
        "Bring-your-own-key dictation for Windows",
        SS_CENTER,
        20,
        138,
        CW - 40,
        18,
        ID_SUBTITLE,
    );

    // The two status pills, centered as a group. Each pill's width is fixed
    // (the version is constant; the status pill is sized to its widest
    // possible text), so the owner-draw just centers content inside.
    let ver = version_label();
    let ver_w = 14 + ICON + 7 + text_width(&ver) + 14;
    let cand = [
        "Checking\u{2026}".to_string(),
        "Updating\u{2026}".to_string(),
        "Up to date".to_string(),
        "Check failed".to_string(),
        "Update to 99.99.99".to_string(),
    ];
    let max_tw = cand.iter().map(|c| text_width(c)).max().unwrap_or(80);
    let status_w = 14 + 10 + 8 + max_tw + 14;
    let gap = 12;
    let gx = center_offset(CW, ver_w + gap + status_w);
    let pill = SS_OWNERDRAW | SS_NOTIFY;
    let ver_pill = ctl(hwnd, "", pill, gx, 174, ver_w, 30, ID_VER_PILL);
    let status_pill = ctl(
        hwnd,
        "",
        pill,
        gx + ver_w + gap,
        174,
        status_w,
        30,
        ID_STATUS_PILL,
    );

    // Bottom-left: license + copyright (muted via WM_CTLCOLORSTATIC).
    ctl(hwnd, "MIT License", 0, 22, 250, 210, 16, ID_LICENSE);
    ctl(
        hwnd,
        "\u{00a9} 2026 Lunarwerx",
        0,
        22,
        268,
        210,
        16,
        ID_COPYRIGHT,
    );

    // Bottom-right: the clickable LunarWerx Studios wordmark. The two theme
    // variants have different aspect ratios, so size the control to the active
    // one (fixed height, width from the aspect) and right-anchor it.
    let (_, lw_aspect) = lw_logo();
    let lw_h = 26;
    let lw_w = aspect_width(lw_h, lw_aspect);
    let lw = ctl(
        hwnd,
        "",
        SS_BITMAP | SS_NOTIFY,
        CW - 22 - lw_w,
        252,
        lw_w,
        lw_h,
        ID_LW_LOGO,
    );
    let (lw_pw, lw_ph) = (s(hwnd, lw_w).max(1) as u32, s(hwnd, lw_h).max(1) as u32);
    if let Some(hbmp) = lw_logo_hbitmap(lw_pw, lw_ph) {
        set_static_bitmap(lw, hbmp);
    }

    if !st.is_null() {
        (*st).ver_pill = ver_pill.0 as isize;
        (*st).status_pill = status_pill.0 as isize;
        (*st).lw_logo = lw.0 as isize;
    }
}
