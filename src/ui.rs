//! Tray icon + cursor-following status pip.
//!
//! The pip is a layered window rendered via `UpdateLayeredWindow` with a
//! 32-bit premultiplied-alpha DIB. That gives us a *real* anti-aliased
//! circle (and anti-aliased text) -- the previous `Ellipse` + `LWA_COLORKEY`
//! approach could only produce 1-bit alpha, which read as a chunky octagon.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GetDC,
    ReleaseDC, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER,
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_BOLD,
    HBITMAP, HDC, OUT_DEFAULT_PRECIS, TRANSPARENT, VARIABLE_PITCH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::HMENU;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, LoadCursorW, PeekMessageW,
    PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, ShowWindow, TranslateMessage,
    UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, IDC_ARROW, IDYES, MB_ICONQUESTION,
    MB_YESNO, MSG, PM_REMOVE, SW_HIDE, SW_SHOWNA, ULW_ALPHA, WM_DESTROY, WM_QUIT, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::state::{App, ErrorKind, Status};

const PIP_SIZE: i32 = 48;
const PIP_OFFSET_X: i32 = 18;
const PIP_OFFSET_Y: i32 = 18;
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(16);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Class name of the hidden overlay window -- also the target `main.rs`'s
/// single-instance guard looks up via `FindWindowW` to reach a running
/// instance (the overlay is the only always-alive top-level window we own).
pub const OVERLAY_CLASS_NAME: &str = "QuickDictateOverlay";

/// Registered-message string a second launch uses to ask the running instance
/// to reveal Settings. Resolved to a numeric id via `RegisterWindowMessageW`
/// (guaranteed unique system-wide, no collision risk with any `WM_*`
/// constant) both here, in `overlay_wnd_proc`, and by the launching instance
/// in `main.rs`.
pub const ACTIVATE_MESSAGE_NAME: &str = "QuickDictate.ShowSettings";

/// Cached result of `RegisterWindowMessageW(ACTIVATE_MESSAGE_NAME)`. `0` means
/// "not yet registered" (`RegisterWindowMessageW` never returns 0 on success).
static ACTIVATE_MESSAGE_ID: AtomicU32 = AtomicU32::new(0);

/// Set by `overlay_wnd_proc` when it receives the activate message from a
/// second launch; polled and cleared by [`run`], which then calls the exact
/// same `settings_ui::show_settings` path the tray's "Settings…" menu item
/// uses.
static SHOW_SETTINGS_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Registers (once) and returns the numeric id of the cross-instance activate
/// message. Safe to call from any thread; `RegisterWindowMessageW` itself is
/// thread-safe and idempotent for a given string.
fn activate_message_id() -> u32 {
    let cached = ACTIVATE_MESSAGE_ID.load(Ordering::Acquire);
    if cached != 0 {
        return cached;
    }
    let wide: Vec<u16> = ACTIVATE_MESSAGE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let id = unsafe { RegisterWindowMessageW(PCWSTR(wide.as_ptr())) };
    if id != 0 {
        ACTIVATE_MESSAGE_ID.store(id, Ordering::Release);
    }
    id
}

pub fn spawn(app: Arc<App>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("qd-ui".into())
        .spawn(move || {
            if let Err(e) = run(app) {
                tracing::error!("ui thread: {e:#}");
            }
        })
        .expect("spawn ui thread")
}

