//! Turning one win32 hotkey message into a HotkeyEvent, and the pollers that
//! watch for a long press or a key release the message loop never sees.

use std::thread;
use std::time::Duration;

use crossbeam_channel::Sender;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{MSG, WM_HOTKEY, WM_TIMER};

use crate::mouse_hook::{self};

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
    if msg.message == WM_TIMER {
        if watch::is_watch_timer(msg.wParam.0) {
            for dropped in watch::on_timer() {
                act_on_dropped(&dropped, b, tx);
            }
        } else {
            rearm_hotkeys(b);
        }
    } else if msg.message == WM_HOTKEY {
        let id = msg.wParam.0 as i32;
        if watch::on_hotkey(id) {
            dispatch_hotkey_press(id, b, tx);
        }
    } else if msg.message == watch::WM_WATCH_KEY_UP {
        watch::send_key_up(msg.wParam.0 as u32);
    }
}

/// The periodic re-arm tick: re-register every keyboard binding, reinstall
/// the mouse hook, and feed the outcome to `hotkeys_blocked()`.
fn rearm_hotkeys(b: &HotkeyBindings<'_>) {
    let mut all_registered = true;
    for (id, binding) in [(b.toggle_id, b.kb_toggle), (b.hold_id, b.kb_hold)] {
        if let Some((combo, mods, vk)) = binding {
            let registered = unsafe { register_one(id, combo, *mods, *vk, true) };
            watch::set_registered(id, registered);
            all_registered &= registered;
        }
    }
    // The watch is a backstop, so a hook that will not install is logged but
    // does not count as a blocked hotkey.
    watch::rearm();
    if b.has_mouse {
        // Windows silently removes a low-level hook that overruns
        // LowLevelHooksTimeout, so the mouse side needs the same
        // periodic re-arm the keyboard side gets. The hook is reinstalled
        // every time, because a removed hook cannot be told from a live one.
        all_registered &= mouse_hook::ensure_installed();
    }
    note_rearm_result(all_registered);
    tracing::debug!("hotkeys re-armed");
}

/// Turn one `WM_HOTKEY` (the binding `id` fired) into its press event, and
/// start the poller that reports the long press or release Windows never
/// sends a message for.
fn dispatch_hotkey_press(id: i32, b: &HotkeyBindings<'_>, tx: &Sender<HotkeyEvent>) {
    tracing::info!("WM_HOTKEY received: id={id}");
    // Only a keyboard binding can produce WM_HOTKEY; a mouse binding drives
    // its own press/release/long-press entirely inside the hook, so the
    // pollers here stay on the keyboard vk they were written for.
    if id == b.toggle_id {
        let _ = tx.send(HotkeyEvent::TogglePressed);
        if let Some((_, _, vk)) = b.kb_toggle {
            spawn_long_press_poller(*vk, tx.clone(), b.reinsert_hold_duration);
        }
    } else if id == b.hold_id {
        let _ = tx.send(HotkeyEvent::HoldPressed);
        if let Some((_, _, vk)) = b.kb_hold {
            spawn_release_poller(*vk, tx.clone());
        }
    }
}

/// Floor between the warnings for presses left alone (see [`act_on_dropped`]),
/// so a Remote Desktop window that takes the hotkey key on every press does
/// not fill the log.
const LEFT_ALONE_WARN_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// When a left-alone press was last logged, and how many since went unlogged.
static LEFT_ALONE_WARN: parking_lot::Mutex<(Option<Instant>, u32)> =
    parking_lot::Mutex::new((None, 0));

/// Act on a press Windows sent no `WM_HOTKEY` for (see [`watch`]): log it,
/// register the binding again, clear the key once it is up, and, only when
/// Windows had the key stuck down, send what the `WM_HOTKEY` would have.
///
/// A handled toggle press gets no long-press poller: that reads the key state
/// Windows just showed it cannot be trusted with, and a false long press would
/// paste the last dictation again. A handled hold press ends on the key-up the
/// watch sees.
fn act_on_dropped(dropped: &watch::Dropped, b: &HotkeyBindings<'_>, tx: &Sender<HotkeyEvent>) {
    let binding = if dropped.id == b.toggle_id {
        b.kb_toggle
    } else {
        b.kb_hold
    };
    let combo = binding.map_or("?", |(combo, _, _)| combo.as_str());
    if dropped.handle {
        tracing::warn!(
            "hotkey {combo}: Windows still counted the key as down, so it sent no \
             WM_HOTKEY for this press; handling the press here, registering the hotkey \
             again and clearing the key"
        );
    } else {
        let mut last = LEFT_ALONE_WARN.lock();
        if last
            .0
            .is_none_or(|t| t.elapsed() >= LEFT_ALONE_WARN_INTERVAL)
        {
            tracing::warn!(
                "hotkey {combo}: the key reached QuickDictate but Windows sent no WM_HOTKEY \
                 ({} more like it since the last line). Another program may have taken it \
                 (Remote Desktop, a key remapper, a game), so the press is left alone; \
                 registering the hotkey again and clearing the key in case Windows lost it",
                last.1
            );
            *last = (Some(Instant::now()), 0);
        } else {
            last.1 += 1;
        }
    }
    if let Some((combo, mods, vk)) = binding {
        let registered = unsafe { register_one(dropped.id, combo, *mods, *vk, true) };
        watch::set_registered(dropped.id, registered);
    }
    if dropped.handle {
        if dropped.hold {
            let _ = tx.send(HotkeyEvent::HoldPressed);
            if !dropped.still_down {
                let _ = tx.send(HotkeyEvent::HoldReleased);
            }
        } else {
            let _ = tx.send(HotkeyEvent::TogglePressed);
        }
    }
    if !dropped.still_down {
        watch::send_key_up(dropped.vk);
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
