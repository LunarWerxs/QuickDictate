//! Claiming the hotkeys from Windows and keeping them claimed.
//!
//! RegisterHotKey can lose a binding to another app at any time, so this also
//! owns the periodic re-arm and the message loop it runs on.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::Sender;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, KillTimer, SetTimer, MSG};

use crate::mouse_hook::{self, is_mouse_vk, MouseBinding};

use super::*;

/// One configured hotkey as parsed: the combo as written, its modifiers and
/// its virtual-key code.
type ParsedCombo = (String, u32, u32);

/// (Re)register one hotkey. `quiet` suppresses the per-registration log line
/// (used by the periodic re-arm so the log isn't spammed every minute).
pub(super) unsafe fn register_one(id: i32, combo: &str, mods: u32, vk: u32, quiet: bool) -> bool {
    let null_hwnd = windows::Win32::Foundation::HWND::default();
    // Drop any stale registration first; failure here just means there wasn't
    // one (fresh start), which is fine.
    let _ = UnregisterHotKey(null_hwnd, id);
    match RegisterHotKey(null_hwnd, id, HOT_KEY_MODIFIERS(mods), vk) {
        Ok(()) => {
            if !quiet {
                tracing::info!(
                    "Registered {} hotkey {combo} (vk=0x{vk:02X})",
                    if id == 1 { "toggle" } else { "hold" }
                );
            }
            true
        }
        Err(e) => {
            if !quiet {
                tracing::warn!("RegisterHotKey({combo}) failed: {e} (will retry on next re-arm)");
            }
            false
        }
    }
}

/// Best-effort *initial* registration of the configured hotkeys. Retries any
/// that fail -- typically because a just-replaced instance of ourselves (Save
/// & Restart, or the self-updater) is still holding the global hotkey -- for
/// up to [`STARTUP_REGISTER_BUDGET`]. Deliberately **never fatal**: whatever
/// isn't registered by the deadline is left to the periodic re-arm in the
/// message loop, so the hotkeys self-heal within a minute instead of the whole
/// thread dying and leaving the app hotkey-dead until the next manual restart
/// (the pre-fix behavior). Attempts are quiet; this fn owns the summary logs.
fn register_initial(
    toggle_id: i32,
    toggle: Option<&ParsedCombo>,
    hold_id: i32,
    hold: Option<&ParsedCombo>,
) {
    let deadline = Instant::now() + STARTUP_REGISTER_BUDGET;
    let mut toggle_done = toggle.is_none();
    let mut hold_done = hold.is_none();
    let mut retried = false;
    loop {
        register_if_pending(&mut toggle_done, toggle_id, toggle, "toggle");
        register_if_pending(&mut hold_done, hold_id, hold, "hold");
        if (toggle_done && hold_done) || Instant::now() >= deadline {
            break;
        }
        retried = true;
        std::thread::sleep(Duration::from_millis(STARTUP_REGISTER_RETRY_MS));
    }
    if !toggle_done || !hold_done {
        tracing::warn!(
            "hotkey(s) still not registered after {}s (another process holding them?); \
             the periodic re-arm will keep trying",
            STARTUP_REGISTER_BUDGET.as_secs()
        );
    } else if retried {
        tracing::info!("hotkeys registered after a brief retry (handoff from previous instance)");
    }
}

/// One `register_initial` attempt for one binding: register it unless it is
/// already `done` (or not configured), and log the success that attempt owns.
fn register_if_pending(done: &mut bool, id: i32, binding: Option<&ParsedCombo>, label: &str) {
    if *done {
        return;
    }
    if let Some((combo, mods, vk)) = binding {
        if unsafe { register_one(id, combo, *mods, *vk, true) } {
            *done = true;
            tracing::info!("Registered {label} hotkey {combo} (vk=0x{vk:02X})");
        }
    }
}

/// Parse one configured combo into `(combo, mods, vk)`, parsed once so the
/// periodic re-arm can re-register without re-parsing. `None` or empty means
/// the hotkey is not configured.
fn parse_binding(combo: Option<&str>) -> Result<Option<ParsedCombo>> {
    let Some(combo) = combo.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let (mods, vk) = parse_combo(combo)?;
    Ok(Some((combo.to_string(), mods, vk)))
}

/// The configured bindings that name a mouse button, as the hook wants them.
fn mouse_bindings_for(bindings: [(i32, Option<&ParsedCombo>); 2]) -> Vec<MouseBinding> {
    bindings
        .into_iter()
        .filter_map(|(id, binding)| {
            let (_, mods, vk) = binding?;
            is_mouse_vk(*vk).then_some(MouseBinding {
                id,
                vk: *vk,
                // MOD_NOREPEAT is a RegisterHotKey concept with no meaning to
                // a hook; strip it so the hook's exact-match compare works.
                mods: *mods & !MOD_NOREPEAT.0,
            })
        })
        .collect()
}