fn run(app: Arc<App>) -> Result<()> {
    let tray_state = build_tray()?;
    let menu_rx = MenuEvent::receiver();

    // Register the cross-instance "show Settings" message before the overlay
    // window (whose wnd_proc handles it) is created, so there's no window
    // that could receive WM_CREATE etc. before the id is known. Idempotent.
    let _ = activate_message_id();

    // Apply the persisted hide-tray-icon setting immediately, before the
    // window is ever shown -- otherwise the icon would flash visible for one
    // frame on every launch. Live changes are picked up below in the poll
    // loop.
    let mut last_hide_tray_icon = app.config.load().hide_tray_icon;
    if last_hide_tray_icon {
        if let Err(e) = tray_state.tray.set_visible(false) {
            tracing::warn!("tray: initial set_visible(false) failed: {e}");
        }
    }

    let overlay = unsafe { Overlay::create(PIP_SIZE)? };
    tracing::info!("overlay hwnd={:?}", overlay.hwnd.0);

    let mut last_status = Status::Idle;
    let mut last_error_kind = ErrorKind::Generic;
    let mut last_pos: Option<POINT> = None;
    let mut last_word_count: u32 = u32::MAX;
    let mut last_spinner = false;
    let mut spinner_angle = 0.0_f32;
    let mut msg = MSG::default();

    // Tray-tooltip explanation for a dead-keys failure. The 2s error pip clears
    // fast, so this persists the "why" on the tray icon's hover text until a
    // dictation actually connects again (or the app restarts).
    const DEAD_KEYS_TOOLTIP: &str =
        "QuickDictate: your API keys were rejected. Open Settings to update them.";
    let default_tooltip = format!("QuickDictate v{}", env!("CARGO_PKG_VERSION"));
    let mut dead_keys_tooltip_active = false;
    // Rebuild the "Recent transcriptions" submenu only when the history has
    // actually changed since we last drew it (cheap version counter, see
    // `TranscriptHistory::version`), not on every poll tick.
    let mut last_history_version: u64 = u64::MAX;

    // Smoothed display counter — lerps toward the live word count so the pip
    // animates instead of snapping. Asymmetric rates: fast on the way up
    // (feels responsive), slow on the way down (damps STT revision jitter).
    let mut display_count: f32 = 0.0;

    loop {
        if app.shutdown.load(Ordering::Acquire) {
            break;
        }

        while unsafe { PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() } {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if msg.message == WM_QUIT {
                app.shutdown.store(true, Ordering::Release);
                break;
            }
        }

        while let Ok(ev) = menu_rx.try_recv() {
            // The tray is intentionally minimal — Settings, Hide tray icon,
            // Recent transcriptions, and Quit. About / updates / log / JSON
            // editing all live inside the Settings window now.
            let id = ev.id().as_ref();
            if ev.id() == &MenuId::new("settings") {
                crate::settings_ui::show_settings(Arc::clone(&app));
            } else if ev.id() == &MenuId::new("hide_tray") {
                // Confirm first: this removes the only *visible* way back into
                // Settings, so the dialog is the one place we can spell out the
                // way back (the tray-icon crate has no per-item tooltip, unlike
                // the Settings checkbox this mirrors). Runs on its own thread so
                // the modal doesn't stall the active poll loop below and
                // freeze the pip mid-dictation.
                let app = Arc::clone(&app);
                std::thread::spawn(move || {
                    let answer = crate::update::msg_box(
                        "QuickDictate",
                        "Hide the tray icon?\n\n\
                         QuickDictate keeps running in the background and your \
                         dictation hotkeys keep working; only the notification-area \
                         icon goes away.\n\n\
                         To get it back, launch QuickDictate again: it reopens \
                         Settings instead of starting a second copy, and you can \
                         untick \"Hide tray icon\" there.",
                        MB_YESNO | MB_ICONQUESTION,
                    );
                    if answer == IDYES {
                        set_hide_tray_icon(&app, true);
                    }
                });
            } else if ev.id() == &MenuId::new("quit") {
                tracing::info!("Quit selected from tray menu");
                app.shutdown.store(true, Ordering::Release);
            } else if ev.id() == &MenuId::new("history:copyall") {
                // "Copy all (N)": concatenate every recent transcription and put
                // the whole batch on the clipboard in one go. Joined oldest-first
                // so the newest lands at the bottom (the way a transcript reads
                // and where the eye naturally goes), even though the menu itself
                // lists them newest-first. A blank line between entries keeps each
                // dictation a distinct paragraph when pasted.
                let all: Vec<String> = app
                    .history
                    .lock()
                    .snapshot()
                    .into_iter()
                    .rev() // newest-first snapshot -> oldest-first output
                    .map(|e| e.text)
                    .filter(|t| !t.is_empty())
                    .collect();
                if all.is_empty() {
                    tracing::warn!("Recent transcriptions: 'Copy all' with nothing to copy");
                } else {
                    let joined = all.join("\n\n");
                    let n = all.len();
                    match crate::output::copy_to_clipboard(&joined) {
                        Ok(()) => tracing::info!(
                            "Recent transcriptions: copied all {n} entries ({} chars) to clipboard",
                            joined.chars().count()
                        ),
                        Err(e) => tracing::warn!(
                            "Recent transcriptions: 'Copy all' clipboard copy failed: {e:#}"
                        ),
                    }
                }
            } else if let Some(idx) = id.strip_prefix("history:") {
                match idx.parse::<usize>() {
                    Ok(i) => {
                        // Clicking a recent transcription copies it to the
                        // clipboard (the user pastes it themselves) rather than
                        // auto-pasting into the focused window.
                        let entry = app.history.lock().get(i);
                        match entry {
                            Some(entry) if !entry.text.is_empty() => {
                                match crate::output::copy_to_clipboard(&entry.text) {
                                    Ok(()) => tracing::info!(
                                        "Recent transcriptions: copied entry {i} ({} chars) to clipboard",
                                        entry.text.chars().count()
                                    ),
                                    Err(e) => tracing::warn!(
                                        "Recent transcriptions: clipboard copy failed: {e:#}"
                                    ),
                                }
                            }
                            _ => tracing::warn!(
                                "Recent transcriptions: entry {i} is missing or empty"
                            ),
                        }
                    }
                    Err(_) => tracing::warn!("bad history menu id: {id}"),
                }
            }
        }

        // Keep the "Recent transcriptions" submenu in sync with the app's
        // history. Cheap to check every tick (one lock + an integer
        // compare); only rebuilds the actual menu items when it changed.
        {
            let version = app.history.lock().version();
            if version != last_history_version {
                let snapshot = app.history.lock().snapshot();
                tray_state.rebuild_history_menu(&snapshot);
                last_history_version = version;
            }
        }

        let status = app.status();
        let error_kind = app.error_kind();
        let cfg = app.config.load();
        let target_count = app.word_count.load(Ordering::Acquire) as f32;
        let want_visible = cfg.mouse_follower_enabled && status != Status::Idle;
        let show_spinner = cfg.stt_provider.eq_ignore_ascii_case("local")
            && matches!(
                status,
                Status::Starting | Status::Listening | Status::Processing
            );
        if show_spinner {
            spinner_angle = (spinner_angle + std::f32::consts::TAU / 48.0) % std::f32::consts::TAU;
        } else {
            spinner_angle = 0.0;
        }

        // Surface a dead-keys failure on the tray tooltip, and keep it there
        // until a session actually connects (Listening) so the explanation
        // outlives the brief error pip.
        if status == Status::Error && error_kind == ErrorKind::DeadKeys {
            if !dead_keys_tooltip_active {
                let _ = tray_state.tray.set_tooltip(Some(DEAD_KEYS_TOOLTIP));
                dead_keys_tooltip_active = true;
            }
        } else if dead_keys_tooltip_active && status == Status::Listening {
            let _ = tray_state.tray.set_tooltip(Some(&default_tooltip));
            dead_keys_tooltip_active = false;
        }

        // Live-apply the hide-tray-icon setting whenever it changes -- no
        // restart needed. tray-icon 0.19's set_visible is a thin wrapper over
        // Shell_NotifyIconW(NIM_MODIFY) on Windows, so this is cheap enough
        // to check every tick.
        if cfg.hide_tray_icon != last_hide_tray_icon {
            if let Err(e) = tray_state.tray.set_visible(!cfg.hide_tray_icon) {
                tracing::warn!("tray: set_visible({}) failed: {e}", !cfg.hide_tray_icon);
            }
            last_hide_tray_icon = cfg.hide_tray_icon;
        }

        // A second launch (blocked by the single-instance mutex in `main.rs`)
        // posted the activate message to the overlay window, which set this
        // flag from `overlay_wnd_proc`. Reveal Settings via the same path the
        // tray menu's "Settings…" item uses -- this is the guaranteed way
        // back in even when the tray icon itself is hidden.
        if SHOW_SETTINGS_REQUESTED.swap(false, Ordering::AcqRel) {
            crate::settings_ui::show_settings(Arc::clone(&app));
        }

        // Smooth the counter toward the live word count. Asymmetric lerp:
        // fast counting up (responsive), slow counting down (damps STT
        // partial-transcript revision jitter so the pip doesn't snap back).
        if want_visible && !show_spinner {
            let rate = if target_count > display_count {
                0.50
            } else {
                0.15
            };
            display_count += (target_count - display_count) * rate;
        } else {
            display_count = 0.0;
        }
        let smooth_count = display_count.round() as u32;

        unsafe {
            if want_visible {
                let mut p = POINT::default();
                if GetCursorPos(&mut p).is_ok() {
                    let pos_changed =
                        !matches!(last_pos, Some(prev) if prev.x == p.x && prev.y == p.y);
                    let status_changed = status != last_status;
                    let count_changed = smooth_count != last_word_count;
                    let spinner_changed = show_spinner != last_spinner;
                    // The error glyph depends on the kind, so a kind flip while
                    // the status stays Error must still repaint (two back-to-back
                    // errors of different kinds within the 2s pip window).
                    let kind_changed = error_kind != last_error_kind;
                    // Render whenever anything changes — the smoothed counter
                    // changes most frames during active dictation, giving a
                    // fluid animation.
                    if pos_changed
                        || status_changed
                        || count_changed
                        || kind_changed
                        || spinner_changed
                        || show_spinner
                    {
                        overlay.render(
                            status,
                            error_kind,
                            smooth_count,
                            show_spinner.then_some(spinner_angle),
                            p.x + PIP_OFFSET_X,
                            p.y + PIP_OFFSET_Y,
                        );
                        last_pos = Some(p);
                        last_word_count = smooth_count;
                        last_error_kind = error_kind;
                    }
                }
            } else if last_status != Status::Idle || last_pos.is_some() {
                overlay.hide();
                last_pos = None;
                last_word_count = u32::MAX;
            }
        }
        last_status = status;
        last_spinner = show_spinner;
        std::thread::sleep(if want_visible {
            ACTIVE_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        });
    }
    Ok(())
}

