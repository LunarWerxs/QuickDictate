//! A watch on the hotkey keys themselves, for the press Windows drops.
//!
//! `RegisterHotKey` is a black box: when it stops answering, nothing says so.
//! On 2 October 2026 a G HUB-driven F14 went silent for an hour while its
//! binding stayed registered and was re-armed every minute, and one synthetic
//! key-up for F14 brought it straight back. Whatever Windows was holding (most
//! likely a key it believed was still down, so `MOD_NOREPEAT` took every fresh
//! press for auto-repeat), the app could neither see it nor recover from it,
//! and the log could not even say whether the key had reached Windows at all.
//!
//! So a passive `WH_KEYBOARD_LL` hook watches the configured keys, plus the
//! modifier keys so a combo is matched exactly as Windows matches it. It never
//! swallows or changes a key, and every other key is ignored the moment its
//! code is read. A fresh press of a hotkey key starts a short timer; the
//! `WM_HOTKEY` that normally follows within a millisecond or two settles it.
//! If the timer fires first, Windows dropped the press: the watch handles it
//! itself, logs that it did, re-registers the binding, and once the key is up
//! sends one key-up for it, which clears whatever Windows was holding.
//!
//! It also answers the question the old log could not. A press with neither a
//! `WM_HOTKEY received` line nor a "dropped" line never reached Windows: the
//! device or its software (a G HUB profile, say) sent nothing.
//!
//! Runs on the hotkey thread, like [`crate::mouse_hook`]: Windows calls a
//! low-level hook on the thread that installed it while that thread pumps
//! messages, and that thread also handles every `WM_HOTKEY` and timer, so the
//! state below needs no lock. The callback stays inside the hook time budget
//! described there: no I/O except one log line per run, no blocking.

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    MAPVK_VK_TO_VSC, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, KillTimer, PostThreadMessageW, SetTimer, SetWindowsHookExW,
    UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, WH_KEYBOARD_LL, WM_APP,
    WM_KEYDOWN, WM_SYSKEYDOWN,
};

use super::HotkeyEvent;

/// How often the watch timer looks for a press still waiting on its
/// `WM_HOTKEY`. Bounds the delay a dropped press picks up before it is handled.
const TIMER_MS: u32 = 250;

/// A fresh press with no `WM_HOTKEY` after this long was dropped. Windows posts
/// the message as soon as the hook chain returns, so a real one lands within a
/// millisecond or two, and a timer message never overtakes it: Windows only
/// makes a `WM_TIMER` when the queue is otherwise empty.
pub(super) const DROPPED_AFTER: Duration = Duration::from_millis(100);

/// While a key is held its further downs are auto-repeat. Auto-repeat starts at
/// most a second after the press (the slowest Windows keyboard delay) and then
/// runs faster, so a down arriving longer than this after the previous one is
/// a new press whose key-up never reached the watch.
pub(super) const LOST_UP_GAP: Duration = Duration::from_millis(1500);

/// A `WM_HOTKEY` this soon after the watch handled a press itself, with no new
/// press in between, is that same press arriving late (another program's slow
/// hook held up the chain), not a second one.
pub(super) const LATE_HOTKEY: Duration = Duration::from_millis(1500);

/// Marks the key-up the watch sends, so its own hook passes it by.
const OUR_KEY_UP: usize = 0x5144_4b55; // "QDKU"

/// Posted to the hotkey thread by the hook: send the clearing key-up for the
/// key in `wParam`. Input is never sent from inside the hook callback.
pub(super) const WM_WATCH_KEY_UP: u32 = WM_APP + 0x51;

/// Re-arm ticks between the "still watching" lines in the log: half an hour at
/// the 60-second re-arm.
const REPORT_EVERY_TICKS: u32 = 30;

/// One configured keyboard binding, and what the watch knows about its key.
#[derive(Debug)]
struct Key {
    id: i32,
    vk: u32,
    /// `MOD_*` bits the combo needs, `MOD_NOREPEAT` stripped.
    mods: u32,
    /// The hold binding, whose release matters; otherwise the toggle.
    hold: bool,
    /// Our `RegisterHotKey` for it currently stands. A key someone else holds
    /// is theirs to answer, so it is not watched for drops.
    registered: bool,
    down: bool,
    last_down: Option<Instant>,
    /// A fresh press waiting on its `WM_HOTKEY`.
    pending: Option<Instant>,
    /// When the watch last handled a press itself.
    rescued: Option<Instant>,
    /// A handled hold press is still down: its key-up ends the dictation.
    release_owed: bool,
    /// Send the clearing key-up once this key is up.
    clear_owed: bool,
}