/// Hand the mouse bindings to the hook and install it. Must run on the
/// hotkey thread: see the comment at the install call.
fn start_mouse_hook(
    mouse_bindings: Vec<MouseBinding>,
    configured: [Option<&ParsedCombo>; 2],
    b: &HotkeyBindings<'_>,
    mouse_passthrough: bool,
    tx: &Sender<HotkeyEvent>,
) {
    for (combo, _, vk) in configured.into_iter().flatten() {
        if is_mouse_vk(*vk) {
            tracing::info!("Binding mouse hotkey {combo} (vk=0x{vk:02X})");
        }
    }
    mouse_hook::configure(
        mouse_bindings,
        tx.clone(),
        mouse_passthrough,
        b.reinsert_hold_duration,
        b.toggle_id,
        b.hold_id,
    );
    // Installed from *this* thread on purpose: Windows dispatches a
    // low-level hook's callback onto the installing thread while it pumps
    // messages, and the loop below is that pump.
    mouse_hook::ensure_installed();
}

/// Pump this thread's messages until shutdown or `WM_QUIT`, handing each one
/// to `dispatch_hotkey_message`.
fn pump_hotkey_messages(b: &HotkeyBindings<'_>, tx: &Sender<HotkeyEvent>, stop_flag: &AtomicBool) {
    let mut msg = MSG::default();
    loop {
        if stop_flag.load(Ordering::Acquire) {
            break;
        }
        let got =
            unsafe { GetMessageW(&mut msg, windows::Win32::Foundation::HWND::default(), 0, 0).0 };
        if got <= 0 {
            break;
        } // 0 = WM_QUIT, -1 = error

        dispatch_hotkey_message(&msg, b, tx);
    }
}

/// Undo what `run_hotkey_loop` claimed: the re-arm timer, the keyboard
/// registrations and the mouse hook.
fn release_hotkeys(b: &HotkeyBindings<'_>, rearm_timer: usize) {
    unsafe {
        let null_hwnd = windows::Win32::Foundation::HWND::default();
        if rearm_timer != 0 {
            let _ = KillTimer(null_hwnd, rearm_timer);
        }
        if b.kb_toggle.is_some() {
            let _ = UnregisterHotKey(null_hwnd, b.toggle_id);
        }
        if b.kb_hold.is_some() {
            let _ = UnregisterHotKey(null_hwnd, b.hold_id);
        }
    }
    // Drop the hook before we return, so a replacement process (Save &
    // Restart, or the self-updater) isn't racing a stale hook of ours for the
    // same buttons.
    if b.has_mouse {
        mouse_hook::uninstall();
    }
}

pub(super) fn run_hotkey_loop(
    toggle_combo: Option<String>,
    hold_combo: Option<String>,
    reinsert_hold_duration: Duration,
    mouse_passthrough: bool,
    tx: Sender<HotkeyEvent>,
    stop_flag: Arc<AtomicBool>,
) -> Result<()> {
    let toggle_id = 1i32;
    let hold_id = 2i32;

    // Parse the combos up front. A parse error is a genuine config mistake (a
    // bad key name) and stays fatal; an OS *registration* failure below does
    // NOT abort us -- see `register_initial`.
    let toggle = parse_binding(toggle_combo.as_deref())?;
    let hold = parse_binding(hold_combo.as_deref())?;

    // Two mechanisms, split by what the binding actually is. `RegisterHotKey`
    // is keyboard-only -- it cannot express a mouse button at all -- so a
    // mouse-bound hotkey goes to the low-level hook in `mouse_hook` instead.
    // Everything downstream (the event enum, the ids, main's handling) is
    // identical either way; only the acquisition differs.
    let kb_toggle = toggle.as_ref().filter(|(_, _, vk)| !is_mouse_vk(*vk));
    let kb_hold = hold.as_ref().filter(|(_, _, vk)| !is_mouse_vk(*vk));
    let mouse_bindings =
        mouse_bindings_for([(toggle_id, toggle.as_ref()), (hold_id, hold.as_ref())]);
    let bindings = HotkeyBindings {
        toggle_id,
        hold_id,
        kb_toggle,
        kb_hold,
        has_mouse: !mouse_bindings.is_empty(),
        reinsert_hold_duration,
    };

    register_initial(toggle_id, kb_toggle, hold_id, kb_hold);

    if bindings.has_mouse {
        start_mouse_hook(
            mouse_bindings,
            [toggle.as_ref(), hold.as_ref()],
            &bindings,
            mouse_passthrough,
            &tx,
        );
    }

    // Self-healing re-arm: RegisterHotKey bindings can silently die across
    // sleep/resume, session lock, and RDP reconnects. A thread-queue timer
    // (no window needed) re-registers both hotkeys every REARM_INTERVAL_MS.
    let rearm_timer = unsafe {
        SetTimer(
            windows::Win32::Foundation::HWND::default(),
            0,
            REARM_INTERVAL_MS,
            None,
        )
    };

    pump_hotkey_messages(&bindings, &tx, &stop_flag);
    release_hotkeys(&bindings, rearm_timer);
    Ok(())
}
