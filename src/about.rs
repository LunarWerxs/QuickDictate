//! The About box — ported from SageThumbs 2K's "2026" card (same layout, same
//! owner-draw, same language): the product logo, title + subtitle, two
//! clickable status *pills* (a GitHub version chip that opens the repo, and a
//! live "Up to date" update-check chip), the license / copyright in the
//! bottom-left, and the clickable LunarWerx Studios wordmark in the
//! bottom-right. The update check runs on a worker thread when the box opens
//! and again whenever the user clicks the status pill, so the chip is never
//! stale. Theme-aware (dark/light) and per-monitor-DPI scaled.

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    Arc as GdiArc, BitBlt, CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, CreatePen,
    DeleteDC, DeleteObject, DrawTextW, Ellipse, FillRect, GetDC, GetStockObject,
    GetTextExtentPoint32W, InvalidateRect, ReleaseDC, RoundRect, SelectObject, SetBkMode,
    SetDCBrushColor, SetDCPenColor, SetTextColor, BITMAPINFO, BITMAPINFOHEADER, DC_BRUSH, DC_PEN,
    DEFAULT_GUI_FONT, DIB_RGB_COLORS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HBITMAP,
    HBRUSH, HDC, HFONT, HGDIOBJ, LOGFONTW, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow, SystemParametersInfoForDpi,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::theme::{
    dark_bg_brush, dark_control, dark_ctlcolor, dark_titlebar, init_dark_app, is_dark, rgb,
    BORDER_STRONG, BTN_FACE, DARK_BG, DARK_TEXT, DISABLED_TEXT, HEADER_TEXT,
};
use crate::update;

pub const REPO_URL: &str = "https://github.com/LunarWerxs/QuickDictate";
const LUNARWERX_URL: &str = "https://lunarwerx.com";

// ---- Control IDs --------------------------------------------------------
/// The clickable LunarWerx Studios wordmark (bottom-right) → the company site.
const ID_LW_LOGO: i32 = 1119;
/// The GitHub version chip → the repo.
const ID_VER_PILL: i32 = 1201;
/// The live update-check chip → re-check (or, when an update exists, the releases page).
const ID_STATUS_PILL: i32 = 1202;
const ID_SUBTITLE: i32 = 1203;
const ID_LICENSE: i32 = 1204;
const ID_COPYRIGHT: i32 = 1205;

/// Posted from the update-check worker thread back to the About window: the check
/// finished. `WPARAM` = outcome (0 up-to-date, 1 update available, 2 failed);
/// `LPARAM` = a `Box<String>` (the newer tag) when WPARAM==1 — the handler reclaims it.
const WM_ABOUT_CHECKED: u32 = WM_APP + 1;

/// Timer that spins the "Checking…" arc while a check is in flight.
const SPINNER_TIMER: usize = 1;
/// Spinner repaint cadence (ms) and per-tick rotation (degrees).
const SPINNER_INTERVAL_MS: u32 = 60;
const SPINNER_STEP_DEG: i32 = 30;
/// Minimum time the "Checking…" spinner stays up — the network check often
/// returns in well under a second, but a spinner that flashes for 100 ms reads
/// as "nothing happened". Padding it to a beat makes the check feel real.
const SPINNER_MIN_MS: u64 = 2000;

/// Client size in 96-DPI design pixels (DPI-scaled per control / for the frame).
const CW: i32 = 440;
const CH: i32 = 300;

/// LunarWerx Studios wordmark — the LIGHT (white) variant on transparent
/// (1680×273), for the dark card.
const LW_LOGO_PNG: &[u8] = include_bytes!("../assets/lw_logo_white.png");
/// LunarWerx Studios wordmark — the DARK (navy) variant on transparent
/// (4911×941), for the light card.
const LW_LOGO_DARK_PNG: &[u8] = include_bytes!("../assets/lw_logo_dark.png");
/// GitHub "mark" (white silhouette on transparent) for the version pill.
const GH_PNG: &[u8] = include_bytes!("../assets/github_mark.png");

