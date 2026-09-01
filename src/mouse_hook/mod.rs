//! Global **mouse-button** hotkeys, via a low-level mouse hook.
//!
//! Every keyboard hotkey in [`crate::hotkeys`] rides on `RegisterHotKey`, and
//! that API is keyboard-only: Windows will not accept `VK_MBUTTON`,
//! `VK_XBUTTON1` or `VK_XBUTTON2`, so "bind my mouse's back button to
//! dictation" cannot be expressed through it at all. This module is the other
//! mechanism — a system-wide `WH_MOUSE_LL` hook — and it exists solely so the
//! three mouse buttons Windows actually reports (middle, and the two thumb
//! buttons commonly labelled Back/Forward) can be bound like any other key.
//!
//! **It costs no extra thread.** A low-level hook's callback is dispatched
//! onto the thread that *installed* it, while that thread pumps messages — and
//! the hotkey thread already sits in a `GetMessageW` loop for `WM_HOTKEY`. So
//! the hook is installed from, and runs on, that same loop.
//!
//! Three constraints drive nearly every design decision below:
//!
//! 1. **The callback must be fast.** Windows gives a low-level hook
//!    `LowLevelHooksTimeout` milliseconds (default 300) to return; overrun it
//!    and the OS *silently* removes the hook — no error, no callback, the
//!    hotkey just stops working forever. So the callback does no I/O, takes no
//!    lock that anything slow can hold, and never blocks. Configuration is read
//!    through an [`ArcSwapOption`] (a lock-free atomic load) and all per-button
//!    state is plain atomics. Because silent removal is still possible under a
//!    badly-behaved system, [`ensure_installed`] is re-run by the hotkey loop's
//!    existing 60-second re-arm timer, matching how the keyboard side already
//!    self-heals.
//!
//! 2. **A swallowed press must swallow its release too.** Suppressing the
//!    `WM_XBUTTONDOWN` for a bound button while letting the `WM_XBUTTONUP`
//!    through leaves every other app seeing an unpaired button-up, which is how
//!    you get stuck drag states. The slot state machine below pairs them.
//!
//! 3. **Recording a mouse button must not also fire it.** If the back button is
//!    already bound and you open Settings to rebind it, the click that you mean
//!    as "record this" would otherwise start a dictation. The settings window
//!    takes a short, self-expiring [capture lease](capture_lease) that makes the
//!    hook pass everything through untouched.
//!
//! ## Layout of this module
//! This file is the hub: the bindable virtual-key codes, the configuration
//! swapped in for the callback to read, the per-button state that callback
//! drives, the capture lease, and installing/removing the hook itself.
//!
//! - [`callback`]: the hook procedure and everything it runs — message
//!   decoding, the press/release state machine, and the long-press poller.
//!   It is kept apart because that code runs under a hard timeout (see
//!   constraint 1 above) and is the half worth reading on its own.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use crossbeam_channel::Sender;
use once_cell::sync::Lazy;
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, WH_MOUSE_LL,
};

use crate::hotkeys::HotkeyEvent;

// Split out of this file so the timing-critical callback can be reviewed on
// its own; the hub keeps the configuration, the state, and installation.
mod callback;

#[cfg(test)]
mod tests;

use callback::*;

/// Virtual-key code for the middle mouse button (the scroll wheel's click).
pub const VK_MBUTTON: u32 = 0x04;
/// Virtual-key code for the first thumb button — what mouse vendors label
/// "Back" and what Windows calls XBUTTON1.
pub const VK_XBUTTON1: u32 = 0x05;
/// Virtual-key code for the second thumb button — "Forward" / XBUTTON2.
pub const VK_XBUTTON2: u32 = 0x06;
/// Virtual-key code for the left mouse button. Never bindable: `parse_combo`
/// in [`crate::hotkeys`] refuses it by VK, so no spelling is a back door.
pub const VK_LBUTTON: u32 = 0x01;
/// Virtual-key code for the right mouse button. Never bindable.
pub const VK_RBUTTON: u32 = 0x02;

