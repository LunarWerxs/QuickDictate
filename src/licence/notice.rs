//! The licence notice: a small card in the corner of the screen, shown when a
//! dictation starts (or is refused) and a licence is needed.
//!
//! WHY NOT A MESSAGE BOX OR THE SETTINGS WINDOW. Both take the focus, and the
//! focus is where the dictated text is about to be pasted: a dialog that pops
//! up at the hotkey would eat the dictation it is warning about. This window is
//! `WS_EX_NOACTIVATE` and answers `WM_MOUSEACTIVATE` with `MA_NOACTIVATE`, so it
//! never takes the focus, not even when clicked. It closes itself after
//! [`VISIBLE_MS`], and only one is ever up at a time.
//!
//! "Buy a licence" opens the perpetual checkout; "Enter key" opens Settings on
//! its Licence page, where both plans and the key field are.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    SelectObject, SetBkMode, SetTextColor, ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK,
    FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, HDC, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
    VARIABLE_PITCH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, LoadCursorW,
    PostQuitMessage, RegisterClassExW, SetTimer, ShowWindow, SystemParametersInfoW,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, HMENU, IDC_HAND, MA_NOACTIVATE, MSG, SPI_GETWORKAREA,
    SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_DESTROY, WM_LBUTTONUP,
    WM_MOUSEACTIVATE, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_BORDER, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use super::posture::Plan;

const CLASS_NAME: &str = "QuickDictateLicenceNotice";
/// How long the notice stays before closing itself.
const VISIBLE_MS: u32 = 20_000;

/// Base size in 96-DPI pixels; scaled by the system DPI.
const BASE_W: i32 = 380;
const BASE_H: i32 = 156;
const BASE_PAD: i32 = 16;
const BASE_BTN_H: i32 = 30;

static SHOWING: AtomicBool = AtomicBool::new(false);

/// What a click asked for, acted on after the window has gone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Click {
    Buy,
    EnterKey,
    Close,
}

struct Content {
    headline: Vec<u16>,
    body: Vec<u16>,
    scale: f32,
    clicked: Option<Click>,
}

thread_local! {
    static CONTENT: RefCell<Option<Content>> = const { RefCell::new(None) };
}

/// Show the notice, unless one is already up. Never blocks the caller: the
/// window lives on its own thread with its own message loop.
pub(super) fn show((headline, body): (String, String)) {
    if SHOWING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("qd-licence-notice".into())
        .spawn(move || {
            let click = unsafe { run(&headline, &body) };
            SHOWING.store(false, Ordering::Release);
            match click {
                Some(Click::Buy) => super::open_buy(Plan::Perpetual),
                Some(Click::EnterKey) => super::open_licence_page(),
                Some(Click::Close) | None => {}
            }
        });
    if let Err(e) = spawned {
        SHOWING.store(false, Ordering::Release);
        tracing::warn!("licence: notice could not start ({e})");
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn colorref((r, g, b): (u8, u8, u8)) -> COLORREF {
    COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16))
}

fn px(v: i32, scale: f32) -> i32 {
    (v as f32 * scale).round() as i32
}

/// Where everything sits, in client pixels. Pure, so painting and hit-testing
/// cannot disagree about a button.
struct Layout {
    headline: RECT,
    body: RECT,
    close: RECT,
    buy: RECT,
    enter: RECT,
}

fn layout(w: i32, h: i32, scale: f32) -> Layout {
    let pad = px(BASE_PAD, scale);
    let btn_h = px(BASE_BTN_H, scale);
    let close_w = px(28, scale);
    let btn_top = h - pad - btn_h;
    let enter_w = px(96, scale);
    let buy_w = px(124, scale);
    let gap = px(8, scale);
    Layout {
        headline: RECT {
            left: pad,
            top: pad - px(2, scale),
            right: w - pad - close_w,
            bottom: pad + px(22, scale),
        },
        body: RECT {
            left: pad,
            top: pad + px(26, scale),
            right: w - pad,
            bottom: btn_top - px(6, scale),
        },
        close: RECT {
            left: w - close_w - px(6, scale),
            top: px(6, scale),
            right: w - px(6, scale),
            bottom: px(6, scale) + close_w,
        },
        enter: RECT {
            left: w - pad - enter_w,
            top: btn_top,
            right: w - pad,
            bottom: btn_top + btn_h,
        },
        buy: RECT {
            left: w - pad - enter_w - gap - buy_w,
            top: btn_top,
            right: w - pad - enter_w - gap,
            bottom: btn_top + btn_h,
        },
    }
}

