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

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use crossbeam_channel::Sender;
use once_cell::sync::Lazy;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK, LLMHF_INJECTED,
    MSLLHOOKSTRUCT, WH_MOUSE_LL, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
    XBUTTON1, XBUTTON2,
};

use crate::hotkeys::HotkeyEvent;

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
fn decode(message: u32, mouse_data: u32) -> Option<(u32, bool)> {
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
fn handle_down(vk: u32, mods: u32) -> bool {
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
fn handle_up(vk: u32) -> bool {
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
fn dispatch_down(vk: u32, mods: u32, capture: bool) -> bool {
    if capture {
        return false;
    }
    handle_down(vk, mods)
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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

#[cfg(test)]
mod tests {
    use super::*;

    const TOGGLE_ID: i32 = 1;
    const HOLD_ID: i32 = 2;

    /// A hook procedure cannot capture, so its configuration and per-button
    /// state are necessarily process-global — which means these tests would
    /// stomp each other under the default parallel test runner. Every test
    /// that touches that state takes this lock and starts from a clean slate.
    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// Serialized access to the global hook state, reset on entry.
    fn guard() -> parking_lot::MutexGuard<'static, ()> {
        // A panicking test would otherwise poison every later one; parking_lot
        // has no poisoning, so the lock is simply re-acquired.
        let g = TEST_LOCK.lock();
        CONFIG.store(None);
        CAPTURE_UNTIL_MS.store(0, Ordering::Release);
        for slot in 0..3 {
            ARMED_ID[slot].store(0, Ordering::Release);
            SWALLOWING[slot].store(false, Ordering::Release);
        }
        g
    }

    fn configure_for_test(
        bindings: Vec<MouseBinding>,
        passthrough: bool,
    ) -> crossbeam_channel::Receiver<HotkeyEvent> {
        let (tx, rx) = crossbeam_channel::unbounded();
        configure(
            bindings,
            tx,
            passthrough,
            // Long enough that no long-press poller fires mid-test.
            Duration::from_secs(3600),
            TOGGLE_ID,
            HOLD_ID,
        );
        rx
    }

    fn hold_on(vk: u32) -> Vec<MouseBinding> {
        vec![MouseBinding {
            id: HOLD_ID,
            vk,
            mods: 0,
        }]
    }

    #[test]
    fn slots_cover_exactly_the_bindable_buttons() {
        assert_eq!(slot_of(VK_MBUTTON), Some(0));
        assert_eq!(slot_of(VK_XBUTTON1), Some(1));
        assert_eq!(slot_of(VK_XBUTTON2), Some(2));
        // Left/right are never bindable, so they have no state slot at all.
        assert_eq!(slot_of(VK_LBUTTON), None);
        assert_eq!(slot_of(VK_RBUTTON), None);
        assert!(is_mouse_vk(VK_XBUTTON1));
        assert!(!is_mouse_vk(VK_LBUTTON));
        assert!(!is_mouse_vk(0x41)); // 'A'
    }

    #[test]
    fn decodes_the_thumb_button_from_the_high_word() {
        // XBUTTON1/2 live in the HIGH word of mouseData — reading the low word
        // would silently bind the wrong physical button.
        assert_eq!(
            decode(WM_XBUTTONDOWN, (XBUTTON1 as u32) << 16),
            Some((VK_XBUTTON1, true))
        );
        assert_eq!(
            decode(WM_XBUTTONUP, (XBUTTON2 as u32) << 16),
            Some((VK_XBUTTON2, false))
        );
        assert_eq!(decode(WM_MBUTTONDOWN, 0), Some((VK_MBUTTON, true)));
        assert_eq!(decode(WM_MBUTTONUP, 0), Some((VK_MBUTTON, false)));
        // A wheel tick, a move, or an unknown X button is not ours.
        assert_eq!(decode(0x0200 /* WM_MOUSEMOVE */, 0), None);
        assert_eq!(decode(WM_XBUTTONDOWN, 9 << 16), None);
    }

    #[test]
    fn a_bound_button_fires_and_is_swallowed_in_pairs() {
        let _g = guard();
        let rx = configure_for_test(hold_on(VK_XBUTTON1), false);
        assert!(handle_down(VK_XBUTTON1, 0), "DOWN must be swallowed");
        assert!(matches!(rx.try_recv(), Ok(HotkeyEvent::HoldPressed)));
        assert!(
            handle_up(VK_XBUTTON1),
            "the paired UP must be swallowed too, or apps see an unpaired button-up"
        );
        assert!(matches!(rx.try_recv(), Ok(HotkeyEvent::HoldReleased)));
    }

    #[test]
    fn passthrough_still_fires_but_suppresses_nothing() {
        let _g = guard();
        let rx = configure_for_test(
            vec![MouseBinding {
                id: TOGGLE_ID,
                vk: VK_MBUTTON,
                mods: 0,
            }],
            true,
        );
        assert!(!handle_down(VK_MBUTTON, 0), "passthrough must not suppress");
        assert!(matches!(rx.try_recv(), Ok(HotkeyEvent::TogglePressed)));
        assert!(!handle_up(VK_MBUTTON));
    }

    #[test]
    fn modifiers_must_match_exactly() {
        let _g = guard();
        let rx = configure_for_test(hold_on(VK_MBUTTON), false);
        // Plain middle-click is bound, so Ctrl+middle-click (open-in-new-tab
        // in every browser) must still reach the app untouched — this mirrors
        // RegisterHotKey's own exact-match semantics.
        assert!(!handle_down(VK_MBUTTON, MOD_CONTROL.0));
        assert!(rx.try_recv().is_err(), "a modified press must not fire");
        // And its release passes through too, since we never claimed the press.
        assert!(!handle_up(VK_MBUTTON));

        // The reverse: a Ctrl-qualified binding must not fire on a bare press.
        let rx = configure_for_test(
            vec![MouseBinding {
                id: HOLD_ID,
                vk: VK_MBUTTON,
                mods: MOD_CONTROL.0,
            }],
            false,
        );
        assert!(!handle_down(VK_MBUTTON, 0));
        assert!(rx.try_recv().is_err());
        assert!(handle_down(VK_MBUTTON, MOD_CONTROL.0));
        assert!(matches!(rx.try_recv(), Ok(HotkeyEvent::HoldPressed)));
    }

    #[test]
    fn a_release_is_claimed_even_if_the_modifier_was_let_go() {
        let _g = guard();
        let rx = configure_for_test(
            vec![MouseBinding {
                id: HOLD_ID,
                vk: VK_XBUTTON2,
                mods: MOD_CONTROL.0,
            }],
            false,
        );
        assert!(handle_down(VK_XBUTTON2, MOD_CONTROL.0));
        assert!(matches!(rx.try_recv(), Ok(HotkeyEvent::HoldPressed)));
        // Ctrl released mid-hold. The UP still belongs to us: re-matching
        // modifiers here would leak an unpaired button-up into every app, and
        // would strand the dictation in the "held" state forever.
        assert!(handle_up(VK_XBUTTON2));
        assert!(matches!(rx.try_recv(), Ok(HotkeyEvent::HoldReleased)));
    }

    #[test]
    fn an_unclaimed_release_is_never_swallowed() {
        let _g = guard();
        let _rx = configure_for_test(hold_on(VK_XBUTTON2), false);
        // No DOWN was claimed, so this UP belongs to whatever app is
        // mid-gesture — swallowing it would strand that app's drag state.
        assert!(!handle_up(VK_XBUTTON2));
    }

    #[test]
    fn an_unbound_button_is_untouched() {
        let _g = guard();
        let rx = configure_for_test(hold_on(VK_XBUTTON1), false);
        assert!(!handle_down(VK_MBUTTON, 0));
        assert!(!handle_up(VK_MBUTTON));
        assert!(
            rx.try_recv().is_err(),
            "an unbound button must fire nothing"
        );
    }

    #[test]
    fn no_config_means_no_interference() {
        let _g = guard();
        // Before `configure`, or after `uninstall`, the hook must be inert —
        // never swallowing a button it has no binding for.
        assert!(!handle_down(VK_MBUTTON, 0));
        assert!(!handle_up(VK_XBUTTON1));
    }

    #[test]
    fn a_lost_release_cannot_wedge_the_button() {
        let _g = guard();
        let rx = configure_for_test(hold_on(VK_XBUTTON1), false);
        assert!(handle_down(VK_XBUTTON1, 0));
        assert!(matches!(rx.try_recv(), Ok(HotkeyEvent::HoldPressed)));
        // Simulate never seeing the UP (session lock, hook reinstalled), then
        // a fresh press: it must still register rather than being ignored as
        // "already down", or the button is dead until restart.
        assert!(handle_down(VK_XBUTTON1, 0));
        assert!(matches!(rx.try_recv(), Ok(HotkeyEvent::HoldPressed)));
    }

    #[test]
    fn uninstall_leaves_the_hook_inert() {
        let _g = guard();
        let rx = configure_for_test(hold_on(VK_MBUTTON), false);
        assert!(handle_down(VK_MBUTTON, 0));
        let _ = rx.try_recv();
        uninstall();
        // Mid-press teardown must not leave a claimed slot behind that would
        // swallow the next release, nor keep firing into a dead channel.
        assert!(!handle_up(VK_MBUTTON));
        assert!(!handle_down(VK_MBUTTON, 0));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_capture_lease_blocks_new_presses_but_never_a_claimed_release() {
        let _g = guard();
        let rx = configure_for_test(hold_on(VK_XBUTTON1), false);

        // Press and hold the bound button: claimed and swallowed as usual.
        assert!(handle_down(VK_XBUTTON1, 0));
        assert!(matches!(rx.try_recv(), Ok(HotkeyEvent::HoldPressed)));

        // Now the settings window arms a recorder (on any field) while the
        // button is STILL physically held.
        capture_lease();

        // A NEW press must be passive, so it can be recorded rather than fire.
        assert!(
            !dispatch_down(VK_MBUTTON, 0, capture_active()),
            "a lease must let a fresh press through to the settings window"
        );

        // But the release of the press we already claimed must still complete.
        // Gating this on the lease would leave the dictation running forever
        // and leak an unpaired button-up into the focused app.
        assert!(
            handle_up(VK_XBUTTON1),
            "a claimed press must be completed even while a capture lease is held"
        );
        assert!(
            matches!(rx.try_recv(), Ok(HotkeyEvent::HoldReleased)),
            "HoldReleased is the only thing that stops a hold-mode dictation"
        );
    }

    #[test]
    fn a_lease_keeps_an_unclaimed_release_passive_too() {
        let _g = guard();
        let rx = configure_for_test(hold_on(VK_XBUTTON1), false);
        // Press arrives during the lease, so it is never claimed...
        capture_lease();
        assert!(!dispatch_down(VK_XBUTTON1, 0, capture_active()));
        assert!(rx.try_recv().is_err());
        // ...and its release must therefore pass through untouched as well,
        // leaving the app under the cursor with a matched pair.
        assert!(!handle_up(VK_XBUTTON1));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn capture_lease_expires_on_its_own() {
        let _g = guard();
        assert!(!capture_active(), "no lease taken yet");
        capture_lease();
        assert!(capture_active(), "a fresh lease holds the hook passive");
        end_capture_lease();
        assert!(!capture_active(), "ending the lease re-arms immediately");
        // And a lease is a deadline, not a flag: one that is never refreshed
        // lapses without anyone having to remember to clear it, so a settings
        // window that vanishes mid-record cannot leave hotkeys dead.
        CAPTURE_UNTIL_MS.store(now_ms().saturating_sub(1), Ordering::Release);
        assert!(!capture_active());
    }
}