/// Version-pill GitHub icon size (96-dpi design px).
const ICON: i32 = 20;

/// STATIC-control styles (winuser.h values; the windows crate doesn't export them).
const SS_CENTER: u32 = 0x0001;
const SS_OWNERDRAW: u32 = 0x000D;
const SS_BITMAP: u32 = 0x000E;
const SS_NOTIFY: u32 = 0x0100;
const STN_CLICKED: u32 = 0;
const STM_SETIMAGE: u32 = 0x0172;

/// Single-instance guard for the About window.
static OPEN: AtomicBool = AtomicBool::new(false);

/// The latest update-check outcome, shown by the status pill.
enum Status {
    Checking,
    UpToDate,
    Available(String),
    Failed,
}

/// Per-window state, owned via `GWLP_USERDATA`.
struct About {
    status: Status,
    /// A network check is in flight — ignore extra status-pill clicks until it lands.
    checking: bool,
    /// The GitHub mark, pre-composited on the pill fill so the blit is seamless.
    gh_icon: Option<HBITMAP>,
    /// Child HWNDs (as raw values) for hit-testing WM_SETCURSOR / invalidation.
    ver_pill: isize,
    status_pill: isize,
    lw_logo: isize,
    /// Current rotation (degrees) of the "Checking…" spinner arc.
    spinner_angle: i32,
}

/// Open the About box on its own thread (the tray thread must never block).
pub fn show_about() {
    if OPEN.swap(true, Ordering::AcqRel) {
        return; // already open
    }
    std::thread::Builder::new()
        .name("qd-about".into())
        .spawn(|| {
            unsafe { run_about() };
            OPEN.store(false, Ordering::Release);
        })
        .ok();
}

unsafe fn run_about() {
    init_dark_app();
    let Ok(h_module) = GetModuleHandleW(PCWSTR::null()) else {
        return;
    };
    let hinst = HINSTANCE(h_module.0);
    let class = w!("QuickDictateAbout");
    // Idempotent: a second RegisterClassW returns 0 (already registered) — fine.
    let wc = WNDCLASSW {
        lpfnWndProc: Some(about_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
        hbrBackground: dark_bg_brush(), // theme-aware: light bg in light, dark bg in dark
        ..Default::default()
    };
    RegisterClassW(&wc);

    // Size the frame so the *client* area is exactly the design size, scaled
    // to the system DPI (no owner window to inherit from — tray-launched).
    let dpi = GetDpiForSystem() as i32;
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
    let exstyle = WS_EX_DLGMODALFRAME | WS_EX_TOPMOST;
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: scale_dpi(CW, dpi),
        bottom: scale_dpi(CH, dpi),
    };
    let _ = AdjustWindowRectExForDpi(&mut rc, style, false, exstyle, dpi as u32);
    let (win_w, win_h) = (rc.right - rc.left, rc.bottom - rc.top);
    // Open centered over the Settings window that launched us (About is only
    // ever opened from there); fall back to the primary screen's center if it
    // can't be located — e.g. minimized.
    let (x, y) = match settings_window_center(win_w, win_h) {
        Some(p) => p,
        None => (
            (GetSystemMetrics(SM_CXSCREEN) - win_w) / 2,
            (GetSystemMetrics(SM_CYSCREEN) - win_h) / 2,
        ),
    };

    let Ok(hwnd) = CreateWindowExW(
        exstyle,
        class,
        w!("About QuickDictate"),
        style,
        x,
        y,
        win_w,
        win_h,
        HWND::default(),
        HMENU::default(),
        hinst,
        None,
    ) else {
        return;
    };
    if is_dark() {
        dark_control(hwnd, w!("DarkMode_Explorer"));
        dark_titlebar(hwnd);
    }
    let _ = ShowWindow(hwnd, SW_SHOW);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

// ---- Small Win32 helpers (ports of SageThumbs' win.rs subset) ------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn scale_dpi(v: i32, dpi: i32) -> i32 {
    let dpi = if dpi == 0 { 96 } else { dpi };
    (v * dpi + 48) / 96
}