/// Whether `vk` is one of the three mouse buttons this module can bind.
/// Deliberately excludes left/right: those are how a person operates their
/// computer, and a bound button is a *suppressed* button.
pub fn is_mouse_vk(vk: u32) -> bool {
    matches!(vk, VK_MBUTTON | VK_XBUTTON1 | VK_XBUTTON2)
}

/// Slot index (0..3) for a bindable mouse VK, used to index the per-button
/// state arrays. `None` for anything not bindable.
fn slot_of(vk: u32) -> Option<usize> {
    match vk {
        VK_MBUTTON => Some(0),
        VK_XBUTTON1 => Some(1),
        VK_XBUTTON2 => Some(2),
        _ => None,
    }
}

/// One configured mouse binding, pre-parsed so the hook callback only ever
/// compares integers.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MouseBinding {
    /// The same hotkey id the keyboard path uses (1 = toggle, 2 = hold), so
    /// downstream event mapping stays identical across both mechanisms.
    pub id: i32,
    pub vk: u32,
    /// Required modifiers, **without** `MOD_NOREPEAT` (which is a
    /// `RegisterHotKey` concept with no meaning here). Matched exactly, so a
    /// plain-middle-click binding does not fire on Ctrl+middle-click — mirroring
    /// `RegisterHotKey`'s own semantics.
    pub mods: u32,
}

/// Everything the hook callback needs, swapped in atomically as one unit.
struct HookConfig {
    bindings: Vec<MouseBinding>,
    tx: Sender<HotkeyEvent>,
    /// False (the default) means a bound button is consumed and never reaches
    /// the app under the cursor — a dedicated dictation button that *also*
    /// navigated your browser back would be a bug, not a feature.
    passthrough: bool,
    /// How long a toggle-bound button must stay held to count as a long press
    /// (the re-paste gesture).
    long_press: Duration,
    toggle_id: i32,
    hold_id: i32,
}

static CONFIG: Lazy<ArcSwapOption<HookConfig>> = Lazy::new(ArcSwapOption::empty);

/// The installed hook handle as a raw isize (`HHOOK` is not `Sync`). 0 = none.
static HOOK: AtomicIsize = AtomicIsize::new(0);

/// Per-slot: which binding id currently owns this button's press (0 = none).
/// Set on a matching DOWN, cleared on the paired UP. Doubles as "is this
/// button held" for the long-press poller.
static ARMED_ID: [AtomicI32; 3] = [const { AtomicI32::new(0) }; 3];

/// Per-slot: whether we suppressed the DOWN, and therefore owe the system a
/// suppressed UP as well (constraint 2 in the module docs).
static SWALLOWING: [AtomicBool; 3] = [const { AtomicBool::new(false) }; 3];

/// Process-start reference point for the monotonic millisecond clock the
/// capture lease uses. `Instant::elapsed` is a QPC read — cheap enough for the
/// hook callback's budget.
static START: Lazy<Instant> = Lazy::new(Instant::now);

/// Monotonic milliseconds since process start.
fn now_ms() -> u64 {
    START.elapsed().as_millis() as u64
}

/// Deadline (in `now_ms()` terms) until which the hook stays passive. See
/// [`capture_lease`].
static CAPTURE_UNTIL_MS: AtomicU64 = AtomicU64::new(0);

/// How long one [`capture_lease`] call holds the hook passive. The settings
/// window refreshes this every frame while a hotkey field is recording, so the
/// lease is continuously renewed while it matters — and **lapses on its own**
/// if that window disappears mid-record. A flag someone has to remember to
/// clear would leave the hotkeys permanently dead after a crash or a forgotten
/// code path; a lease cannot.
const CAPTURE_LEASE_MS: u64 = 600;

