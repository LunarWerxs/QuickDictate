//! Theme-aware palette + dark-mode plumbing, ported from SageThumbs 2K's
//! `dark.rs` (the subset the About box needs). Every colour is a function
//! returning the dark value in dark mode and the light value in light mode, so
//! the same owner-draw code renders both skins. `QUICKDICTATE_THEME=light|dark`
//! overrides the OS setting for testing.

use core::ffi::c_void;
use std::sync::OnceLock;

use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{BOOL, COLORREF, HMODULE, HWND, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, SetBkColor, SetBkMode, SetTextColor, HBRUSH, HDC, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::Win32::UI::WindowsAndMessaging::{WM_CTLCOLORBTN, WM_CTLCOLORSTATIC};

pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

/// Pick the dark or light value for the current theme.
#[inline]
fn tc(dark: COLORREF, light: COLORREF) -> COLORREF {
    if is_dark() {
        dark
    } else {
        light
    }
}

// ---- Theme-aware palette (dark value, light value) — SageThumbs' values ----
#[allow(non_snake_case)]
pub fn DARK_BG() -> COLORREF {
    tc(rgb(32, 32, 32), rgb(243, 243, 243))
}
#[allow(non_snake_case)]
pub fn DARK_TEXT() -> COLORREF {
    tc(rgb(232, 232, 232), rgb(26, 26, 26))
}
#[allow(non_snake_case)]
pub fn BTN_FACE() -> COLORREF {
    tc(rgb(50, 50, 50), rgb(251, 251, 251))
}
#[allow(non_snake_case)]
pub fn BORDER_STRONG() -> COLORREF {
    tc(rgb(85, 85, 85), rgb(140, 140, 140))
}
#[allow(non_snake_case)]
pub fn HEADER_TEXT() -> COLORREF {
    tc(rgb(150, 150, 150), rgb(96, 96, 96))
}
#[allow(non_snake_case)]
pub fn DISABLED_TEXT() -> COLORREF {
    tc(rgb(110, 110, 110), rgb(163, 163, 163))
}

// ---- egui palette (same SageThumbs values, exposed as (r,g,b) tuples so the
// settings window can build egui::Color32s without a COLORREF round-trip) ----
/// #4a90f5 — the brand blue (both themes).
pub const ACCENT_RGB: (u8, u8, u8) = (74, 144, 245);
pub const ACCENT_HOT_RGB: (u8, u8, u8) = (96, 162, 250);
pub const ACCENT_PRESS_RGB: (u8, u8, u8) = (58, 120, 210);

pub fn bg_rgb() -> (u8, u8, u8) {
    if is_dark() {
        (32, 32, 32)
    } else {
        (243, 243, 243)
    }
}
pub fn surface_rgb() -> (u8, u8, u8) {
    if is_dark() {
        (41, 41, 41)
    } else {
        (255, 255, 255)
    }
}
pub fn input_rgb() -> (u8, u8, u8) {
    if is_dark() {
        (50, 50, 50)
    } else {
        (251, 251, 251)
    }
}
pub fn border_rgb() -> (u8, u8, u8) {
    if is_dark() {
        (60, 60, 60)
    } else {
        (206, 206, 206)
    }
}
pub fn text_rgb() -> (u8, u8, u8) {
    if is_dark() {
        (232, 232, 232)
    } else {
        (26, 26, 26)
    }
}
pub fn muted_rgb() -> (u8, u8, u8) {
    if is_dark() {
        (150, 150, 150)
    } else {
        (96, 96, 96)
    }
}

/// True when the (effective) theme is dark. Reads `AppsUseLightTheme == 0`,
/// cached for the life of the process. `QUICKDICTATE_THEME=light|dark`
/// overrides the registry (test hook).
pub fn is_dark() -> bool {
    static DARK: OnceLock<bool> = OnceLock::new();
    *DARK.get_or_init(|| {
        if let Ok(v) = std::env::var("QUICKDICTATE_THEME") {
            match v.to_ascii_lowercase().as_str() {
                "light" => return false,
                "dark" => return true,
                _ => {}
            }
        }
        windows_registry::CURRENT_USER
            .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
            .and_then(|k| k.get_u32("AppsUseLightTheme"))
            .map(|v| v == 0)
            .unwrap_or(false)
    })
}

type FnSetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
type FnAllowDarkModeForWindow = unsafe extern "system" fn(HWND, BOOL) -> BOOL;
type FnRefreshImmersive = unsafe extern "system" fn();

struct Uxtheme {
    set_preferred_app_mode: Option<FnSetPreferredAppMode>, // ordinal 135 (Win 1903+)
    allow_dark_for_window: Option<FnAllowDarkModeForWindow>, // ordinal 133
    refresh_immersive: Option<FnRefreshImmersive>,         // ordinal 104
}
unsafe impl Send for Uxtheme {}
unsafe impl Sync for Uxtheme {}

fn uxtheme() -> &'static Uxtheme {
    static U: OnceLock<Uxtheme> = OnceLock::new();
    U.get_or_init(|| unsafe {
        let h: HMODULE = LoadLibraryW(w!("uxtheme.dll")).unwrap_or_default();
        let by_ord = |ord: u16| GetProcAddress(h, PCSTR(ord as usize as *const u8));
        Uxtheme {
            // Undocumented Win10/11 uxtheme export ordinals; each is
            // Option-guarded so a missing ordinal degrades gracefully.
            set_preferred_app_mode: by_ord(135)
                .map(|p| std::mem::transmute::<_, FnSetPreferredAppMode>(p)),
            allow_dark_for_window: by_ord(133)
                .map(|p| std::mem::transmute::<_, FnAllowDarkModeForWindow>(p)),
            refresh_immersive: by_ord(104).map(|p| std::mem::transmute::<_, FnRefreshImmersive>(p)),
        }
    })
}

