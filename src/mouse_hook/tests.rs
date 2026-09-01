//! Unit tests for the mouse-hook state machine.
//!
//! These drive [`handle_down`] / [`handle_up`] and the capture lease directly
//! rather than installing a real hook, so the press/release pairing that
//! constraint 2 in the module docs is about can be checked without a mouse or
//! a message pump. A process-wide lock serializes them, because the state they
//! exercise is process-global statics.

use windows::Win32::UI::Input::KeyboardAndMouse::MOD_CONTROL;
use windows::Win32::UI::WindowsAndMessaging::{
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

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