/// A dropped press, for the hotkey thread to handle.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Rescue {
    pub(super) id: i32,
    pub(super) vk: u32,
    pub(super) hold: bool,
    /// The key is still down. When it is not, the clearing key-up goes out
    /// now (and a hold press ends at once); when it is, both wait for its
    /// key-up.
    pub(super) still_down: bool,
}

/// What a key event asks of the hook.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct KeyOutcome {
    /// A press is waiting on its `WM_HOTKEY`: make sure the timer runs.
    pub(super) arm_timer: bool,
    /// A handled hold press came up: send `HoldReleased`. Only ever the hold
    /// binding's id.
    pub(super) release: Option<i32>,
    /// Send the clearing key-up for this key.
    pub(super) clear: Option<u32>,
}

/// The modifier keys, as a hook reports them, each with the `MOD_*` bit it
/// stands for. Hardware reports the left/right codes; injected input (our own
/// Ctrl+V among it) may use the generic ones.
const MODIFIER_KEYS: [(u32, u32); 11] = [
    (0x10, MOD_SHIFT.0),   // VK_SHIFT
    (0xA0, MOD_SHIFT.0),   // VK_LSHIFT
    (0xA1, MOD_SHIFT.0),   // VK_RSHIFT
    (0x11, MOD_CONTROL.0), // VK_CONTROL
    (0xA2, MOD_CONTROL.0), // VK_LCONTROL
    (0xA3, MOD_CONTROL.0), // VK_RCONTROL
    (0x12, MOD_ALT.0),     // VK_MENU
    (0xA4, MOD_ALT.0),     // VK_LMENU
    (0xA5, MOD_ALT.0),     // VK_RMENU
    (0x5B, MOD_WIN.0),     // VK_LWIN
    (0x5C, MOD_WIN.0),     // VK_RWIN
];

/// The watch's state. Pure: every input carries its own time, so the tests
/// drive it without a keyboard.
#[derive(Debug)]
pub(super) struct Watch {
    keys: Vec<Key>,
    /// Which of [`MODIFIER_KEYS`] are down, one bit each.
    modifiers_down: u16,
    /// Fresh presses of a watched key, and drops handled, since the last report.
    presses: u32,
    rescues: u32,
}

impl Watch {
    /// `bindings` is `(id, mods, vk, hold)` for each keyboard binding, `mods`
    /// as parsed (with `MOD_NOREPEAT`). Every binding starts unregistered.
    pub(super) fn new(bindings: &[(i32, u32, u32, bool)]) -> Self {
        let keys = bindings
            .iter()
            .map(|&(id, mods, vk, hold)| Key {
                id,
                vk,
                mods: mods & !MOD_NOREPEAT.0,
                hold,
                registered: false,
                down: false,
                last_down: None,
                pending: None,
                rescued: None,
                release_owed: false,
                clear_owed: false,
            })
            .collect();
        Self {
            keys,
            modifiers_down: 0,
            presses: 0,
            rescues: 0,
        }
    }

    pub(super) fn set_registered(&mut self, id: i32, registered: bool) {
        for k in self.keys.iter_mut().filter(|k| k.id == id) {
            k.registered = registered;
        }
    }

