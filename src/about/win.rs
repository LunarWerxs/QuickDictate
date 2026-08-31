//! Small Win32 helpers (ports of SageThumbs' `win.rs` subset), shared by the
//! About window's layout, artwork and owner-draw code.

use core::ffi::c_void;
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, CreateFontIndirectW, DeleteObject, GetDC, GetStockObject,
    GetTextExtentPoint32W, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DEFAULT_GUI_FONT,
    DIB_RGB_COLORS, HBITMAP, HDC, HFONT, HGDIOBJ, LOGFONTW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub(super) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(super) fn scale_dpi(v: i32, dpi: i32) -> i32 {
    let dpi = if dpi == 0 { 96 } else { dpi };
    (v * dpi + 48) / 96
}

/// Top-left origin (screen pixels) that centers a `win_w`×`win_h` window over
/// the QuickDictate **Settings** window, when it exists and has a sane rect.
/// Returns `None` — so the caller falls back to screen-center — if the Settings
/// window can't be found or is minimized. Matches the eframe window's title set
/// in `settings_ui::show_settings` (`run_native("QuickDictate Settings", …)`).
pub(super) unsafe fn settings_window_center(win_w: i32, win_h: i32) -> Option<(i32, i32)> {
    let title = wide("QuickDictate Settings");
    let owner = FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr())).ok()?;
    if owner.0.is_null() {
        return None;
    }
    let mut rc = RECT::default();
    GetWindowRect(owner, &mut rc).ok()?;
    let (ow, oh) = (rc.right - rc.left, rc.bottom - rc.top);
    if ow <= 0 || oh <= 0 {
        return None; // minimized / degenerate → center on screen instead
    }
    Some((rc.left + (ow - win_w) / 2, rc.top + (oh - win_h) / 2))
}

pub(super) unsafe fn s(hwnd: HWND, v: i32) -> i32 {
    scale_dpi(v, GetDpiForWindow(hwnd) as i32)
}

/// The system message font (Segoe UI on Win11), cached.
unsafe fn gui_font() -> HFONT {
    static FONT: OnceLock<usize> = OnceLock::new();
    let p = *FONT.get_or_init(|| {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        let hf = if SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            Some(&mut ncm as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
        {
            CreateFontIndirectW(&ncm.lfMessageFont)
        } else {
            HFONT(GetStockObject(DEFAULT_GUI_FONT).0)
        };
        hf.0 as usize
    });
    HFONT(p as *mut c_void)
}

/// Shared cache behind the DPI-aware font getters: look `key` up in `cache`,
/// or derive a font from the system message font at `dpi` (via `tweak`, which
/// adjusts the LOGFONTW before creation) and memoize it. `fallback` supplies
/// the font when the DPI metrics query fails.
unsafe fn cached_font<K: Copy + PartialEq>(
    cache: &'static OnceLock<std::sync::Mutex<Vec<(K, usize)>>>,
    key: K,
    dpi: u32,
    tweak: impl FnOnce(&mut LOGFONTW),
    fallback: impl FnOnce() -> HFONT,
) -> HFONT {
    let cache = cache.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    // This runs inside a wndproc. A poisoned lock means some other thread
    // panicked while holding it; the cached font handles are still perfectly
    // valid, so take the contents and carry on rather than turning someone
    // else's panic into a dead About window.
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, p)) = guard.iter().find(|(k, _)| *k == key) {
        return HFONT(p as *mut c_void);
    }
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let hf = if SystemParametersInfoForDpi(
        SPI_GETNONCLIENTMETRICS.0,
        ncm.cbSize,
        Some(&mut ncm as *mut _ as *mut c_void),
        0,
        dpi,
    )
    .is_ok()
    {
        let mut lf = ncm.lfMessageFont;
        tweak(&mut lf);
        CreateFontIndirectW(&lf)
    } else {
        fallback()
    };
    guard.push((key, hf.0 as usize));
    hf
}

/// DPI-aware GUI font for `hwnd` (system message font at the window's DPI).
pub(super) unsafe fn gui_font_for(hwnd: HWND) -> HFONT {
    let dpi = GetDpiForWindow(hwnd);
    let dpi = if dpi == 0 { 96 } else { dpi };
    if dpi == 96 {
        return gui_font();
    }
    static FONTS: OnceLock<std::sync::Mutex<Vec<(u32, usize)>>> = OnceLock::new();
    cached_font(&FONTS, dpi, dpi, |_| {}, || gui_font())
}

