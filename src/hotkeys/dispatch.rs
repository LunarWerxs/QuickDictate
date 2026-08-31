//! Turning one win32 hotkey message into a HotkeyEvent, and the pollers that
//! watch for a long press or a key release the message loop never sees.

use std::thread;
use std::time::Duration;

use crossbeam_channel::Sender;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{MSG, WM_HOTKEY, WM_TIMER};

use crate::mouse_hook::{self};

/// How often the loop re-registers its hotkeys. `RegisterHotKey` bindings can
/// silently die across sleep/resume, session lock/unlock, RDP reconnects, and
/// display changes; periodically re-arming them (SageThumbs-style self-healing)
use super::*;

/// Everything `dispatch_hotkey_message` needs that does NOT change between
/// messages: the registered binding ids, the two optional keyboard combos, the
/// mouse flag and the long-press duration.
///
/// Passing these as eight loose parameters tripped `clippy::too_many_arguments`
/// (8/7), which the pre-push gate treats as an error. Bundling them is not
/// merely lint appeasement - it says the true thing about the split: seven of
/// the eight were loop-INVARIANT config, and only `msg` varies per iteration.
pub(super) struct HotkeyBindings<'a> {
    pub(super) toggle_id: i32,
    pub(super) hold_id: i32,
    pub(super) kb_toggle: Option<&'a (String, u32, u32)>,
    pub(super) kb_hold: Option<&'a (String, u32, u32)>,
    pub(super) has_mouse: bool,
    pub(super) reinsert_hold_duration: Duration,
}

/// Handle one message pumped out of `run_hotkey_loop`'s `GetMessageW` loop:
/// either the periodic re-arm timer or a real `WM_HOTKEY` press. Split out
/// as its own function purely to keep the loop's cognitive load down; the
/// behavior is identical to having it inline.
pub(super) fn dispatch_hotkey_message(msg: &MSG, b: &HotkeyBindings<'_>, tx: &Sender<HotkeyEvent>) {
    let HotkeyBindings {
        toggle_id,
        hold_id,
        kb_toggle,
        kb_hold,
        has_mouse,
        reinsert_hold_duration,
    } = *b;
    if msg.message == WM_TIMER {
        let mut all_registered = true;
        unsafe {
            if let Some((combo, mods, vk)) = kb_toggle {
                all_registered &= register_one(toggle_id, combo, *mods, *vk, true);
            }
            if let Some((combo, mods, vk)) = kb_hold {
                all_registered &= register_one(hold_id, combo, *mods, *vk, true);
            }
        }
        if has_mouse {
            // Windows silently removes a low-level hook that overruns
            // LowLevelHooksTimeout, so the mouse side needs the same
            // periodic re-arm the keyboard side gets. No-op when the hook
            // is still live.
            all_registered &= mouse_hook::ensure_installed();
        }
        note_rearm_result(all_registered);
        tracing::debug!("hotkeys re-armed");
        return;
    }
    if msg.message != WM_HOTKEY {
        return;
    }
    let id = msg.wParam.0 as i32;
    tracing::info!("WM_HOTKEY received: id={id}");
    // Only a keyboard binding can produce WM_HOTKEY; a mouse binding drives
    // its own press/release/long-press entirely inside the hook, so the
    // pollers here stay on the keyboard vk they were written for.
    if id == toggle_id {
        let _ = tx.send(HotkeyEvent::TogglePressed);
        if let Some((_, _, vk)) = kb_toggle {
            spawn_long_press_poller(*vk, tx.clone(), reinsert_hold_duration);
        }
    } else if id == hold_id {
        let _ = tx.send(HotkeyEvent::HoldPressed);
        if let Some((_, _, vk)) = kb_hold {
            spawn_release_poller(*vk, tx.clone());
        }
    }
}

pub(super) fn spawn_long_press_poller(vk: u32, tx: Sender<HotkeyEvent>, hold_duration: Duration) {
    thread::spawn(move || {
        let key = vk as i32;
        let deadline = std::time::Instant::now() + hold_duration;
        loop {
            let state = unsafe { GetAsyncKeyState(key) };
            if (state as u16 & 0x8000) == 0 {
                return;
            }
            if std::time::Instant::now() >= deadline {
                let _ = tx.send(HotkeyEvent::ToggleLongPressed);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
}

fn spawn_release_poller(vk: u32, tx: Sender<HotkeyEvent>) {
    thread::spawn(move || {
        // Wait for the key to go up. GetAsyncKeyState high bit set => currently pressed.
        let key = vk as i32;
        loop {
            let state = unsafe { GetAsyncKeyState(key) };
            // High bit (0x8000) indicates key is currently down.
            if (state as u16 & 0x8000) == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let _ = tx.send(HotkeyEvent::HoldReleased);
    });
}