    /// The `MOD_*` bits of the modifier keys down right now.
    fn held_mods(&self) -> u32 {
        MODIFIER_KEYS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.modifiers_down & (1 << i) != 0)
            .fold(0, |acc, (_, &(_, bit))| acc | bit)
    }

    /// One key event from the hook.
    pub(super) fn on_key(&mut self, vk: u32, down: bool, now: Instant) -> KeyOutcome {
        if let Some(i) = MODIFIER_KEYS.iter().position(|&(m, _)| m == vk) {
            if down {
                self.modifiers_down |= 1 << i;
            } else {
                self.modifiers_down &= !(1 << i);
            }
            return KeyOutcome::default();
        }
        let mods = self.held_mods();
        let mut out = KeyOutcome::default();
        let mut presses = 0;
        for k in self.keys.iter_mut().filter(|k| k.vk == vk) {
            if down {
                let fresh = !k.down
                    || k.last_down
                        .is_none_or(|t| now.duration_since(t) > LOST_UP_GAP);
                k.down = true;
                k.last_down = Some(now);
                if fresh && k.registered && k.mods == mods {
                    k.pending = Some(now);
                    k.rescued = None;
                    out.arm_timer = true;
                    presses += 1;
                }
            } else {
                k.down = false;
                if std::mem::take(&mut k.release_owed) {
                    out.release = Some(k.id);
                }
                if std::mem::take(&mut k.clear_owed) {
                    out.clear = Some(k.vk);
                }
            }
        }
        self.presses += presses;
        out
    }

    /// A `WM_HOTKEY` for binding `id`. Returns whether to act on it: false
    /// only for the late copy of a press the watch already handled.
    pub(super) fn on_hotkey(&mut self, id: i32, now: Instant) -> bool {
        let Some(k) = self.keys.iter_mut().find(|k| k.id == id) else {
            return true;
        };
        if k.pending.take().is_some() {
            return true;
        }
        if k.rescued
            .take()
            .is_some_and(|t| now.duration_since(t) < LATE_HOTKEY)
        {
            return false;
        }
        true
    }

    /// The watch timer fired. Returns the dropped presses, and whether a press
    /// is still waiting (so the timer must run again).
    pub(super) fn on_timer(&mut self, now: Instant) -> (Vec<Rescue>, bool) {
        let mut rescues = Vec::new();
        let mut waiting = false;
        for k in &mut self.keys {
            let Some(at) = k.pending else {
                continue;
            };
            if now.duration_since(at) < DROPPED_AFTER {
                waiting = true;
                continue;
            }
            k.pending = None;
            k.rescued = Some(now);
            k.release_owed = k.hold && k.down;
            k.clear_owed = k.down;
            rescues.push(Rescue {
                id: k.id,
                vk: k.vk,
                hold: k.hold,
                still_down: k.down,
            });
        }
        self.rescues += rescues.len() as u32;
        (rescues, waiting)
    }

    /// Presses seen and drops handled since the last call, then zero both.
    fn take_counts(&mut self) -> (u32, u32) {
        (
            std::mem::take(&mut self.presses),
            std::mem::take(&mut self.rescues),
        )
    }
}

/// The watch as the hotkey thread runs it.
struct Live {
    watch: Watch,
    tx: Sender<HotkeyEvent>,
    /// The watch timer while it runs, else 0.
    timer: usize,
    noted_injected: bool,
    ticks: u32,
}

thread_local! {
    static LIVE: RefCell<Option<Live>> = const { RefCell::new(None) };
}

/// The installed hook, 0 when none.
static HOOK: AtomicIsize = AtomicIsize::new(0);

/// Start watching `bindings` (see [`Watch::new`]). Call on the hotkey thread,
/// before its message loop, and only with at least one keyboard binding.
pub(super) fn start(bindings: &[(i32, u32, u32, bool)], tx: Sender<HotkeyEvent>) {
    LIVE.with_borrow_mut(|live| {
        *live = Some(Live {
            watch: Watch::new(bindings),
            tx,
            timer: 0,
            noted_injected: false,
            ticks: 0,
        });
    });
    install();
}

/// Stop watching: remove the hook and the timer.
pub(super) fn stop() {
    let raw = HOOK.swap(0, Ordering::AcqRel);
    if raw != 0 {
        let _ = unsafe { UnhookWindowsHookEx(HHOOK(raw as *mut core::ffi::c_void)) };
    }
    if let Some(live) = LIVE.with_borrow_mut(Option::take) {
        if live.timer != 0 {
            let _ = unsafe { KillTimer(HWND::default(), live.timer) };
        }
    }
}

/// Install the hook, replacing any earlier one: new first, then the old one
/// unhooked, the same way and for the same reason as
/// [`crate::mouse_hook::ensure_installed`] (Windows removes a slow hook without
/// a word, so a stored handle proves nothing). Returns whether a hook is live.
pub(super) fn install() -> bool {
    if LIVE.with_borrow(Option::is_none) {
        return true;
    }
    let module = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) };
    let hmod = match module {
        Ok(m) => windows::Win32::Foundation::HINSTANCE(m.0),
        Err(e) => {
            tracing::warn!("hotkey watch: GetModuleHandleW failed: {e}");
            return false;
        }
    };
    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), hmod, 0) } {
        Ok(h) => {
            let old = HOOK.swap(h.0 as isize, Ordering::AcqRel);
            if old == 0 {
                tracing::info!("hotkey watch installed");
            } else {
                let _ = unsafe { UnhookWindowsHookEx(HHOOK(old as *mut core::ffi::c_void)) };
            }
            true
        }
        Err(e) => {
            tracing::warn!("hotkey watch: SetWindowsHookExW failed: {e} (will retry on re-arm)");
            false
        }
    }
}

/// Record whether binding `id` is registered with Windows right now.
pub(super) fn set_registered(id: i32, registered: bool) {
    LIVE.with_borrow_mut(|live| {
        if let Some(live) = live {
            live.watch.set_registered(id, registered);
        }
    });
}

