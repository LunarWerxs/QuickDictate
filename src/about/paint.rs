//! Owner-draw for the two status pills and the spinner.

use windows::Win32::Foundation::{COLORREF, HWND, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    Arc as GdiArc, BitBlt, CreateCompatibleDC, CreatePen, DeleteDC, DeleteObject, DrawTextW,
    Ellipse, FillRect, GetStockObject, GetTextExtentPoint32W, RoundRect, SelectObject, SetBkMode,
    SetDCBrushColor, SetDCPenColor, SetTextColor, DC_BRUSH, DC_PEN, DT_LEFT, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, HBITMAP, HBRUSH, HDC, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;

use crate::theme::{rgb, BORDER_STRONG, BTN_FACE, DARK_BG, DARK_TEXT};

use super::*;

// ---- Owner-draw ---------------------------------------------------------

/// Text extent of `text` in the HDC's currently-selected font.
unsafe fn measure(hdc: HDC, text: &str) -> i32 {
    let w = wide(text);
    let n = w.len().saturating_sub(1);
    let mut sz = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &w[..n], &mut sz);
    sz.cx
}

unsafe fn fill_rc(hdc: HDC, rc: &RECT, color: COLORREF) {
    SetDCBrushColor(hdc, color);
    FillRect(hdc, rc, HBRUSH(GetStockObject(DC_BRUSH).0));
}

/// Paint the rounded pill frame (face + hairline border) — full-stadium
/// rounding (ellipse == height).
unsafe fn pill_frame(hwnd: HWND, hdc: HDC, rc: &RECT) {
    SelectObject(hdc, GetStockObject(DC_BRUSH));
    SelectObject(hdc, GetStockObject(DC_PEN));
    SetDCBrushColor(hdc, BTN_FACE());
    SetDCPenColor(hdc, BORDER_STRONG());
    let h = rc.bottom - rc.top;
    let inset = s(hwnd, 1);
    let _ = RoundRect(
        hdc,
        rc.left,
        rc.top,
        rc.right - inset,
        rc.bottom - inset,
        h,
        h,
    );
}

/// Blit an opaque bitmap into `dst` at `(x,y)`, `w`×`h`.
pub(super) unsafe fn blit(dst: HDC, hbmp: HBITMAP, x: i32, y: i32, w: i32, h: i32) {
    let mdc = CreateCompatibleDC(dst);
    if mdc.is_invalid() {
        return;
    }
    let old = SelectObject(mdc, hbmp);
    let _ = BitBlt(dst, x, y, w, h, mdc, 0, 0, SRCCOPY);
    SelectObject(mdc, old);
    let _ = DeleteDC(mdc);
}

/// Draw text left-aligned + vertically centered starting at `left`.
unsafe fn draw_pill_text(hdc: HDC, text: &str, left: i32, rc: &RECT, color: COLORREF) {
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, color);
    let mut buf = wide(text);
    let n = buf.len().saturating_sub(1);
    let mut tr = RECT {
        left,
        top: rc.top,
        right: rc.right,
        bottom: rc.bottom,
    };
    DrawTextW(
        hdc,
        &mut buf[..n],
        &mut tr,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
}

pub(super) unsafe fn draw_ver_pill(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill_rc(hdc, &rc, DARK_BG());
    pill_frame(hwnd, hdc, &rc);

    let icon_px = s(hwnd, ICON);
    let gap = s(hwnd, 7);
    let ver = version_label();
    SelectObject(hdc, gui_font_for(hwnd));
    let tw = measure(hdc, &ver);
    let group = icon_px + gap + tw;
    let gx = rc.left + center_offset(rc.right - rc.left, group);
    let iy = rc.top + center_offset(rc.bottom - rc.top, icon_px);
    let st = about_state(hwnd);
    if !st.is_null() {
        if let Some(icon) = (*st).gh_icon {
            blit(hdc, icon, gx, iy, icon_px, icon_px);
        }
    }
    draw_pill_text(hdc, &ver, gx + icon_px + gap, &rc, DARK_TEXT());
}

