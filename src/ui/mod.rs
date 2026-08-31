//! Tray icon + cursor-following status pip.
//!
//! The pip is a layered window rendered via `UpdateLayeredWindow` with a
//! 32-bit premultiplied-alpha DIB. That gives us a *real* anti-aliased
//! circle (and anti-aliased text) -- the previous `Ellipse` + `LWA_COLORKEY`
//! approach could only produce 1-bit alpha, which read as a chunky octagon.

mod loop_state;
mod overlay;
mod tray;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;

use crate::state::App;

use loop_state::*;
use overlay::*;
use tray::*;

const PIP_SIZE: i32 = 48;
const PIP_OFFSET_X: i32 = 18;
const PIP_OFFSET_Y: i32 = 18;
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(16);
/// Idle-loop wait. The loop does not merely sleep this long: it blocks in
/// `MsgWaitForMultipleObjectsEx` with this as a TIMEOUT, waking instantly for
/// any win32 message (tray click, menu command, the second-launch activate
/// request) AND for the wake event that every status transition signals (see
/// `App::wake_ui`). So the pip lights the moment the hotkey is pressed and the
/// tray menu opens the moment it is clicked, while a genuinely quiet app parks
/// for a second at a time.
///
/// The distinction was learned the hard way, twice in one evening: widening a
/// plain sleep from 100ms to 1s cut idle wakeups roughly 10x (about 86k/day
/// instead of 864k/day) but lagged the cursor pip behind the hotkey, and
/// fixing THAT with only a wake channel still lagged tray clicks, because the
/// win32 message pump lives in this same loop. The timeout is a backstop for
/// unsignalled changes (a config flag, history growth), not the mechanism for
/// responsiveness.
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// How long the main loop waits before its next pass: the fast active
/// interval while the pip needs to track the cursor smoothly for a real
/// dictation, the slow idle interval otherwise. When idle this is a TIMEOUT on
/// the wake channel rather than a sleep, so a status change still lands
/// immediately. Pure so it's unit-testable without standing up a window or an
/// `App`.
fn poll_interval(active_visible: bool) -> Duration {
    if active_visible {
        ACTIVE_POLL_INTERVAL
    } else {
        IDLE_POLL_INTERVAL
    }
}

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

#[allow(
    clippy::expect_used,
    reason = "a thread that cannot be spawned at startup is unrecoverable; the panic message is the only diagnostic there is"
)]
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
    let mut state = UiLoopState::new(&app)?;

    loop {
        if app.shutdown.load(Ordering::Acquire) {
            break;
        }

        pump_win32_messages(&app);
        drain_menu_events(&app);
        state.refresh_history_menu(&app);

        let tick = compute_tick_snapshot(&app);
        state.update_spinner_angle(tick.show_spinner);
        state.sync_error_tooltip(&tick);
        state.sync_hotkey_tooltip(&tick);
        state.sync_update_tooltip();
        state.apply_hide_tray_icon_live(&tick);
        check_show_settings_request(&app);
        state.update_display_count(&tick);
        state.render_pip_or_hide(&tick);
        state.wait_for_next_tick(&tick);
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