/// A `WM_HOTKEY` for binding `id` arrived. Returns whether to act on it.
pub(super) fn on_hotkey(id: i32) -> bool {
    let act = LIVE.with_borrow_mut(|live| {
        live.as_mut()
            .is_none_or(|live| live.watch.on_hotkey(id, Instant::now()))
    });
    if !act {
        tracing::info!(
            "WM_HOTKEY id={id} arrived late, after the watch had already handled that press; ignored"
        );
    }
    act
}

/// Whether `timer` (a `WM_TIMER`'s `wParam`) is the watch timer.
pub(super) fn is_watch_timer(timer: usize) -> bool {
    timer != 0 && LIVE.with_borrow(|live| live.as_ref().is_some_and(|l| l.timer == timer))
}

/// The watch timer fired: stop it, start it again if a press is still waiting,
/// and return the presses Windows dropped.
pub(super) fn on_timer() -> Vec<Rescue> {
    LIVE.with_borrow_mut(|live| {
        let Some(live) = live else {
            return Vec::new();
        };
        if live.timer != 0 {
            let _ = unsafe { KillTimer(HWND::default(), live.timer) };
            live.timer = 0;
        }
        let (rescues, waiting) = live.watch.on_timer(Instant::now());
        if waiting {
            live.timer = unsafe { SetTimer(HWND::default(), 0, TIMER_MS, None) };
        }
        rescues
    })
}

/// One re-arm tick: reinstall the hook, and every half hour log what the
/// watch saw, so the log shows it was alive across any stretch the hotkey
/// seemed dead.
pub(super) fn rearm() {
    install();
    let report = LIVE.with_borrow_mut(|live| {
        let live = live.as_mut()?;
        live.ticks += 1;
        if live.ticks < REPORT_EVERY_TICKS {
            return None;
        }
        live.ticks = 0;
        Some(live.watch.take_counts())
    });
    if let Some((presses, rescues)) = report {
        tracing::info!(
            "hotkey watch: {presses} hotkey press(es) seen in the last 30 min, \
             {rescues} of them dropped by Windows and handled here"
        );
    }
}

/// Send one key-up for `vk`, marked as ours. Clears a key Windows still holds
/// as down after its press was dropped; the app with focus sees a lone key-up,
/// which does nothing.
pub(super) fn send_key_up(vk: u32) {
    let scan = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) } as u16;
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk as u16),
                wScan: scan,
                dwFlags: KEYEVENTF_KEYUP,
                time: 0,
                dwExtraInfo: OUR_KEY_UP,
            },
        },
    };
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if sent == 1 {
        tracing::info!(
            "hotkey watch: sent one key-up for vk=0x{vk:02X} to clear Windows' key state"
        );
    } else {
        tracing::warn!("hotkey watch: SendInput refused the key-up for vk=0x{vk:02X}");
    }
}

/// The hook's half: feed one key event to the watch and carry out what it
/// asks. Never fails and never swallows the key.
fn on_key_event(vk: u32, down: bool, injected: bool) {
    let mut note_injected = false;
    let outcome = LIVE.with(|cell| {
        // Only this thread touches the watch and nothing it does while holding
        // it pumps messages, so the borrow is always free here. If that ever
        // stops being true, skip the event: a panic would cross the FFI
        // boundary and take the whole app down.
        let mut guard = cell.try_borrow_mut().ok()?;
        let live = guard.as_mut()?;
        let out = live.watch.on_key(vk, down, Instant::now());
        if out.arm_timer && live.timer == 0 {
            live.timer = unsafe { SetTimer(HWND::default(), 0, TIMER_MS, None) };
        }
        if out.arm_timer && injected && !live.noted_injected {
            live.noted_injected = true;
            note_injected = true;
        }
        if out.release.is_some() {
            let _ = live.tx.send(HotkeyEvent::HoldReleased);
        }
        Some(out)
    });
    if note_injected {
        tracing::info!(
            "hotkey key vk=0x{vk:02X} arrives as injected input (keyboard software such as \
             Logitech G HUB, or Remote Desktop); watched the same. Noted once."
        );
    }
    if let Some(vk) = outcome.and_then(|o| o.clear) {
        // Sent from the message loop, not from inside the hook.
        let _ = unsafe {
            PostThreadMessageW(
                GetCurrentThreadId(),
                WM_WATCH_KEY_UP,
                WPARAM(vk as usize),
                LPARAM(0),
            )
        };
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A negative code means "pass it on without inspecting", per the Win32
    // hook contract.
    if code == HC_ACTION as i32 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        if info.dwExtraInfo != OUR_KEY_UP {
            let message = wparam.0 as u32;
            let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
            on_key_event(info.vkCode, down, info.flags.0 & LLKHF_INJECTED.0 != 0);
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}
