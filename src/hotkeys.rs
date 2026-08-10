use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use crossbeam_channel::{Receiver, Sender};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL,
    MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, KillTimer, PostThreadMessageW, SetTimer, MSG, WM_HOTKEY, WM_QUIT, WM_TIMER,
};

/// How often the loop re-registers its hotkeys. `RegisterHotKey` bindings can
/// silently die across sleep/resume, session lock/unlock, RDP reconnects, and
/// display changes; periodically re-arming them (SageThumbs-style self-healing)
/// recovers within a minute instead of requiring an app restart.
const REARM_INTERVAL_MS: u32 = 60_000;

/// How long we keep retrying the *initial* hotkey registration before giving
/// up and leaving it to the periodic re-arm. A "Save & Restart" spawns the new
/// process while the old one still owns the global hotkey, so RegisterHotKey
/// fails until the old process exits -- normally well under a second, but we
/// allow generous head-room so the handoff is invisible even on a busy box.
const STARTUP_REGISTER_BUDGET: Duration = Duration::from_secs(6);
/// Gap between initial-registration retries within that budget.
const STARTUP_REGISTER_RETRY_MS: u64 = 150;

/// Consecutive periodic re-arm attempts (see `REARM_INTERVAL_MS`) that must
/// all fail to register before we call the hotkeys "blocked" rather than
/// mid-transient (e.g. a sleep/resume or RDP reconnect the very next re-arm
/// clears on its own). At the default 60s interval, 2 consecutive failures
/// means the failure has already persisted for roughly a minute.
const BLOCKED_STREAK_THRESHOLD: u32 = 2;

/// Floor between "hotkeys still not registered" warnings out of the periodic
/// re-arm, so a permanently-claimed hotkey nudges the log every few minutes
/// instead of either going silent forever or spamming once a minute forever.
const REARM_WARN_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How many consecutive re-arm attempts have failed to register *something*.
/// Reset to 0 the moment a re-arm attempt registers everything configured.
static REARM_FAIL_STREAK: AtomicU32 = AtomicU32::new(0);

/// Set once `REARM_FAIL_STREAK` reaches `BLOCKED_STREAK_THRESHOLD`; cleared
/// as soon as a re-arm attempt succeeds again. Backs `hotkeys_blocked()`.
static HOTKEYS_BLOCKED: AtomicBool = AtomicBool::new(false);

/// Last time the periodic re-arm logged a "still blocked" warning, for the
/// `REARM_WARN_INTERVAL` rate limit.
static LAST_BLOCKED_WARN: parking_lot::Mutex<Option<Instant>> = parking_lot::Mutex::new(None);

/// True once a configured hotkey has failed to (re)register for
/// `BLOCKED_STREAK_THRESHOLD` consecutive re-arm attempts -- i.e. it looks
/// permanently claimed by another process rather than a one-off blip. Cheap
/// to poll (a single atomic load) so other modules (tray icon, settings
/// window) can surface a "hotkey didn't register" indicator without wiring
/// up their own tracing subscriber. No UI lives here; this just exposes the
/// state.
pub fn hotkeys_blocked() -> bool {
    HOTKEYS_BLOCKED.load(Ordering::Acquire)
}

/// Pure step function for the blocked-state streak, kept separate from the
/// static atomics above so it is unit-testable without any win32 calls.
/// Given the previous consecutive-failure count and whether the latest
/// re-arm attempt registered everything configured, returns the new count
/// and whether the blocked threshold has now been reached.
fn step_blocked_streak(streak: u32, all_registered: bool) -> (u32, bool) {
    let streak = if all_registered {
        0
    } else {
        streak.saturating_add(1)
    };
    (streak, streak >= BLOCKED_STREAK_THRESHOLD)
}

/// Record the outcome of one periodic re-arm attempt, updating the flag
/// `hotkeys_blocked()` reports and (rate-limited) logging a warning so a
/// permanently-claimed hotkey isn't silent forever even with no console
/// attached (`windows_subsystem = "windows"`).
fn note_rearm_result(all_registered: bool) {
    let prev = REARM_FAIL_STREAK.load(Ordering::Acquire);
    let (streak, blocked) = step_blocked_streak(prev, all_registered);
    REARM_FAIL_STREAK.store(streak, Ordering::Release);
    HOTKEYS_BLOCKED.store(blocked, Ordering::Release);
    if !blocked {
        return;
    }
    let mut last_warn = LAST_BLOCKED_WARN.lock();
    let should_warn = last_warn.is_none_or(|t| t.elapsed() >= REARM_WARN_INTERVAL);
    if should_warn {
        *last_warn = Some(Instant::now());
        tracing::warn!(
            "hotkey(s) still not registered after repeated re-arm attempts \
             (another process holding them?); dictation will not respond to \
             the configured key(s) until this clears"
        );
    }
}

