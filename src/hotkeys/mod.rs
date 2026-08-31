//! Global hotkey registration and the thread that dispatches presses.

mod combo;
mod dispatch;
mod register;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};

/// How often the loop re-registers its hotkeys. `RegisterHotKey` bindings can
/// silently die across sleep/resume, session lock/unlock, RDP reconnects, and
/// display changes; periodically re-arming them (SageThumbs-style self-healing)
pub use combo::parse_combo;

use dispatch::*;
use register::*;

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
        mouse_passthrough: bool,
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
                    mouse_passthrough,
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