/// Map a status to (dot colour, label). Pure — no pointer, no FFI — so the
/// wndproc-facing wrapper below can stay a thin null check over this.
pub(super) fn status_label(status: &Status) -> (COLORREF, String) {
    match status {
        Status::Checking => (rgb(150, 150, 150), "Checking\u{2026}".to_string()),
        Status::UpToDate => (rgb(63, 185, 80), "Up to date".to_string()),
        Status::Available(tag) => (rgb(210, 153, 34), format!("Update to {tag}")),
        // Spinner colour (the dot is replaced by the arc while updating).
        Status::Updating => (rgb(74, 144, 245), "Updating\u{2026}".to_string()),
        Status::Failed => (rgb(190, 110, 110), "Check failed".to_string()),
    }
}

/// Map the current status to (dot colour, label).
unsafe fn status_display(st: *mut About) -> (COLORREF, String) {
    if st.is_null() {
        return (rgb(150, 150, 150), "Checking\u{2026}".to_string());
    }
    status_label(&(*st).status)
}

/// Draw a rotating open arc (~270°) in the `dotd`×`dotd` box at `(x,y)`, using
/// the brand blue — the animated "Checking…" spinner. The gap in the ring
/// rotates with `angle`, which reads as motion.
unsafe fn draw_spinner(hwnd: HWND, hdc: HDC, x: i32, y: i32, dotd: i32, angle: i32) {
    let pen = CreatePen(PS_SOLID, s(hwnd, 2).max(1), rgb(74, 144, 245));
    if pen.is_invalid() {
        return;
    }
    let old = SelectObject(hdc, pen);
    let (cx, cy) = (x + dotd / 2, y + dotd / 2);
    let r = (dotd / 2) as f32;
    let a0 = (angle as f32).to_radians();
    let a1 = ((angle + 270) as f32).to_radians();
    // GDI Arc is drawn counter-clockwise from the (xStart,yStart) radial to the
    // (xEnd,yEnd) radial along the bounding ellipse.
    let sx = cx + (r * a0.cos()) as i32;
    let sy = cy - (r * a0.sin()) as i32;
    let ex = cx + (r * a1.cos()) as i32;
    let ey = cy - (r * a1.sin()) as i32;
    let _ = GdiArc(hdc, x, y, x + dotd, y + dotd, sx, sy, ex, ey);
    SelectObject(hdc, old);
    let _ = DeleteObject(pen);
}

pub(super) unsafe fn draw_status_pill(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill_rc(hdc, &rc, DARK_BG());
    pill_frame(hwnd, hdc, &rc);

    let st = about_state(hwnd);
    let (dot, text) = status_display(st);
    let checking = !st.is_null() && matches!((*st).status, Status::Checking | Status::Updating);
    let dotd = s(hwnd, 10);
    let gap = s(hwnd, 8);
    SelectObject(hdc, gui_font_for(hwnd));
    let tw = measure(hdc, &text);
    let group = dotd + gap + tw;
    let gx = rc.left + center_offset(rc.right - rc.left, group);
    let dy = rc.top + center_offset(rc.bottom - rc.top, dotd);
    if checking {
        // Animated spinner arc in place of the static dot.
        let angle = if st.is_null() { 0 } else { (*st).spinner_angle };
        draw_spinner(hwnd, hdc, gx, dy, dotd, angle);
    } else {
        // Static status dot.
        SelectObject(hdc, GetStockObject(DC_BRUSH));
        SelectObject(hdc, GetStockObject(DC_PEN));
        SetDCBrushColor(hdc, dot);
        SetDCPenColor(hdc, dot);
        let _ = Ellipse(hdc, gx, dy, gx + dotd, dy + dotd);
    }
    draw_pill_text(hdc, &text, gx + dotd + gap, &rc, DARK_TEXT());
}