#[derive(Copy, Clone, Debug)]
pub enum HotkeyEvent {
    TogglePressed,
    ToggleLongPressed,
    HoldPressed,
    HoldReleased,
}

pub struct HotkeyManager {
    pub events: Receiver<HotkeyEvent>,
    pub external_tx: Sender<HotkeyEvent>,
    // Shared with the spawned thread's closure (not a one-time snapshot of
    // it) so a late-published id -- one that lands after the startup wait
    // below gives up -- is still visible to `shutdown()` instead of a stale
    // zero that would make it skip PostThreadMessageW forever.
    thread_id: Arc<AtomicU32>,
    join: parking_lot::Mutex<Option<thread::JoinHandle<()>>>,
    stop_flag: Arc<AtomicBool>,
}

impl HotkeyManager {
    pub fn start(
        toggle_combo: Option<String>,
        hold_combo: Option<String>,
        reinsert_hold_duration: Duration,
    ) -> Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let external_tx = tx.clone();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag2 = Arc::clone(&stop_flag);
        let thread_id = Arc::new(AtomicU32::new(0));
        let thread_id2 = Arc::clone(&thread_id);

        let join = thread::Builder::new()
            .name("qd-hotkeys".into())
            .spawn(move || {
                unsafe {
                    let tid = windows::Win32::System::Threading::GetCurrentThreadId();
                    thread_id2.store(tid, Ordering::Release);
                }
                if let Err(e) = run_hotkey_loop(
                    toggle_combo,
                    hold_combo,
                    reinsert_hold_duration,
                    tx,
                    stop_flag2,
                ) {
                    tracing::error!("hotkey thread: {e:#}");
                }
            })?;

        // Wait briefly so the id is usually already published by the time
        // `start()` returns. No longer load-bearing for correctness -- the
        // `thread_id` stored below is the same Arc the spawned thread writes
        // into, so even a publish that lands after this wait gives up is
        // still visible the next time anything reads it (see `shutdown()`).
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while thread_id.load(Ordering::Acquire) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }

        Ok(Self {
            events: rx,
            external_tx,
            thread_id,
            join: parking_lot::Mutex::new(Some(join)),
            stop_flag,
        })
    }

    pub fn shutdown(&self) {
        self.stop_flag.store(true, Ordering::Release);
        let tid = self.thread_id.load(Ordering::Acquire);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(
                    tid,
                    WM_QUIT,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                );
            }
        }
        if let Some(j) = self.join.lock().take() {
            let _ = j.join();
        }
    }
}

