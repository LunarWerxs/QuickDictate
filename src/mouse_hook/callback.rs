//! The low-level hook procedure and the state machine it drives.
//!
//! Everything here runs on the hotkey thread inside Windows' hook callback,
//! under the `LowLevelHooksTimeout` budget described in the module docs: no
//! I/O, no blocking lock, no allocation beyond the one `Arc` clone the config
//! load costs. The functions are split fine enough that the press/release
//! pairing can be tested without a mouse — see the sibling `tests` module,
//! which drives [`handle_down`] / [`handle_up`] directly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, LLMHF_INJECTED, MSLLHOOKSTRUCT, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

use crate::hotkeys::HotkeyEvent;

use super::*;

/// True if `vk` is currently held down, per the async key state.
fn key_down(vk: u32) -> bool {
    (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
}

/// The modifier keys held right now, in `HOT_KEY_MODIFIERS` bit terms. A
/// low-level mouse hook carries no modifier state of its own, so we sample it.
fn current_modifiers() -> u32 {
    const VK_SHIFT: u32 = 0x10;
    const VK_CONTROL: u32 = 0x11;
    const VK_MENU: u32 = 0x12;
    const VK_LWIN: u32 = 0x5B;
    const VK_RWIN: u32 = 0x5C;
    let mut mods = 0u32;
    if key_down(VK_CONTROL) {
        mods |= MOD_CONTROL.0;
    }
    if key_down(VK_MENU) {
        mods |= MOD_ALT.0;
    }
    if key_down(VK_SHIFT) {
        mods |= MOD_SHIFT.0;
    }
    if key_down(VK_LWIN) || key_down(VK_RWIN) {
        mods |= MOD_WIN.0;
    }
    mods
}

/// Decode a low-level mouse message into `(vk, is_down)` for the three
/// bindable buttons, or `None` for everything else (moves, wheel, left/right).
pub(super) fn decode(message: u32, mouse_data: u32) -> Option<(u32, bool)> {
    match message {
        WM_MBUTTONDOWN => Some((VK_MBUTTON, true)),
        WM_MBUTTONUP => Some((VK_MBUTTON, false)),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            // Which thumb button lives in the HIGH word of mouseData.
            let vk = match (mouse_data >> 16) as u16 {
                XBUTTON1 => VK_XBUTTON1,
                XBUTTON2 => VK_XBUTTON2,
                _ => return None,
            };
            Some((vk, message == WM_XBUTTONDOWN))
        }
        _ => None,
    }
}

/// A button went down. Returns true if the press should be **suppressed**
/// (not passed to the rest of the system).
///
/// Takes the held modifiers as an argument rather than sampling them itself,
/// so the decision is a pure function of its inputs — the callback owns the
/// ambient `GetAsyncKeyState` read, and this stays testable without a mouse.
pub(super) fn handle_down(vk: u32, mods: u32) -> bool {
    let Some(slot) = slot_of(vk) else {
        return false;
    };
    let Some(cfg) = CONFIG.load_full() else {
        return false;
    };

    // Any state left over from a press whose release we never saw (the hook
    // was reinstalled mid-press, the session locked, etc.) is stale the moment
    // a fresh DOWN arrives. Clear it rather than early-return, so one lost UP
    // can't wedge the button permanently.
    ARMED_ID[slot].store(0, Ordering::Release);
    SWALLOWING[slot].store(false, Ordering::Release);

    let Some(binding) = cfg
        .bindings
        .iter()
        .find(|b| b.vk == vk && b.mods == mods)
        .copied()
    else {
        // A button we DO have a binding for, pressed with the wrong modifiers
        // held, is the likeliest cause of "the hook installed but my mouse
        // hotkey does nothing" — and a stuck Ctrl/Alt/Shift, which Remote
        // Desktop sessions produce routinely, causes exactly this while being
        // invisible to the person pressing the button. Say so rather than
        // failing silently; the modifier match itself is deliberate (see
        // `MouseBinding::mods`).
        if let Some(b) = cfg.bindings.iter().find(|b| b.vk == vk) {
            tracing::info!(
                "mouse hotkey vk=0x{vk:02X} not fired: modifiers held are 0x{mods:02X}, \
                 the binding needs exactly 0x{:02X} (a stuck modifier key will do this)",
                b.mods
            );
        }
        return false; // not ours — pass it through untouched
    };

    ARMED_ID[slot].store(binding.id, Ordering::Release);
    if binding.id == cfg.toggle_id {
        let _ = cfg.tx.send(HotkeyEvent::TogglePressed);
        spawn_long_press_poller(slot, cfg.toggle_id, cfg.tx.clone(), cfg.long_press);
    } else if binding.id == cfg.hold_id {
        let _ = cfg.tx.send(HotkeyEvent::HoldPressed);
    }
    let swallow = !cfg.passthrough;
    SWALLOWING[slot].store(swallow, Ordering::Release);
    swallow
}