fn hit(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// Create the window, pump its messages until it closes, and say what was
/// clicked.
unsafe fn run(headline: &str, body: &str) -> Option<Click> {
    let class = wide(CLASS_NAME);
    let h_instance = HINSTANCE(GetModuleHandleW(PCWSTR::null()).ok()?.0);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: h_instance,
        lpszClassName: PCWSTR(class.as_ptr()),
        hCursor: LoadCursorW(HINSTANCE::default(), IDC_HAND).unwrap_or_default(),
        ..Default::default()
    };
    // 1410 = ERROR_CLASS_ALREADY_EXISTS: the second notice of the process.
    if RegisterClassExW(&wc) == 0 && windows::Win32::Foundation::GetLastError().0 != 1410 {
        return None;
    }

    let scale = (GetDpiForSystem().max(96) as f32) / 96.0;
    let (w, h) = (px(BASE_W, scale), px(BASE_H, scale));
    let mut work = RECT::default();
    let _ = SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        Some(std::ptr::addr_of_mut!(work).cast()),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
    let margin = px(16, scale);
    let x = work.right - w - margin;
    let y = work.bottom - h - margin;

    CONTENT.with(|c| {
        *c.borrow_mut() = Some(Content {
            headline: headline.encode_utf16().collect(),
            body: body.encode_utf16().collect(),
            scale,
            clicked: None,
        });
    });
    let title = wide("QuickDictate licence");
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        PCWSTR(class.as_ptr()),
        PCWSTR(title.as_ptr()),
        WS_POPUP | WS_BORDER,
        x,
        y,
        w,
        h,
        HWND::default(),
        HMENU::default(),
        h_instance,
        None,
    )
    .ok()?;
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    SetTimer(hwnd, 1, VISIBLE_MS, None);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    CONTENT.with(|c| c.borrow_mut().take().and_then(|c| c.clicked))
}

unsafe fn font(px_height: i32, weight: i32) -> HFONT {
    let face = wide("Segoe UI");
    CreateFontW(
        -px_height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        ANTIALIASED_QUALITY.0 as u32,
        (VARIABLE_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
        PCWSTR(face.as_ptr()),
    )
}

unsafe fn fill(hdc: HDC, r: &RECT, rgb: (u8, u8, u8)) {
    let brush = CreateSolidBrush(colorref(rgb));
    FillRect(hdc, r, brush);
    let _ = DeleteObject(brush);
}

unsafe fn text(
    hdc: HDC,
    s: &[u16],
    r: &RECT,
    rgb: (u8, u8, u8),
    f: HFONT,
    fmt: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
) {
    let old = SelectObject(hdc, f);
    SetTextColor(hdc, colorref(rgb));
    let mut buf = s.to_vec();
    let mut rect = *r;
    DrawTextW(hdc, &mut buf, &mut rect, fmt);
    SelectObject(hdc, old);
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    CONTENT.with(|c| {
        let c = c.borrow();
        let Some(c) = c.as_ref() else {
            return;
        };
        let mut client = RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client);
        let l = layout(client.right, client.bottom, c.scale);
        fill(hdc, &client, crate::theme::surface_rgb());
        SetBkMode(hdc, TRANSPARENT);

        let head = font(px(16, c.scale), FW_SEMIBOLD.0 as i32);
        let normal = font(px(13, c.scale), FW_NORMAL.0 as i32);
        let one_line = DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS;
        text(
            hdc,
            &c.headline,
            &l.headline,
            crate::theme::text_rgb(),
            head,
            one_line,
        );
        text(
            hdc,
            &c.body,
            &l.body,
            crate::theme::muted_rgb(),
            normal,
            DT_LEFT | DT_WORDBREAK,
        );
        let centred = DT_CENTER | DT_SINGLELINE | DT_VCENTER;
        text(
            hdc,
            &"\u{00D7}".encode_utf16().collect::<Vec<_>>(),
            &l.close,
            crate::theme::muted_rgb(),
            head,
            centred,
        );
        fill(hdc, &l.buy, crate::theme::ACCENT_RGB);
        text(
            hdc,
            &"Buy a licence".encode_utf16().collect::<Vec<_>>(),
            &l.buy,
            (255, 255, 255),
            normal,
            centred,
        );
        fill(hdc, &l.enter, crate::theme::border_rgb());
        text(
            hdc,
            &"Enter key".encode_utf16().collect::<Vec<_>>(),
            &l.enter,
            crate::theme::text_rgb(),
            normal,
            centred,
        );
        let _ = DeleteObject(head);
        let _ = DeleteObject(normal);
    });
    let _ = EndPaint(hwnd, &ps);
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // The whole point: a click must not take the focus from the window
        // the dictation is going to.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = i32::from((lparam.0 & 0xffff) as u16 as i16);
            let y = i32::from(((lparam.0 >> 16) & 0xffff) as u16 as i16);
            let clicked = CONTENT.with(|c| {
                let mut c = c.borrow_mut();
                let c = c.as_mut()?;
                let mut client = RECT::default();
                let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client);
                let l = layout(client.right, client.bottom, c.scale);
                let what = if hit(&l.buy, x, y) {
                    Click::Buy
                } else if hit(&l.enter, x, y) {
                    Click::EnterKey
                } else if hit(&l.close, x, y) {
                    Click::Close
                } else {
                    return None;
                };
                c.clicked = Some(what);
                Some(what)
            });
            if clicked.is_some() {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