/// (Re)register one hotkey. `quiet` suppresses the per-registration log line
/// (used by the periodic re-arm so the log isn't spammed every minute).
unsafe fn register_one(id: i32, combo: &str, mods: u32, vk: u32, quiet: bool) -> bool {
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
    toggle: Option<&(String, u32, u32)>,
    hold_id: i32,
    hold: Option<&(String, u32, u32)>,
) {
    let deadline = Instant::now() + STARTUP_REGISTER_BUDGET;
    let mut toggle_done = toggle.is_none();
    let mut hold_done = hold.is_none();
    let mut retried = false;
    loop {
        if !toggle_done {
            if let Some((combo, mods, vk)) = toggle {
                if unsafe { register_one(toggle_id, combo, *mods, *vk, true) } {
                    toggle_done = true;
                    tracing::info!("Registered toggle hotkey {combo} (vk=0x{vk:02X})");
                }
            }
        }
        if !hold_done {
            if let Some((combo, mods, vk)) = hold {
                if unsafe { register_one(hold_id, combo, *mods, *vk, true) } {
                    hold_done = true;
                    tracing::info!("Registered hold hotkey {combo} (vk=0x{vk:02X})");
                }
            }
        }
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

fn run_hotkey_loop(
    toggle_combo: Option<String>,
    hold_combo: Option<String>,
    reinsert_hold_duration: Duration,
    tx: Sender<HotkeyEvent>,
    stop_flag: Arc<AtomicBool>,
) -> Result<()> {
    let toggle_id = 1i32;
    let hold_id = 2i32;
    // (combo, mods, vk) for each configured hotkey, parsed once so the
    // periodic re-arm can re-register without re-parsing.
    let mut toggle: Option<(String, u32, u32)> = None;
    let mut hold: Option<(String, u32, u32)> = None;

    // Parse the combos up front. A parse error is a genuine config mistake (a
    // bad key name) and stays fatal; an OS *registration* failure below does
    // NOT abort us -- see `register_initial`.
    if let Some(combo) = toggle_combo.as_deref().filter(|s| !s.is_empty()) {
        let (mods, vk) = parse_combo(combo)?;
        toggle = Some((combo.to_string(), mods, vk));
    }
    if let Some(combo) = hold_combo.as_deref().filter(|s| !s.is_empty()) {
        let (mods, vk) = parse_combo(combo)?;
        hold = Some((combo.to_string(), mods, vk));
    }

    register_initial(toggle_id, toggle.as_ref(), hold_id, hold.as_ref());

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

        if msg.message == WM_TIMER {
            let mut all_registered = true;
            unsafe {
                if let Some((combo, mods, vk)) = toggle.as_ref() {
                    all_registered &= register_one(toggle_id, combo, *mods, *vk, true);
                }
                if let Some((combo, mods, vk)) = hold.as_ref() {
                    all_registered &= register_one(hold_id, combo, *mods, *vk, true);
                }
            }
            note_rearm_result(all_registered);
            tracing::debug!("hotkeys re-armed");
            continue;
        }
        if msg.message != WM_HOTKEY {
            continue;
        }
        let id = msg.wParam.0 as i32;
        tracing::info!("WM_HOTKEY received: id={id}");
        if id == toggle_id {
            let _ = tx.send(HotkeyEvent::TogglePressed);
            if let Some((_, _, vk)) = toggle {
                spawn_long_press_poller(vk, tx.clone(), reinsert_hold_duration);
            }
        } else if id == hold_id {
            let _ = tx.send(HotkeyEvent::HoldPressed);
            if let Some((_, _, vk)) = hold {
                spawn_release_poller(vk, tx.clone());
            }
        }
    }

    unsafe {
        let null_hwnd = windows::Win32::Foundation::HWND::default();
        if rearm_timer != 0 {
            let _ = KillTimer(null_hwnd, rearm_timer);
        }
        if toggle.is_some() {
            let _ = UnregisterHotKey(null_hwnd, toggle_id);
        }
        if hold.is_some() {
            let _ = UnregisterHotKey(null_hwnd, hold_id);
        }
    }
    Ok(())
}

fn spawn_long_press_poller(vk: u32, tx: Sender<HotkeyEvent>, hold_duration: Duration) {
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

/// Parse a hotkey combo like "f14" or "ctrl+shift+d" into (modifiers, vk).
/// `pub(crate)` so the settings window can validate user input before saving.
pub(crate) fn parse_combo(combo: &str) -> Result<(u32, u32)> {
    let mut modifiers: u32 = 0;
    let mut vk: u32 = 0;
    for part_raw in combo.split('+') {
        let part = part_raw.trim().to_ascii_lowercase();
        if part.is_empty() {
            continue;
        }
        let m = match part.as_str() {
            "ctrl" | "control" => Some(MOD_CONTROL.0),
            "alt" | "menu" => Some(MOD_ALT.0),
            "shift" => Some(MOD_SHIFT.0),
            "win" | "windows" | "super" => Some(MOD_WIN.0),
            _ => None,
        };
        if let Some(bits) = m {
            modifiers |= bits;
            continue;
        }
        let candidate =
            vk_for(&part).ok_or_else(|| anyhow!("unknown key '{part}' in '{combo}'"))?;
        if vk != 0 {
            bail!("multiple non-modifier keys in '{combo}'");
        }
        vk = candidate;
    }
    if vk == 0 {
        bail!("no main key in '{combo}'");
    }
    Ok((modifiers | MOD_NOREPEAT.0, vk))
}

fn vk_for(name: &str) -> Option<u32> {
    // Letters a-z
    if name.len() == 1 {
        let c = name.as_bytes()[0];
        if c.is_ascii_lowercase() {
            return Some((0x41 + (c - b'a')) as u32);
        }
        if c.is_ascii_digit() {
            return Some((0x30 + (c - b'0')) as u32);
        }
    }
    let v = match name {
        "f1" => 0x70,
        "f2" => 0x71,
        "f3" => 0x72,
        "f4" => 0x73,
        "f5" => 0x74,
        "f6" => 0x75,
        "f7" => 0x76,
        "f8" => 0x77,
        "f9" => 0x78,
        "f10" => 0x79,
        "f11" => 0x7A,
        "f12" => 0x7B,
        "f13" => 0x7C,
        "f14" => 0x7D,
        "f15" => 0x7E,
        "f16" => 0x7F,
        "f17" => 0x80,
        "f18" => 0x81,
        "f19" => 0x82,
        "f20" => 0x83,
        "f21" => 0x84,
        "f22" => 0x85,
        "f23" => 0x86,
        "f24" => 0x87,
        "space" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "escape" | "esc" => 0x1B,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "page_up" => 0x21,
        "pagedown" | "page_down" => 0x22,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "numpad0" => 0x60,
        "numpad1" => 0x61,
        "numpad2" => 0x62,
        "numpad3" => 0x63,
        "numpad4" => 0x64,
        "numpad5" => 0x65,
        "numpad6" => 0x66,
        "numpad7" => 0x67,
        "numpad8" => 0x68,
        "numpad9" => 0x69,
        _ => return None,
    };
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bare_function_key() {
        // No modifiers, but MOD_NOREPEAT is always set; f14 == VK 0x7D.
        assert_eq!(parse_combo("f14").unwrap(), (MOD_NOREPEAT.0, 0x7D));
    }

    #[test]
    fn parses_modifiers_plus_a_letter() {
        let (mods, vk) = parse_combo("ctrl+shift+d").unwrap();
        assert_eq!(vk, 0x44); // 'd'
        assert_eq!(mods, MOD_CONTROL.0 | MOD_SHIFT.0 | MOD_NOREPEAT.0);
    }

    #[test]
    fn parsing_ignores_case_and_surrounding_whitespace() {
        assert_eq!(
            parse_combo("  CTRL + Shift + D ").unwrap(),
            parse_combo("ctrl+shift+d").unwrap()
        );
    }

    #[test]
    fn accepts_modifier_and_key_aliases() {
        // control==ctrl, menu==alt, del==delete
        assert_eq!(
            parse_combo("control+menu+del").unwrap(),
            (MOD_CONTROL.0 | MOD_ALT.0 | MOD_NOREPEAT.0, 0x2E)
        );
        // windows/super == win; return == enter
        assert_eq!(
            parse_combo("windows+return").unwrap(),
            (MOD_WIN.0 | MOD_NOREPEAT.0, 0x0D)
        );
        assert_eq!(
            parse_combo("super+esc").unwrap().0,
            MOD_WIN.0 | MOD_NOREPEAT.0
        );
    }

    #[test]
    fn every_parsed_combo_sets_norepeat() {
        for combo in ["f13", "ctrl+a", "alt+shift+space"] {
            let (mods, _) = parse_combo(combo).unwrap();
            assert_ne!(mods & MOD_NOREPEAT.0, 0, "combo {combo} missing NOREPEAT");
        }
    }

    #[test]
    fn rejects_malformed_combos() {
        assert!(parse_combo("").is_err()); // nothing
        assert!(parse_combo("ctrl").is_err()); // modifier only, no main key
        assert!(parse_combo("a+b").is_err()); // two non-modifier keys
        assert!(parse_combo("ctrl+notakey").is_err()); // unknown key name
        assert!(parse_combo("f25").is_err()); // outside the F-key table
    }

    #[test]
    fn blocked_streak_needs_threshold_consecutive_failures() {
        let (streak, blocked) = step_blocked_streak(0, false);
        assert_eq!(streak, 1);
        assert!(
            !blocked,
            "a single failure should not trip the blocked flag"
        );

        let (streak, blocked) = step_blocked_streak(streak, false);
        assert_eq!(streak, BLOCKED_STREAK_THRESHOLD);
        assert!(blocked, "threshold consecutive failures should trip it");
    }

    #[test]
    fn blocked_streak_resets_on_any_success() {
        let (streak, blocked) = step_blocked_streak(BLOCKED_STREAK_THRESHOLD, true);
        assert_eq!(streak, 0, "a successful re-arm must clear the streak");
        assert!(!blocked);

        // A success right after the very first failure also resets cleanly.
        let (streak, _) = step_blocked_streak(1, true);
        assert_eq!(streak, 0);
    }

    #[test]
    fn blocked_streak_stays_blocked_past_threshold() {
        // Once blocked, continued failures keep it blocked (no wraparound)
        // until a success clears it.
        let (streak, blocked) = step_blocked_streak(BLOCKED_STREAK_THRESHOLD + 5, false);
        assert!(streak > BLOCKED_STREAK_THRESHOLD);
        assert!(blocked);
    }

    #[test]
    fn vk_table_maps_the_known_keys() {
        // Locks the hand-written VK lookup table — a typo here would silently
        // register the wrong physical key. (vk_for expects lowercase input, as
        // parse_combo feeds it.)
        assert_eq!(vk_for("a"), Some(0x41));
        assert_eq!(vk_for("z"), Some(0x5A));
        assert_eq!(vk_for("0"), Some(0x30));
        assert_eq!(vk_for("9"), Some(0x39));
        assert_eq!(vk_for("f1"), Some(0x70));
        assert_eq!(vk_for("f12"), Some(0x7B));
        assert_eq!(vk_for("f13"), Some(0x7C));
        assert_eq!(vk_for("f24"), Some(0x87));
        assert_eq!(vk_for("space"), Some(0x20));
        assert_eq!(vk_for("enter"), Some(0x0D));
        assert_eq!(vk_for("up"), Some(0x26));
        assert_eq!(vk_for("numpad0"), Some(0x60));
        assert_eq!(vk_for("nope"), None);
        assert_eq!(vk_for("A"), None); // case-sensitive: expects lowercase
    }
}
