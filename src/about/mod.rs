//! The About box — ported from SageThumbs 2K's "2026" card (same layout, same
//! owner-draw, same language): the product logo, title + subtitle, two
//! clickable status *pills* (a GitHub version chip that opens the repo, and a
//! live "Up to date" update-check chip), the license / copyright in the
//! bottom-left, and the clickable LunarWerx Studios wordmark in the
//! bottom-right. The update check runs on a worker thread when the box opens
//! and again whenever the user clicks the status pill, so the chip is never
//! stale. When a newer release is waiting, clicking the pill installs it
//! **in-app** (download → verify → swap → relaunch via [`update`](crate::update)) rather than
//! opening the browser. Theme-aware (dark/light) and per-monitor-DPI scaled.

mod art;
mod layout;
mod paint;
mod updates;
mod win;

#[cfg(test)]
mod tests;

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, SetBkMode, SetTextColor, HBITMAP, HDC, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::theme::{
    dark_bg_brush, dark_control, dark_ctlcolor, dark_titlebar, init_dark_app, is_dark, DARK_BG,
    DISABLED_TEXT, HEADER_TEXT,
};

use art::*;
use layout::*;
use paint::*;
use updates::*;
use win::*;

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

/// Posted from the in-app install worker when the update **failed** (on success
/// the process relaunches and this window dies with it). `LPARAM` = a
/// `Box<String>` (the tag) — the handler reclaims it and restores the pill to
/// "Update to <tag>" so the user can retry; the worker already showed the error.
const WM_ABOUT_UPDATE_FAILED: u32 = WM_APP + 2;

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
    /// The user clicked "Update to <tag>" and the in-app install is running
    /// (download → verify → swap → relaunch). Shows the spinner + "Updating…".
    Updating,
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

unsafe fn about_state(hwnd: HWND) -> *mut About {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut About
}

unsafe fn ctlcolor_text(hdc: HDC, color: COLORREF) -> LRESULT {
    SetTextColor(hdc, color);
    windows::Win32::Graphics::Gdi::SetBkColor(hdc, DARK_BG());
    SetBkMode(hdc, TRANSPARENT);
    LRESULT(dark_bg_brush().0 as isize)
}

/// `WM_CTLCOLORSTATIC` for the subtitle/license/copyright statics — muted
/// on-surface colours, handled before the generic static colouring in
/// `dark_ctlcolor`. `None` when the control isn't one of the muted ones, so
/// the caller falls through to the generic path.
unsafe fn on_ctlcolorstatic(wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let id = GetDlgCtrlID(HWND(lparam.0 as *mut c_void));
    let hdc = HDC(wparam.0 as *mut c_void);
    let muted = match id {
        ID_SUBTITLE | ID_LICENSE => Some(HEADER_TEXT()),
        ID_COPYRIGHT => Some(DISABLED_TEXT()),
        _ => None,
    };
    muted.map(|c| ctlcolor_text(hdc, c))
}

unsafe fn on_wm_create(hwnd: HWND) -> LRESULT {
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

unsafe fn on_wm_drawitem(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let d = &*(lparam.0 as *const DRAWITEMSTRUCT);
    match d.CtlID as i32 {
        ID_VER_PILL => draw_ver_pill(hwnd, d),
        ID_STATUS_PILL => draw_status_pill(hwnd, d),
        _ => {}
    }
    LRESULT(1)
}

unsafe fn on_wm_timer(hwnd: HWND) -> LRESULT {
    let st = about_state(hwnd);
    if !st.is_null() && matches!((*st).status, Status::Checking | Status::Updating) {
        (*st).spinner_angle = ((*st).spinner_angle + SPINNER_STEP_DEG) % 360;
        invalidate_status(hwnd);
    } else {
        // No longer checking / updating — stop animating.
        let _ = KillTimer(hwnd, SPINNER_TIMER);
    }
    LRESULT(0)
}

unsafe fn on_wm_about_checked(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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

unsafe fn on_wm_about_update_failed(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let _ = KillTimer(hwnd, SPINNER_TIMER);
    // Reclaim the tag the worker boxed (dropped harmlessly if the window is
    // already gone).
    let tag = if lparam.0 != 0 {
        *Box::from_raw(lparam.0 as *mut String)
    } else {
        String::new()
    };
    let st = about_state(hwnd);
    if !st.is_null() {
        (*st).checking = false;
        // Restore the actionable pill so the user can retry.
        (*st).status = Status::Available(tag);
    }
    invalidate_status(hwnd);
    LRESULT(0)
}

unsafe fn on_wm_command(hwnd: HWND, wparam: WPARAM) -> LRESULT {
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

unsafe fn on_wm_setcursor(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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

unsafe fn on_wm_dpichanged(hwnd: HWND, lparam: LPARAM) -> LRESULT {
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

unsafe fn on_wm_ncdestroy(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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

extern "system" fn about_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // Muted on-surface colours for the subtitle / license / copyright —
        // handled BEFORE the generic static colouring.
        if msg == WM_CTLCOLORSTATIC {
            if let Some(r) = on_ctlcolorstatic(wparam, lparam) {
                return r;
            }
        }
        if let Some(r) = dark_ctlcolor(msg, wparam) {
            return r;
        }
        match msg {
            WM_CREATE => on_wm_create(hwnd),
            WM_DRAWITEM => on_wm_drawitem(hwnd, lparam),
            WM_TIMER if wparam.0 == SPINNER_TIMER => on_wm_timer(hwnd),
            WM_ABOUT_CHECKED => on_wm_about_checked(hwnd, wparam, lparam),
            WM_ABOUT_UPDATE_FAILED => on_wm_about_update_failed(hwnd, lparam),
            WM_COMMAND => on_wm_command(hwnd, wparam),
            WM_SETCURSOR => on_wm_setcursor(hwnd, msg, wparam, lparam),
            WM_KEYDOWN if wparam.0 == 0x1B => {
                // Esc closes (plain window — no dialog manager to send IDCANCEL).
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DPICHANGED => on_wm_dpichanged(hwnd, lparam),
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_NCDESTROY => on_wm_ncdestroy(hwnd, msg, wparam, lparam),
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