/// Top-left origin (screen pixels) that centers a `win_w`×`win_h` window over
/// the QuickDictate **Settings** window, when it exists and has a sane rect.
/// Returns `None` — so the caller falls back to screen-center — if the Settings
/// window can't be found or is minimized. Matches the eframe window's title set
/// in `settings_ui::show_settings` (`run_native("QuickDictate Settings", …)`).
unsafe fn settings_window_center(win_w: i32, win_h: i32) -> Option<(i32, i32)> {
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

unsafe fn s(hwnd: HWND, v: i32) -> i32 {
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
    let mut guard = cache.lock().unwrap();
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
unsafe fn gui_font_for(hwnd: HWND) -> HFONT {
    let dpi = GetDpiForWindow(hwnd);
    let dpi = if dpi == 0 { 96 } else { dpi };
    if dpi == 96 {
        return gui_font();
    }
    static FONTS: OnceLock<std::sync::Mutex<Vec<(u32, usize)>>> = OnceLock::new();
    cached_font(&FONTS, dpi, dpi, |_| {}, || gui_font())
}

/// A sized/weighted variant of the DPI-aware GUI font (for the big title).
unsafe fn gui_font_sized(hwnd: HWND, px: i32, weight: i32) -> HFONT {
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
unsafe fn text_width(text: &str) -> i32 {
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
unsafe fn ctl(
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
unsafe fn set_static_bitmap(ctl: HWND, hbmp: HBITMAP) {
    let old = SendMessageW(ctl, STM_SETIMAGE, WPARAM(0), LPARAM(hbmp.0 as isize));
    if old.0 != 0 {
        let _ = DeleteObject(HGDIOBJ(old.0 as *mut c_void));
    }
}

/// Straight-RGBA (top row first) → premultiplied 32-bpp DIB-section HBITMAP
/// (SageThumbs' `create_premultiplied_dib`).
unsafe fn rgba_to_hbitmap(w: u32, h: u32, rgba: &[u8]) -> Option<HBITMAP> {
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
        let m = |c: u8| (((c as u16) * (a as u16) + 127) / 255) as u8;
        dst[i * 4] = m(b);
        dst[i * 4 + 1] = m(g);
        dst[i * 4 + 2] = m(r);
        dst[i * 4 + 3] = a;
    }
    Some(hbmp)
}

fn open_url(url: &str) {
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

fn color_r(c: COLORREF) -> u8 {
    (c.0 & 0xFF) as u8
}
fn color_g(c: COLORREF) -> u8 {
    ((c.0 >> 8) & 0xFF) as u8
}
fn color_b(c: COLORREF) -> u8 {
    ((c.0 >> 16) & 0xFF) as u8
}

unsafe fn about_state(hwnd: HWND) -> *mut About {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut About
}

// ---- Artwork -------------------------------------------------------------

/// The QuickDictate logo (the blue mic tile — same art as the tray / exe icon,
/// see `crate::icon`), decoded and scaled to `size`² and flattened onto the
/// card background so the tile's rounded corners match the card. Mirrors
/// `lw_logo_hbitmap` above.
fn qd_logo_rgba(size: u32) -> Vec<u8> {
    let base = DARK_BG();
    let sz = size.max(1);
    let logo = image::load_from_memory(crate::icon::PNG)
        .expect("decode app icon")
        .resize_exact(sz, sz, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
    let mut out = image::RgbaImage::from_pixel(
        sz,
        sz,
        image::Rgba([color_r(base), color_g(base), color_b(base), 255]),
    );
    image::imageops::overlay(&mut out, &logo, 0, 0);
    out.into_raw()
}

/// The LunarWerx Studios wordmark bytes + aspect ratio for the active theme.
fn lw_logo() -> (&'static [u8], f32) {
    if is_dark() {
        (LW_LOGO_PNG, 1680.0 / 273.0)
    } else {
        (LW_LOGO_DARK_PNG, 4911.0 / 941.0)
    }
}

/// The themed LunarWerx wordmark sized to `w`×`h`, composited onto the card
/// background (SS_BITMAP BitBlts — no alpha — so we pre-composite).
unsafe fn lw_logo_hbitmap(w: u32, h: u32) -> Option<HBITMAP> {
    let (bytes, _) = lw_logo();
    let logo = image::load_from_memory(bytes)
        .ok()?
        .resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let base = DARK_BG();
    let mut out = image::RgbaImage::from_pixel(
        w,
        h,
        image::Rgba([color_r(base), color_g(base), color_b(base), 255]),
    );
    image::imageops::overlay(&mut out, &logo, 0, 0);
    rgba_to_hbitmap(w, h, out.as_raw())
}

/// The GitHub mark at `px`², tinted `fg` and composited over `fill` (the pill
/// face), so it can be BitBlt'd straight onto the pill with no alpha-blend.
unsafe fn github_icon_hbitmap(px: u32, fill: COLORREF, fg: COLORREF) -> Option<HBITMAP> {
    let src = image::load_from_memory(GH_PNG)
        .ok()?
        .resize_exact(px.max(1), px.max(1), image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let (fr, fgc, fb) = (color_r(fill), color_g(fill), color_b(fill));
    let (gr, gg, gb) = (color_r(fg), color_g(fg), color_b(fg));
    let mut out = image::RgbaImage::new(src.width(), src.height());
    for (o, p) in out.pixels_mut().zip(src.pixels()) {
        let a = p[3] as u32; // octocat coverage
        let mix = |dst: u8, on: u8| ((on as u32 * a + dst as u32 * (255 - a)) / 255) as u8;
        *o = image::Rgba([mix(fr, gr), mix(fgc, gg), mix(fb, gb), 255]);
    }
    rgba_to_hbitmap(out.width(), out.height(), out.as_raw())
}

// ---- Layout --------------------------------------------------------------

unsafe fn build_about(hwnd: HWND) {
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
    let ver = format!("v{}", env!("CARGO_PKG_VERSION"));
    let ver_w = 14 + ICON + 7 + text_width(&ver) + 14;
    let cand = [
        "Checking\u{2026}".to_string(),
        "Up to date".to_string(),
        "Check failed".to_string(),
        "Update to 99.99.99".to_string(),
    ];
    let max_tw = cand.iter().map(|c| text_width(c)).max().unwrap_or(80);
    let status_w = 14 + 10 + 8 + max_tw + 14;
    let gap = 12;
    let gx = (CW - (ver_w + gap + status_w)) / 2;
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
    let lw_w = (lw_h as f32 * lw_aspect).round() as i32;
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

// ---- Update check (worker thread → WM_ABOUT_CHECKED) --------------------

/// Kick off a fresh GitHub update check on a worker thread; it posts the
/// outcome back to `hwnd` via [`WM_ABOUT_CHECKED`]. Also starts the spinner
/// timer so the "Checking…" arc animates until the result lands.
unsafe fn start_check(hwnd: HWND) {
    // Animate the "Checking…" arc while the network call is in flight.
    let st = about_state(hwnd);
    if !st.is_null() {
        (*st).spinner_angle = 0;
    }
    let _ = SetTimer(hwnd, SPINNER_TIMER, SPINNER_INTERVAL_MS, None);

    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let outcome = update::check();
        // Hold the spinner up for at least SPINNER_MIN_MS so the check is
        // visibly "working" even when GitHub answers instantly.
        let min = std::time::Duration::from_millis(SPINNER_MIN_MS);
        let elapsed = start.elapsed();
        if elapsed < min {
            std::thread::sleep(min - elapsed);
        }
        let (code, lp) = match outcome {
            update::UpdateCheck::UpToDate => (0usize, 0isize),
            update::UpdateCheck::Available(tag) => (1usize, Box::into_raw(Box::new(tag)) as isize),
            update::UpdateCheck::Failed => (2usize, 0isize),
        };
        unsafe {
            let _ = PostMessageW(
                HWND(raw as *mut c_void),
                WM_ABOUT_CHECKED,
                WPARAM(code),
                LPARAM(lp),
            );
        }
    });
}

unsafe fn invalidate_status(hwnd: HWND) {
    let st = about_state(hwnd);
    if !st.is_null() && (*st).status_pill != 0 {
        let _ = InvalidateRect(HWND((*st).status_pill as *mut c_void), None, true);
    }
}

/// Status-pill click: open the releases page when an update is waiting,
/// otherwise re-run the check (unless one is already in flight).
unsafe fn on_status_click(hwnd: HWND) {
    let st = about_state(hwnd);
    if st.is_null() {
        return;
    }
    if let Status::Available(_) = (*st).status {
        open_url(update::RELEASES_URL);
        return;
    }
    if (*st).checking {
        return;
    }
    (*st).checking = true;
    (*st).status = Status::Checking;
    invalidate_status(hwnd);
    start_check(hwnd);
}

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
unsafe fn blit(dst: HDC, hbmp: HBITMAP, x: i32, y: i32, w: i32, h: i32) {
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

unsafe fn draw_ver_pill(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill_rc(hdc, &rc, DARK_BG());
    pill_frame(hwnd, hdc, &rc);

    let icon_px = s(hwnd, ICON);
    let gap = s(hwnd, 7);
    let ver = format!("v{}", env!("CARGO_PKG_VERSION"));
    SelectObject(hdc, gui_font_for(hwnd));
    let tw = measure(hdc, &ver);
    let group = icon_px + gap + tw;
    let gx = rc.left + ((rc.right - rc.left) - group) / 2;
    let iy = rc.top + ((rc.bottom - rc.top) - icon_px) / 2;
    let st = about_state(hwnd);
    if !st.is_null() {
        if let Some(icon) = (*st).gh_icon {
            blit(hdc, icon, gx, iy, icon_px, icon_px);
        }
    }
    draw_pill_text(hdc, &ver, gx + icon_px + gap, &rc, DARK_TEXT());
}

/// Map the current status to (dot colour, label).
unsafe fn status_display(st: *mut About) -> (COLORREF, String) {
    if st.is_null() {
        return (rgb(150, 150, 150), "Checking\u{2026}".to_string());
    }
    match &(*st).status {
        Status::Checking => (rgb(150, 150, 150), "Checking\u{2026}".to_string()),
        Status::UpToDate => (rgb(63, 185, 80), "Up to date".to_string()),
        Status::Available(tag) => (rgb(210, 153, 34), format!("Update to {tag}")),
        Status::Failed => (rgb(190, 110, 110), "Check failed".to_string()),
    }
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

unsafe fn draw_status_pill(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill_rc(hdc, &rc, DARK_BG());
    pill_frame(hwnd, hdc, &rc);

    let st = about_state(hwnd);
    let (dot, text) = status_display(st);
    let checking = !st.is_null() && matches!((*st).status, Status::Checking);
    let dotd = s(hwnd, 10);
    let gap = s(hwnd, 8);
    SelectObject(hdc, gui_font_for(hwnd));
    let tw = measure(hdc, &text);
    let group = dotd + gap + tw;
    let gx = rc.left + ((rc.right - rc.left) - group) / 2;
    let dy = rc.top + ((rc.bottom - rc.top) - dotd) / 2;
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

unsafe fn ctlcolor_text(hdc: HDC, color: COLORREF) -> LRESULT {
    SetTextColor(hdc, color);
    windows::Win32::Graphics::Gdi::SetBkColor(hdc, DARK_BG());
    SetBkMode(hdc, TRANSPARENT);
    LRESULT(dark_bg_brush().0 as isize)
}

extern "system" fn about_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // Muted on-surface colours for the subtitle / license / copyright —
        // handled BEFORE the generic static colouring.
        if msg == WM_CTLCOLORSTATIC {
            let id = GetDlgCtrlID(HWND(lparam.0 as *mut c_void));
            let hdc = HDC(wparam.0 as *mut c_void);
            let muted = match id {
                ID_SUBTITLE | ID_LICENSE => Some(HEADER_TEXT()),
                ID_COPYRIGHT => Some(DISABLED_TEXT()),
                _ => None,
            };
            if let Some(c) = muted {
                return ctlcolor_text(hdc, c);
            }
        }
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => {
                let state = Box::new(About {
                    status: Status::Checking,
                    checking: true,
                    gh_icon: None,
                    ver_pill: 0,
                    status_pill: 0,
                    lw_logo: 0,
                    spinner_angle: 0,
                });
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
                build_about(hwnd);
                start_check(hwnd); // check on open
                LRESULT(0)
            }
            WM_DRAWITEM => {
                let d = &*(lparam.0 as *const DRAWITEMSTRUCT);
                match d.CtlID as i32 {
                    ID_VER_PILL => draw_ver_pill(hwnd, d),
                    ID_STATUS_PILL => draw_status_pill(hwnd, d),
                    _ => {}
                }
                LRESULT(1)
            }
            WM_TIMER if wparam.0 == SPINNER_TIMER => {
                let st = about_state(hwnd);
                if !st.is_null() && matches!((*st).status, Status::Checking) {
                    (*st).spinner_angle = ((*st).spinner_angle + SPINNER_STEP_DEG) % 360;
                    invalidate_status(hwnd);
                } else {
                    // No longer checking — stop animating.
                    let _ = KillTimer(hwnd, SPINNER_TIMER);
                }
                LRESULT(0)
            }
            WM_ABOUT_CHECKED => {
                let _ = KillTimer(hwnd, SPINNER_TIMER);
                let st = about_state(hwnd);
                if !st.is_null() {
                    (*st).checking = false;
                    (*st).status = match wparam.0 {
                        1 => {
                            let tag = if lparam.0 != 0 {
                                *Box::from_raw(lparam.0 as *mut String)
                            } else {
                                String::new()
                            };
                            Status::Available(tag)
                        }
                        2 => Status::Failed,
                        _ => Status::UpToDate,
                    };
                } else if lparam.0 != 0 {
                    // Window torn down between post and dispatch — reclaim the tag.
                    drop(Box::from_raw(lparam.0 as *mut String));
                }
                invalidate_status(hwnd);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let notify = ((wparam.0 >> 16) & 0xFFFF) as u32;
                match id {
                    1 | 2 => {
                        // IDOK / IDCANCEL (Enter / Esc via the dialog manager).
                        let _ = DestroyWindow(hwnd);
                    }
                    ID_LW_LOGO if notify == STN_CLICKED => open_url(LUNARWERX_URL),
                    ID_VER_PILL if notify == STN_CLICKED => open_url(REPO_URL),
                    ID_STATUS_PILL if notify == STN_CLICKED => on_status_click(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                // Hand cursor over the three clickables; default elsewhere.
                let over = wparam.0 as isize;
                let st = about_state(hwnd);
                let clickable = !st.is_null()
                    && [(*st).ver_pill, (*st).status_pill, (*st).lw_logo]
                        .iter()
                        .any(|&h| h != 0 && h == over);
                if clickable {
                    if let Ok(hand) = LoadCursorW(HINSTANCE::default(), IDC_HAND) {
                        SetCursor(hand);
                    }
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_KEYDOWN if wparam.0 == 0x1B => {
                // Esc closes (plain window — no dialog manager to send IDCANCEL).
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                if lparam.0 != 0 {
                    let r = &*(lparam.0 as *const RECT);
                    let _ = SetWindowPos(
                        hwnd,
                        HWND::default(),
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let _ = KillTimer(hwnd, SPINNER_TIMER);
                let p = about_state(hwnd);
                if !p.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    let st = Box::from_raw(p);
                    if let Some(icon) = st.gh_icon {
                        let _ = DeleteObject(icon);
                    }
                }
                PostQuitMessage(0);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