/// Put the process into "allow dark" mode — call once before creating windows.
pub unsafe fn init_dark_app() {
    let ux = uxtheme();
    if let Some(f) = ux.set_preferred_app_mode {
        f(1); // PreferredAppMode::AllowDark
    }
    if let Some(f) = ux.refresh_immersive {
        f();
    }
}

/// Opt one window/control into dark mode + apply a dark visual-style class.
pub unsafe fn dark_control(h: HWND, theme: PCWSTR) {
    if let Some(f) = uxtheme().allow_dark_for_window {
        let _ = f(h, BOOL(1));
    }
    let _ = SetWindowTheme(h, theme, PCWSTR::null());
}

/// Dark title bar via DWM.
pub unsafe fn dark_titlebar(h: HWND) {
    let on = BOOL(1);
    let _ = DwmSetWindowAttribute(
        h,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &on as *const _ as *const c_void,
        std::mem::size_of::<BOOL>() as u32,
    );
}

/// Window-background brush for the current theme (cached; theme is constant
/// per run).
pub unsafe fn dark_bg_brush() -> HBRUSH {
    static B: OnceLock<usize> = OnceLock::new();
    HBRUSH(*B.get_or_init(|| CreateSolidBrush(DARK_BG()).0 as usize) as *mut c_void)
}

/// Shared WM_CTLCOLOR* handler for statics/buttons — on-surface text colouring
/// in both themes. `Some(lresult)` means "handled, return this".
pub unsafe fn dark_ctlcolor(msg: u32, wparam: WPARAM) -> Option<LRESULT> {
    match msg {
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            let hdc = HDC(wparam.0 as *mut c_void);
            SetTextColor(hdc, DARK_TEXT());
            SetBkColor(hdc, DARK_BG());
            SetBkMode(hdc, TRANSPARENT);
            Some(LRESULT(dark_bg_brush().0 as isize))
        }
        _ => None,
    }
}