/// Hold the mouse hook passive for the next [`CAPTURE_LEASE_MS`] milliseconds:
/// bound buttons fire nothing and are passed through to whatever window is
/// under the cursor. Called each frame by the settings window while a hotkey
/// field is recording, so that pressing an already-bound mouse button in order
/// to *rebind* it doesn't also start a dictation.
pub fn capture_lease() {
    CAPTURE_UNTIL_MS.store(now_ms() + CAPTURE_LEASE_MS, Ordering::Release);
}

/// Drop any active capture lease immediately (recording finished or was
/// cancelled), so hotkeys are live again on the very next press instead of
/// after the lease lapses.
pub fn end_capture_lease() {
    CAPTURE_UNTIL_MS.store(0, Ordering::Release);
}

fn capture_active() -> bool {
    now_ms() < CAPTURE_UNTIL_MS.load(Ordering::Acquire)
}

/// Publish the binding set the hook should act on. Safe to call before or
/// after [`ensure_installed`]; the callback always reads the latest.
pub fn configure(
    bindings: Vec<MouseBinding>,
    tx: Sender<HotkeyEvent>,
    passthrough: bool,
    long_press: Duration,
    toggle_id: i32,
    hold_id: i32,
) {
    Lazy::force(&START);
    CONFIG.store(Some(Arc::new(HookConfig {
        bindings,
        tx,
        passthrough,
        long_press,
        toggle_id,
        hold_id,
    })));
}

/// Install the hook if it isn't already installed. Returns whether a hook is
/// live afterwards.
///
/// **Must be called from the thread that pumps the message loop** — Windows
/// dispatches a low-level hook's callback onto the installing thread, so a
/// hook installed from a thread that never pumps simply never fires.
///
/// Idempotent, and called both at startup and from the hotkey loop's periodic
/// re-arm, because Windows silently removes a low-level hook that overruns
/// `LowLevelHooksTimeout`.
pub fn ensure_installed() -> bool {
    if HOOK.load(Ordering::Acquire) != 0 {
        return true;
    }
    let module = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) };
    let hmod = match module {
        Ok(m) => windows::Win32::Foundation::HINSTANCE(m.0),
        Err(e) => {
            tracing::warn!("mouse hotkeys: GetModuleHandleW failed: {e}");
            return false;
        }
    };
    match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), hmod, 0) } {
        Ok(h) => {
            HOOK.store(h.0 as isize, Ordering::Release);
            tracing::info!("mouse hotkey hook installed");
            true
        }
        Err(e) => {
            tracing::warn!("mouse hotkeys: SetWindowsHookExW failed: {e} (will retry on re-arm)");
            false
        }
    }
}

/// Remove the hook and forget the configured bindings. Called on shutdown so a
/// replacement process (Save & Restart, self-update) isn't competing with a
/// stale hook for the same buttons.
///
/// **Known, accepted edge:** if a bound button is *physically held* at this
/// moment, we already swallowed its press, and its release will arrive after
/// the hook is gone — so one unpaired button-up reaches whatever app has focus.
/// It cannot be prevented from here (the hook is what would have caught it),
/// and the alternative, synthesizing a balancing button-up with `SendInput`,
/// would mean this app injecting mouse events into other people's windows
/// during teardown, which is a worse failure mode than the one it fixes. The
/// blast radius is one stray event, and back/forward navigation is driven by
/// button-*down* in practice, so it is usually invisible. Reaching it requires
/// a relaunch to land in the exact instant the button is down.
pub fn uninstall() {
    let raw = HOOK.swap(0, Ordering::AcqRel);
    if raw != 0 {
        let _ = unsafe { UnhookWindowsHookEx(HHOOK(raw as *mut core::ffi::c_void)) };
        tracing::info!("mouse hotkey hook removed");
    }
    CONFIG.store(None);
    for slot in 0..3 {
        ARMED_ID[slot].store(0, Ordering::Release);
        SWALLOWING[slot].store(false, Ordering::Release);
    }
}