/// A sized/weighted variant of the DPI-aware GUI font (for the big title).
pub(super) unsafe fn gui_font_sized(hwnd: HWND, px: i32, weight: i32) -> HFONT {
    let dpi = GetDpiForWindow(hwnd);
    let dpi = if dpi == 0 { 96 } else { dpi };
    #[allow(clippy::type_complexity)]
    static FONTS: OnceLock<std::sync::Mutex<Vec<((i32, i32, u32), usize)>>> = OnceLock::new();
    cached_font(
        &FONTS,
        (px, weight, dpi),
        dpi,
        |lf| {
            lf.lfWidth = 0; // let GDI pick the natural width for the height
            lf.lfHeight = -scale_dpi(px, dpi as i32);
            lf.lfWeight = weight;
        },
        || gui_font_for(hwnd),
    )
}

/// Pixel width of `text` rendered in the GUI font (for centering controls).
pub(super) unsafe fn text_width(text: &str) -> i32 {
    let hdc = GetDC(HWND::default());
    let old = SelectObject(hdc, gui_font());
    let w = wide(text);
    let n = w.len().saturating_sub(1);
    let mut sz = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &w[..n], &mut sz);
    SelectObject(hdc, old);
    ReleaseDC(HWND::default(), hdc);
    sz.cx
}

/// Create a child STATIC at 96-DPI design coords (scaled), with the GUI font.
/// (Same positional-args shape as SageThumbs' `ctl` helper.)
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn ctl(
    parent: HWND,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    cw: i32,
    ch: i32,
    id: i32,
) -> HWND {
    let hinst = GetModuleHandleW(PCWSTR::null())
        .map(|m| HINSTANCE(m.0))
        .unwrap_or_default();
    let (x, y, cw, ch) = (s(parent, x), s(parent, y), s(parent, cw), s(parent, ch));
    let t = wide(text);
    let h = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("STATIC"),
        PCWSTR(t.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WINDOW_STYLE(style),
        x,
        y,
        cw,
        ch,
        parent,
        HMENU(id as usize as *mut c_void),
        hinst,
        None,
    )
    .unwrap_or_default();
    SendMessageW(
        h,
        WM_SETFONT,
        WPARAM(gui_font_for(parent).0 as usize),
        LPARAM(1),
    );
    h
}

/// Set a static control's bitmap, freeing whatever bitmap it held before.
pub(super) unsafe fn set_static_bitmap(ctl: HWND, hbmp: HBITMAP) {
    let old = SendMessageW(ctl, super::STM_SETIMAGE, WPARAM(0), LPARAM(hbmp.0 as isize));
    if old.0 != 0 {
        let _ = DeleteObject(HGDIOBJ(old.0 as *mut c_void));
    }
}

/// Straight-RGBA (top row first) → premultiplied 32-bpp DIB-section HBITMAP
/// (SageThumbs' `create_premultiplied_dib`).
pub(super) unsafe fn rgba_to_hbitmap(w: u32, h: u32, rgba: &[u8]) -> Option<HBITMAP> {
    if w == 0 || h == 0 || rgba.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w as i32;
    bmi.bmiHeader.biHeight = -(h as i32); // top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbmp = CreateDIBSection(HDC::default(), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(hbmp);
        return None;
    }
    let px = (w as usize) * (h as usize);
    let dst = std::slice::from_raw_parts_mut(bits as *mut u8, px * 4);
    for i in 0..px {
        let (r, g, b, a) = (
            rgba[i * 4],
            rgba[i * 4 + 1],
            rgba[i * 4 + 2],
            rgba[i * 4 + 3],
        );
        dst[i * 4] = premultiply(b, a);
        dst[i * 4 + 1] = premultiply(g, a);
        dst[i * 4 + 2] = premultiply(r, a);
        dst[i * 4 + 3] = a;
    }
    Some(hbmp)
}

pub(super) fn open_url(url: &str) {
    let u = wide(url);
    unsafe {
        ShellExecuteW(
            HWND::default(),
            w!("open"),
            PCWSTR(u.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

// ---- Colour helpers -----------------------------------------------------

pub(super) fn color_r(c: COLORREF) -> u8 {
    (c.0 & 0xFF) as u8
}
pub(super) fn color_g(c: COLORREF) -> u8 {
    ((c.0 >> 8) & 0xFF) as u8
}
pub(super) fn color_b(c: COLORREF) -> u8 {
    ((c.0 >> 16) & 0xFF) as u8
}

/// Premultiply a straight-alpha channel value against coverage `a`:
/// `(c * a + 127) / 255`, rounded to the nearest whole channel value.
pub(super) fn premultiply(c: u8, a: u8) -> u8 {
    (((c as u16) * (a as u16) + 127) / 255) as u8
}

/// Linear-interpolate `on` over `dst` by coverage `a` (0 = all `dst`, 255 =
/// all `on`) — used to tint the GitHub mark onto the pill face with no real
/// alpha channel in the destination bitmap.
pub(super) fn blend(dst: u8, on: u8, a: u8) -> u8 {
    let a = a as u32;
    ((on as u32 * a + dst as u32 * (255 - a)) / 255) as u8
}