// ===== Tray =====

/// Persist `hide_tray_icon` and hot-store it — the same write-then-`config.store`
/// the Settings window's Save does, so both entry points leave settings.json and
/// the live config in exactly one state. Nothing here touches the tray itself:
/// the poll loop in [`run`] sees the changed value on its next tick and calls
/// `set_visible`, which is also what makes the Settings checkbox apply live.
///
/// Caveat, narrower than it first looks: only a *currently visible* Settings
/// window can clobber this, because its `draft` predates our write and its Save
/// writes the whole draft back. A hidden one can't — `reseed_for_reopen` re-clones
/// the draft from live config on every reveal, which is what makes the documented
/// way back in (relaunch -> Settings reopens with this box correctly ticked) work.
/// So the residual race needs someone to ignore the checkbox sitting in front of
/// them, hide from the tray instead, then Save — and even that self-heals on the
/// next close/reopen. Not worth live-syncing one field into a deliberately
/// draft-then-Save window.
fn set_hide_tray_icon(app: &App, hide: bool) {
    let mut cfg = (**app.config.load()).clone();
    if cfg.hide_tray_icon == hide {
        return;
    }
    cfg.hide_tray_icon = hide;
    match cfg.save(&crate::config::Config::settings_path()) {
        Ok(()) => {
            app.config.store(Arc::new(cfg));
            tracing::info!("tray: hide_tray_icon set to {hide} from the tray menu");
        }
        Err(e) => tracing::warn!("tray: could not persist hide_tray_icon ({e})"),
    }
}

