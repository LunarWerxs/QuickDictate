//! The notification-area icon and its menu: building the tray, keeping the
//! history submenu current, and dispatching the win32 messages and menu
//! events the user's clicks arrive as.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, IDYES, MB_ICONQUESTION, MB_YESNO, MSG,
    PM_REMOVE, WM_QUIT,
};

use crate::state::App;

use super::*;

/// Drain and dispatch the win32 message queue for the hidden overlay window
/// (tray clicks, menu commands, the second-launch activate message). Also
/// the mechanism that notices `WM_QUIT` and starts shutdown.
pub(super) fn pump_win32_messages(app: &App) {
    let mut msg = MSG::default();
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
}

/// "Copy all (N)": concatenate every recent transcription and put the whole
/// batch on the clipboard in one go. Joined oldest-first so the newest lands
/// at the bottom (the way a transcript reads and where the eye naturally
/// goes), even though the menu itself lists them newest-first. A blank line
/// between entries keeps each dictation a distinct paragraph when pasted.
fn handle_history_copy_all(app: &App) {
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
        return;
    }
    let joined = all.join("\n\n");
    let n = all.len();
    match crate::output::copy_to_clipboard(&joined) {
        Ok(()) => tracing::info!(
            "Recent transcriptions: copied all {n} entries ({} chars) to clipboard",
            joined.chars().count()
        ),
        Err(e) => tracing::warn!("Recent transcriptions: 'Copy all' clipboard copy failed: {e:#}"),
    }
}

/// Clicking a recent transcription copies it to the clipboard (the user
/// pastes it themselves) rather than auto-pasting into the focused window.
/// `idx` is `id` with the `"history:"` prefix already stripped.
fn handle_history_click(app: &App, id: &str, idx: &str) {
    let i: usize = match idx.parse() {
        Ok(i) => i,
        Err(_) => {
            tracing::warn!("bad history menu id: {id}");
            return;
        }
    };
    let entry = app.history.lock().get(i);
    match entry {
        Some(entry) if !entry.text.is_empty() => {
            match crate::output::copy_to_clipboard(&entry.text) {
                Ok(()) => tracing::info!(
                    "Recent transcriptions: copied entry {i} ({} chars) to clipboard",
                    entry.text.chars().count()
                ),
                Err(e) => tracing::warn!("Recent transcriptions: clipboard copy failed: {e:#}"),
            }
        }
        _ => tracing::warn!("Recent transcriptions: entry {i} is missing or empty"),
    }
}

/// Confirm first: this removes the only *visible* way back into Settings, so
/// the dialog is the one place we can spell out the way back (the tray-icon
/// crate has no per-item tooltip, unlike the Settings checkbox this
/// mirrors). Runs on its own thread so the modal doesn't stall the active
/// poll loop and freeze the pip mid-dictation.
fn handle_hide_tray_menu_click(app: &Arc<App>) {
    let app = Arc::clone(app);
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
}

/// Drain pending tray/menu events for this tick and dispatch each to its
/// handler. The tray is intentionally minimal -- Settings, Hide tray icon,
/// Recent transcriptions, and Quit. About / updates / log / JSON editing all
/// live inside the Settings window now.
pub(super) fn drain_menu_events(app: &Arc<App>) {
    let menu_rx = MenuEvent::receiver();
    while let Ok(ev) = menu_rx.try_recv() {
        let id = ev.id().as_ref();
        if ev.id() == &MenuId::new("settings") {
            crate::settings_ui::show_settings(Arc::clone(app));
        } else if ev.id() == &MenuId::new("hide_tray") {
            handle_hide_tray_menu_click(app);
        } else if ev.id() == &MenuId::new("quit") {
            tracing::info!("Quit selected from tray menu");
            app.shutdown.store(true, Ordering::Release);
        } else if ev.id() == &MenuId::new("history:copyall") {
            handle_history_copy_all(app);
        } else if let Some(idx) = id.strip_prefix("history:") {
            handle_history_click(app, id, idx);
        }
    }
}

/// A second launch (blocked by the single-instance mutex in `main.rs`)
/// posted the activate message to the overlay window, which set this flag
/// from `overlay_wnd_proc`. Reveal Settings via the same path the tray
/// menu's "Settings…" item uses -- this is the guaranteed way back in even
/// when the tray icon itself is hidden.
pub(super) fn check_show_settings_request(app: &Arc<App>) {
    if SHOW_SETTINGS_REQUESTED.swap(false, Ordering::AcqRel) {
        crate::settings_ui::show_settings(Arc::clone(app));
    }
}

pub(super) struct TrayState {
    pub(super) tray: TrayIcon,
    pub(super) history_menu: Submenu,
}

/// Max chars of a transcript to show as a menu item's label before eliding.
/// Keeps the submenu from stretching off-screen with a long dictation.
const HISTORY_LABEL_CHARS: usize = 40;

impl TrayState {
    /// Rebuild the "Recent transcriptions" submenu's items from a fresh
    /// snapshot. Called only when the history's version counter changes, so
    /// this isn't on the hot active UI-poll path in the common case.
    pub(super) fn rebuild_history_menu(&self, entries: &[crate::state::HistoryEntry]) {
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

pub(super) fn build_tray() -> Result<TrayState> {
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

#[allow(
    clippy::expect_used,
    reason = "the bytes are include_bytes! of a committed PNG, and embedded_artwork_decodes forces this path in the test suite"
)]
fn make_icon() -> tray_icon::Icon {
    // The tray/notification variant (transparent glyph, not the filled tile — see
    // `crate::icon`), pre-scaled to 32² so Windows' notification area has a crisp
    // source to downsample from at any DPI.
    let (rgba, w, h) = crate::icon::notification_rgba(32);
    tray_icon::Icon::from_rgba(rgba, w, h).expect("tray icon")
}