/// A button came up. Returns true if the release should be suppressed.
///
/// Modifiers are deliberately *not* re-checked: a press we claimed owns its
/// release even if the user let go of Ctrl in between. Re-matching here would
/// strand the DOWN we already swallowed without its pair.
pub(super) fn handle_up(vk: u32) -> bool {
    let Some(slot) = slot_of(vk) else {
        return false;
    };
    let Some(cfg) = CONFIG.load_full() else {
        return false;
    };
    // Only consume an UP whose DOWN we claimed; otherwise the app under the
    // cursor is mid-gesture and the release is genuinely theirs.
    let id = ARMED_ID[slot].swap(0, Ordering::AcqRel);
    if id == 0 {
        return false;
    }
    if id == cfg.hold_id {
        let _ = cfg.tx.send(HotkeyEvent::HoldReleased);
    }
    SWALLOWING[slot].swap(false, Ordering::AcqRel)
}

/// Watch a toggle-bound mouse button and emit [`HotkeyEvent::ToggleLongPressed`]
/// if it is still held when `hold_duration` elapses. Mirrors the keyboard
/// path's poller, but reads our own [`ARMED_ID`] rather than
/// `GetAsyncKeyState`: we suppress the button's real events, and whether the
/// async key state still reflects a suppressed button is not something Windows
/// documents. Our own bookkeeping is the only source of truth that cannot
/// disagree with what we actually did.
fn spawn_long_press_poller(slot: usize, toggle_id: i32, tx: Sender<HotkeyEvent>, hold: Duration) {
    thread::spawn(move || {
        let deadline = Instant::now() + hold;
        loop {
            if ARMED_ID[slot].load(Ordering::Acquire) != toggle_id {
                return; // released (or re-armed by a newer press) — no long press
            }
            if Instant::now() >= deadline {
                let _ = tx.send(HotkeyEvent::ToggleLongPressed);
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    });
}

/// Whether we have already remarked that mouse input is arriving injected.
/// Logged once per run: it is diagnostic colour (RDP session, or a vendor
/// driver remapping buttons), not a problem in itself.
static NOTED_INJECTED: AtomicBool = AtomicBool::new(false);

fn note_injected(vk: u32) {
    if !NOTED_INJECTED.swap(true, Ordering::AcqRel) {
        tracing::info!(
            "mouse button vk=0x{vk:02X} arrived as injected input (Remote Desktop, or a mouse \
             driver remapping buttons). Handled normally \u{2014} this is only noted once."
        );
    }
}

/// A button went down while a [capture lease](capture_lease) may be held.
///
/// The lease makes us passive to **new presses only**: the settings window is
/// trying to *record* this button, so it must reach egui and must not also fire
/// a dictation. Releases are deliberately not routed through here — see
/// [`hook_proc`].
pub(super) fn dispatch_down(vk: u32, mods: u32, capture: bool) -> bool {
    if capture {
        return false;
    }
    handle_down(vk, mods)
}

pub(super) unsafe extern "system" fn hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // A negative code means "pass it on without inspecting", per the Win32
    // hook contract.
    if code == HC_ACTION as i32 {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if let Some((vk, is_down)) = decode(wparam.0 as u32, info.mouseData) {
            // Injected events are deliberately NOT filtered out.
            //
            // The instinct is to ignore anything that did not come from physical
            // hardware, but here it is wrong and the cost is total: this app
            // injects keystrokes only, never mouse input (`output.rs` builds
            // `INPUT_KEYBOARD` records exclusively), so there is no feedback loop
            // for such a filter to prevent. What it *would* prevent is the
            // feature working at all in the two setups most likely to want it: a
            // Remote Desktop session, where every click is delivered by the RDP
            // stack rather than a local device, and a mouse whose vendor driver
            // (Logitech G HUB and friends) remaps a physical button by
            // synthesizing one. Both would present as "the hook installed and
            // the button does nothing" — silent, and near-impossible to guess.
            if info.flags & LLMHF_INJECTED != 0 {
                note_injected(vk);
            }
            let suppress = if is_down {
                dispatch_down(vk, current_modifiers(), capture_active())
            } else {
                // A release is NEVER gated by the capture lease. `handle_up` is
                // already a no-op unless we claimed the matching press, and a
                // press we *did* claim has to be completed no matter what
                // happened in between: skipping it would leak an unpaired
                // button-up into whatever app has focus (we ate its button-down)
                // and would never emit `HoldReleased`, leaving a hold-mode
                // dictation running with nothing to stop it. Reachable for real:
                // hold a mouse-bound hotkey, then open Settings and arm a
                // recorder before letting go.
                handle_up(vk)
            };
            if suppress {
                return LRESULT(1); // suppressed: goes no further
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}