struct TrayState {
    tray: TrayIcon,
    history_menu: Submenu,
}

/// Max chars of a transcript to show as a menu item's label before eliding.
/// Keeps the submenu from stretching off-screen with a long dictation.
const HISTORY_LABEL_CHARS: usize = 40;

impl TrayState {
    /// Rebuild the "Recent transcriptions" submenu's items from a fresh
    /// snapshot. Called only when the history's version counter changes, so
    /// this isn't on the hot active UI-poll path in the common case.
    fn rebuild_history_menu(&self, entries: &[crate::state::HistoryEntry]) {
        // Clear whatever's there now (placeholder or stale entries).
        while self.history_menu.remove_at(0).is_some() {}

        if entries.is_empty() {
            let placeholder = MenuItem::with_id(
                MenuId::new("history:none"),
                "(no recent transcriptions)",
                false, // disabled -- informational only
                None,
            );
            let _ = self.history_menu.append(&placeholder);
            return;
        }

        // Aggregate action pinned to the top: copy every recent transcription
        // to the clipboard at once. A separator sets it apart from the tappable
        // per-entry rows below (which each copy just themselves). Newest-first,
        // matching the order the entries are listed.
        let copy_all = MenuItem::with_id(
            MenuId::new("history:copyall"),
            format!("Copy all ({})", entries.len()),
            true,
            None,
        );
        let _ = self.history_menu.append(&copy_all);
        let _ = self.history_menu.append(&PredefinedMenuItem::separator());

        for (i, entry) in entries.iter().enumerate() {
            let age = time_ago(entry.when);
            let label = format!("{} — {}", elide(&entry.text, HISTORY_LABEL_CHARS), age);
            let item = MenuItem::with_id(MenuId::new(format!("history:{i}")), label, true, None);
            let _ = self.history_menu.append(&item);
        }
    }
}

/// Coarse "how long ago" label for a history entry's timestamp, e.g. "just
/// now", "3m ago", "2h ago". No dependency on a date/time crate -- this is
/// display-only, so a rough bucket is all we need.
fn time_ago(when: std::time::SystemTime) -> String {
    let elapsed = match when.elapsed() {
        Ok(d) => d,
        Err(_) => return "just now".to_string(), // clock skew; don't show a negative age
    };
    let secs = elapsed.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Trim `s` to at most `max_chars` characters (Unicode scalar count), folding
/// any internal newlines to spaces first so a multi-line dictation still
/// reads as one tidy menu-item line.
fn elide(s: &str, max_chars: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let truncated: String = flat.chars().take(max_chars).collect();
    if flat.chars().count() > max_chars {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn build_tray() -> Result<TrayState> {
    let version = env!("CARGO_PKG_VERSION");
    let version_label = format!("QuickDictate v{version}");

    let menu = Menu::new();
    let settings = MenuItem::with_id(MenuId::new("settings"), "Settings…", true, None);
    // Grouped with "Settings…" (it's a preference, not an app action) and kept
    // clear of "Quit" so a misclick can't cost you a running session. There's
    // deliberately no checked state: hiding the icon takes this menu with it,
    // so the item can only ever be ticked *on* from here -- unhiding is the
    // Settings checkbox's job.
    let hide_icon = MenuItem::with_id(MenuId::new("hide_tray"), "Hide tray icon", true, None);
    let history_menu = Submenu::new("Recent transcriptions", true);
    let placeholder = MenuItem::with_id(
        MenuId::new("history:none"),
        "(no recent transcriptions)",
        false,
        None,
    );
    history_menu.append(&placeholder)?;
    let separator = PredefinedMenuItem::separator();
    let separator2 = PredefinedMenuItem::separator();
    let quit = MenuItem::with_id(MenuId::new("quit"), "Quit QuickDictate", true, None);
    menu.append(&settings)?;
    menu.append(&hide_icon)?;
    menu.append(&separator)?;
    menu.append(&history_menu)?;
    menu.append(&separator2)?;
    menu.append(&quit)?;

    let icon = make_icon();
    let tray = TrayIconBuilder::new()
        .with_tooltip(&version_label)
        .with_icon(icon)
        .with_menu(Box::new(menu))
        .build()?;
    Ok(TrayState { tray, history_menu })
}

fn make_icon() -> tray_icon::Icon {
    // The tray/notification variant (transparent glyph, not the filled tile — see
    // `crate::icon`), pre-scaled to 32² so Windows' notification area has a crisp
    // source to downsample from at any DPI.
    let (rgba, w, h) = crate::icon::notification_rgba(32);
    tray_icon::Icon::from_rgba(rgba, w, h).expect("tray icon")
}

// ===== Layered overlay =====

/// Owns the layered window plus an in-memory 32-bit BGRA bitmap we render
/// into and then ship to the screen via `UpdateLayeredWindow`. All fields
/// are accessed only from the UI thread.
struct Overlay {
    hwnd: HWND,
    mem_dc: HDC,
    bitmap: HBITMAP,
    pixels: *mut u32,
    size: i32,
    visible: std::cell::Cell<bool>,
}

impl Overlay {
    unsafe fn create(size: i32) -> Result<Self> {
        let class_name: Vec<u16> = OVERLAY_CLASS_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let h_module = GetModuleHandleW(PCWSTR::null())?;
        let h_instance = HINSTANCE(h_module.0);

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_wnd_proc),
            hInstance: h_instance,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            let err = windows::Win32::Foundation::GetLastError();
            if err.0 != 1410 {
                anyhow::bail!("RegisterClassExW failed: {:?}", err);
            }
        }

        // WS_EX_LAYERED is required for UpdateLayeredWindow; we deliberately
        // DO NOT call SetLayeredWindowAttributes -- the two APIs are mutually
        // exclusive on a given window.
        let ex_style =
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW;
        let style = WS_POPUP;
        let hwnd = CreateWindowExW(
            ex_style,
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            size,
            size,
            HWND::default(),
            HMENU::default(),
            h_instance,
            None,
        )?;

        // 32-bit top-down BGRA DIB section. CreateDIBSection hands us the raw
        // pixel buffer so we can write directly without GDI's rasterizer.
        let screen_dc = GetDC(HWND::default());
        let mem_dc = CreateCompatibleDC(screen_dc);
        let _ = ReleaseDC(HWND::default(), screen_dc);

        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size, // negative => top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut pixels_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut pixels_ptr, None, 0)?;
        if pixels_ptr.is_null() {
            anyhow::bail!("CreateDIBSection returned null pixel pointer");
        }
        SelectObject(mem_dc, bitmap);

        Ok(Self {
            hwnd,
            mem_dc,
            bitmap,
            pixels: pixels_ptr as *mut u32,
            size,
            visible: std::cell::Cell::new(false),
        })
    }

    unsafe fn hide(&self) {
        if self.visible.get() {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            self.visible.set(false);
        }
    }

    /// Software-render the disc + word count into the DIB, then ship it to
    /// the screen via UpdateLayeredWindow. Pixels are premultiplied BGRA as
    /// required by ULW_ALPHA.
    unsafe fn render(
        &self,
        status: Status,
        error_kind: ErrorKind,
        word_count: u32,
        spinner_angle: Option<f32>,
        screen_x: i32,
        screen_y: i32,
    ) {
        let total = (self.size * self.size) as usize;
        let pixels = std::slice::from_raw_parts_mut(self.pixels, total);
        // Clear to fully transparent.
        for p in pixels.iter_mut() {
            *p = 0;
        }

        // Disc color picked by status. Values are (R, G, B).
        let (r, g, b) = match status {
            Status::Idle => return, // window will be hidden; nothing to draw
            Status::Starting => (0xFA, 0xB0, 0x05), // amber
            Status::Listening => (0x22, 0xC5, 0x5E), // green
            Status::Processing => (0x4A, 0x90, 0xF5), // blue
            Status::Error => (0xEF, 0x44, 0x44), // red
        };
        let cx = (self.size as f32 - 1.0) / 2.0;
        let cy = (self.size as f32 - 1.0) / 2.0;
        // Leave 1 px gutter so the soft edge doesn't get cropped by the window.
        let radius_outer = (self.size as f32 / 2.0) - 1.0;
        // 1 px feather for the anti-aliased edge.
        let edge = 1.0_f32;

        for y in 0..self.size {
            for x in 0..self.size {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let dist = (dx * dx + dy * dy).sqrt();
                // Smooth alpha: 1.0 inside, ramps to 0 across `edge` pixels at the rim.
                let a = ((radius_outer - dist) / edge).clamp(0.0, 1.0);
                if a == 0.0 {
                    continue;
                }
                let alpha = (a * 255.0 + 0.5) as u32;
                // Premultiplied BGRA: BB GG RR AA stored as 0xAARRGGBB on
                // little-endian (which Windows expects).
                let pr = ((r as f32) * a + 0.5) as u32;
                let pg = ((g as f32) * a + 0.5) as u32;
                let pb = ((b as f32) * a + 0.5) as u32;
                let idx = (y * self.size + x) as usize;
                pixels[idx] = (alpha << 24) | (pr << 16) | (pg << 8) | pb;
            }
        }

        if let Some(start_angle) = spinner_angle {
            // Local providers are batch-only, so a word count sits at zero
            // until the final transcript. Draw a rotating 270° ring instead.
            // Blend it directly into the already-opaque disc so the layered
            // window keeps correct premultiplied alpha at antialiased edges.
            let ring_radius = self.size as f32 * 0.22;
            let ring_half_width = self.size as f32 * 0.034;
            let sweep = std::f32::consts::TAU * 0.75;
            for y in 0..self.size {
                for x in 0..self.size {
                    let dx = x as f32 - cx;
                    let dy = y as f32 - cy;
                    let dist = (dx * dx + dy * dy).sqrt();
                    let radial =
                        (ring_half_width + 0.8 - (dist - ring_radius).abs()).clamp(0.0, 1.0);
                    if radial == 0.0 {
                        continue;
                    }
                    let around = (dy.atan2(dx) - start_angle).rem_euclid(std::f32::consts::TAU);
                    if around > sweep {
                        continue;
                    }
                    let tip = (around.min(sweep - around) / 0.16).clamp(0.0, 1.0);
                    let coverage = radial * tip;
                    let idx = (y * self.size + x) as usize;
                    let old = pixels[idx];
                    let blend = |channel: u32| -> u32 {
                        (channel as f32 + (255.0 - channel as f32) * coverage + 0.5) as u32
                    };
                    let red = blend((old >> 16) & 0xff);
                    let green = blend((old >> 8) & 0xff);
                    let blue = blend(old & 0xff);
                    pixels[idx] = (old & 0xff00_0000) | (red << 16) | (green << 8) | blue;
                }
            }
        } else {
            // Draw the label on top. GDI doesn't touch the alpha channel, but
            // the disc interior already has alpha=255, so text stays opaque.
            let (label, face_name, height_factor) = match status {
                Status::Error if error_kind == ErrorKind::DeadKeys => {
                    ("\u{E8D7}".to_string(), "Segoe MDL2 Assets\0", 0.52)
                }
                Status::Error => ("!".to_string(), "Segoe UI\0", 0.45),
                _ => (format!("{word_count}"), "Segoe UI\0", 0.45),
            };
            let mut label_utf16: Vec<u16> = label.encode_utf16().collect();
            let font_height = (self.size as f32 * height_factor) as i32;
            let face: Vec<u16> = face_name.encode_utf16().collect();
            let font = CreateFontW(
                -font_height,
                0,
                0,
                0,
                FW_BOLD.0 as i32,
                0u32,
                0u32,
                0u32,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                ANTIALIASED_QUALITY.0 as u32,
                (VARIABLE_PITCH.0 as u32) | (FF_DONTCARE.0 as u32),
                PCWSTR(face.as_ptr()),
            );
            let old_font = SelectObject(self.mem_dc, font);
            let _ = SetBkMode(self.mem_dc, TRANSPARENT);

            // Drop shadow: 1 px down-right, black.
            let _ = SetTextColor(self.mem_dc, COLORREF(0x00000000));
            let mut shadow_rect = RECT {
                left: 1,
                top: 1,
                right: self.size + 1,
                bottom: self.size + 1,
            };
            DrawTextW(
                self.mem_dc,
                &mut label_utf16,
                &mut shadow_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
            // Main text: white.
            let _ = SetTextColor(self.mem_dc, COLORREF(0x00FFFFFF));
            let mut text_rect = RECT {
                left: 0,
                top: 0,
                right: self.size,
                bottom: self.size,
            };
            DrawTextW(
                self.mem_dc,
                &mut label_utf16,
                &mut text_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
            SelectObject(self.mem_dc, old_font);
            let _ = DeleteObject(font);
        }

        // Ship the bitmap to the screen, also moving the window.
        // UpdateLayeredWindow won't reveal a hidden window -- it only updates
        // an already-visible one. So show it on first use.
        let pt_dst = POINT {
            x: screen_x,
            y: screen_y,
        };
        let sz = SIZE {
            cx: self.size,
            cy: self.size,
        };
        let pt_src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        if let Err(e) = UpdateLayeredWindow(
            self.hwnd,
            HDC::default(),
            Some(&pt_dst),
            Some(&sz),
            self.mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        ) {
            tracing::warn!("UpdateLayeredWindow failed: {e}");
        }
        if !self.visible.get() {
            // SW_SHOWNA shows without stealing focus.
            let _ = ShowWindow(self.hwnd, SW_SHOWNA);
            self.visible.set(true);
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.bitmap);
            let _ = DeleteDC(self.mem_dc);
        }
    }
}

unsafe extern "system" fn overlay_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        // A second QuickDictate launch found us via FindWindowW (see
        // `main.rs`'s single-instance guard) and posted the registered
        // activate message. We can't thread the `App`/`Arc` through a raw
        // wnd_proc, so just flip a flag the poll loop in `run` picks up and
        // acts on via the normal `settings_ui::show_settings` path.
        m if m == activate_message_id() => {
            SHOW_SETTINGS_REQUESTED.store(true, Ordering::Release);
            LRESULT(0)
        }
        // No WM_PAINT handler: UpdateLayeredWindow drives all visuals.
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
