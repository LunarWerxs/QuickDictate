//! The UI thread's per-tick state machine: what it remembers between ticks,
//! and the read-only snapshot of the app it recomputes on each one.

use std::sync::atomic::Ordering;

use anyhow::Result;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::state::{App, ErrorKind, Status};

use super::*;

/// Per-tick snapshot of read-only app/config state the loop body branches
/// on. Recomputed fresh every pass by [`compute_tick_snapshot`]; nothing
/// here persists across ticks (that's [`UiLoopState`]'s job).
pub(super) struct TickSnapshot {
    pub(super) status: Status,
    pub(super) error_kind: ErrorKind,
    pub(super) hotkey_blocked: bool,
    pub(super) render_status: Status,
    pub(super) render_kind: ErrorKind,
    pub(super) active_visible: bool,
    pub(super) want_visible: bool,
    pub(super) show_spinner: bool,
    pub(super) target_count: f32,
    pub(super) hide_tray_icon: bool,
}

/// Everything [`run`]'s poll loop carries from one pass to the next -- the
/// tray, the pip window, and every "last we drew/said X" comparison value
/// that makes each tick a diff against the previous one. Bundled into one
/// struct because most phases of the loop body read or write several of
/// these fields at once; threading a dozen `&mut` locals through free
/// functions instead would trip clippy's `too_many_arguments` on the first
/// extraction.
pub(super) struct UiLoopState {
    pub(super) tray: TrayState,
    pub(super) overlay: Overlay,
    pub(super) wake_event: windows::Win32::Foundation::HANDLE,
    pub(super) default_tooltip: String,
    pub(super) last_hide_tray_icon: bool,
    pub(super) last_status: Status,
    pub(super) last_error_kind: ErrorKind,
    pub(super) last_pos: Option<POINT>,
    pub(super) last_word_count: u32,
    pub(super) last_spinner: bool,
    pub(super) spinner_angle: f32,
    pub(super) error_tooltip_kind: Option<ErrorKind>,
    pub(super) hotkey_tooltip_active: bool,
    pub(super) update_tooltip_tag: Option<String>,
    pub(super) last_history_version: u64,
    pub(super) display_count: f32,
}

impl UiLoopState {
    pub(super) fn new(app: &App) -> Result<Self> {
        let tray = build_tray()?;

        // The auto-reset event this thread idles on. `App::wake_ui` signals it on
        // every status change; `MsgWaitForMultipleObjectsEx` below also wakes on
        // any win32 message, so tray clicks and menu commands are handled the
        // instant they arrive rather than at the next idle tick. Never closed:
        // it lives exactly as long as the process.
        let wake_event = unsafe {
            windows::Win32::System::Threading::CreateEventW(None, false, false, None)
                .map_err(|e| anyhow::anyhow!("CreateEventW for the UI wake event: {e}"))?
        };
        app.register_ui_wake_event(wake_event.0 as isize);

        // Register the cross-instance "show Settings" message before the overlay
        // window (whose wnd_proc handles it) is created, so there's no window
        // that could receive WM_CREATE etc. before the id is known. Idempotent.
        let _ = activate_message_id();

        // Apply the persisted hide-tray-icon setting immediately, before the
        // window is ever shown -- otherwise the icon would flash visible for one
        // frame on every launch. Live changes are picked up below in the poll
        // loop.
        let last_hide_tray_icon = app.config.load().hide_tray_icon;
        if last_hide_tray_icon {
            if let Err(e) = tray.tray.set_visible(false) {
                tracing::warn!("tray: initial set_visible(false) failed: {e}");
            }
        }

        let overlay = unsafe { Overlay::create(PIP_SIZE)? };
        tracing::info!("overlay hwnd={:?}", overlay.hwnd.0);

        Ok(Self {
            tray,
            overlay,
            wake_event,
            // Tray-tooltip explanation for the last dictation error. The 2s error pip
            // clears fast, so this persists the "why" (one line per cause, from
            // `ErrorKind::tooltip()`) on the tray icon's hover text until a dictation
            // actually connects again (or the app restarts).
            default_tooltip: format!("QuickDictate v{}", env!("CARGO_PKG_VERSION")),
            last_hide_tray_icon,
            last_status: Status::Idle,
            last_error_kind: ErrorKind::Generic,
            last_pos: None,
            last_word_count: u32::MAX,
            last_spinner: false,
            spinner_angle: 0.0,
            error_tooltip_kind: None,
            // Tray-tooltip explanation for a hotkey another process has claimed.
            // Tracked separately from `error_tooltip_kind` above because it isn't
            // gated on `Status::Error` at all -- the hotkey re-arm loop in
            // `hotkeys.rs` polls independently of the dictation session state
            // machine -- so it clears the instant `hotkeys_blocked()` goes false
            // instead of waiting for a Listening status.
            hotkey_tooltip_active: false,
            // Tag of the update currently advertised on the tooltip, so the text is
            // only rewritten when it actually changes.
            update_tooltip_tag: None,
            // Rebuild the "Recent transcriptions" submenu only when the history has
            // actually changed since we last drew it (cheap version counter, see
            // `TranscriptHistory::version`), not on every poll tick.
            last_history_version: u64::MAX,
            // Smoothed display counter — lerps toward the live word count so the pip
            // animates instead of snapping. Asymmetric rates: fast on the way up
            // (feels responsive), slow on the way down (damps STT revision jitter).
            display_count: 0.0,
        })
    }

    /// Keep the "Recent transcriptions" submenu in sync with the app's
    /// history. Cheap to check every tick (one lock + an integer compare);
    /// only rebuilds the actual menu items when it changed.
    pub(super) fn refresh_history_menu(&mut self, app: &App) {
        let version = app.history.lock().version();
        if version != self.last_history_version {
            let snapshot = app.history.lock().snapshot();
            self.tray.rebuild_history_menu(&snapshot);
            self.last_history_version = version;
        }
    }

    pub(super) fn update_spinner_angle(&mut self, show_spinner: bool) {
        if show_spinner {
            self.spinner_angle =
                (self.spinner_angle + std::f32::consts::TAU / 48.0) % std::f32::consts::TAU;
        } else {
            self.spinner_angle = 0.0;
        }
    }

    /// Persist an explanation of the last dictation error on the tray
    /// tooltip, using `ErrorKind::tooltip()` so every named cause gets real
    /// text instead of collapsing to a bare "!" -- keep it there until a
    /// session actually connects (Listening) so the explanation outlives the
    /// brief error pip. See `error_glyph` for the pip's side of the same fix.
    pub(super) fn sync_error_tooltip(&mut self, tick: &TickSnapshot) {
        if tick.hotkey_blocked {
            return;
        }
        if tick.status == Status::Error {
            if self.error_tooltip_kind != Some(tick.error_kind) {
                let text = format!("QuickDictate: {}", tick.error_kind.tooltip());
                let _ = self.tray.tray.set_tooltip(Some(&text));
                self.error_tooltip_kind = Some(tick.error_kind);
            }
        } else if self.error_tooltip_kind.is_some() && tick.status == Status::Listening {
            let _ = self.tray.tray.set_tooltip(Some(&self.default_tooltip));
            self.error_tooltip_kind = None;
        }
    }

    /// A blocked hotkey isn't a dictation error, so it doesn't share the
    /// persist-until-Listening lifetime above -- it clears the moment
    /// `hotkeys_blocked()` does.
    pub(super) fn sync_hotkey_tooltip(&mut self, tick: &TickSnapshot) {
        if tick.hotkey_blocked {
            if !self.hotkey_tooltip_active {
                let text = format!("QuickDictate: {}", ErrorKind::HotkeyBlocked.tooltip());
                let _ = self.tray.tray.set_tooltip(Some(&text));
                self.hotkey_tooltip_active = true;
            }
        } else if self.hotkey_tooltip_active {
            let _ = self.tray.tray.set_tooltip(Some(&self.default_tooltip));
            self.hotkey_tooltip_active = false;
        }
    }

    /// A waiting update. Since v0.5.4 the daily check reports rather than
    /// installs (see update.rs), so this tooltip is how a user learns an
    /// update exists without opening About. Lowest priority of the three: an
    /// error or a blocked hotkey is more urgent and has already claimed the
    /// tooltip above.
    pub(super) fn sync_update_tooltip(&mut self) {
        if self.error_tooltip_kind.is_some() || self.hotkey_tooltip_active {
            return;
        }
        let waiting = crate::update::pending_update();
        if waiting != self.update_tooltip_tag {
            let text = match waiting.as_deref() {
                Some(tag) => format!("QuickDictate: update available (v{tag})"),
                None => self.default_tooltip.clone(),
            };
            let _ = self.tray.tray.set_tooltip(Some(&text));
            self.update_tooltip_tag = waiting;
        }
    }

    /// Live-apply the hide-tray-icon setting whenever it changes -- no
    /// restart needed. tray-icon 0.19's set_visible is a thin wrapper over
    /// Shell_NotifyIconW(NIM_MODIFY) on Windows, so this is cheap enough to
    /// check every tick.
    pub(super) fn apply_hide_tray_icon_live(&mut self, tick: &TickSnapshot) {
        if tick.hide_tray_icon != self.last_hide_tray_icon {
            if let Err(e) = self.tray.tray.set_visible(!tick.hide_tray_icon) {
                tracing::warn!("tray: set_visible({}) failed: {e}", !tick.hide_tray_icon);
            }
            self.last_hide_tray_icon = tick.hide_tray_icon;
        }
    }

    /// Smooth the counter toward the live word count. Asymmetric lerp: fast
    /// counting up (responsive), slow counting down (damps STT
    /// partial-transcript revision jitter so the pip doesn't snap back).
    pub(super) fn update_display_count(&mut self, tick: &TickSnapshot) {
        if tick.want_visible && !tick.show_spinner {
            let rate = if tick.target_count > self.display_count {
                0.50
            } else {
                0.15
            };
            self.display_count += (tick.target_count - self.display_count) * rate;
        } else {
            self.display_count = 0.0;
        }
    }

    /// Move the pip to the cursor and repaint it (or hide it) for this tick.
    pub(super) fn render_pip_or_hide(&mut self, tick: &TickSnapshot) {
        let smooth_count = self.display_count.round() as u32;
        unsafe {
            if tick.want_visible {
                let mut p = POINT::default();
                if GetCursorPos(&mut p).is_ok() {
                    let pos_changed =
                        !matches!(self.last_pos, Some(prev) if prev.x == p.x && prev.y == p.y);
                    let status_changed = tick.render_status != self.last_status;
                    let count_changed = smooth_count != self.last_word_count;
                    let spinner_changed = tick.show_spinner != self.last_spinner;
                    // The error glyph depends on the kind, so a kind flip while
                    // the status stays Error must still repaint (two back-to-back
                    // errors of different kinds within the 2s pip window).
                    let kind_changed = tick.render_kind != self.last_error_kind;
                    // Render whenever anything changes — the smoothed counter
                    // changes most frames during active dictation, giving a
                    // fluid animation.
                    if pos_changed
                        || status_changed
                        || count_changed
                        || kind_changed
                        || spinner_changed
                        || tick.show_spinner
                    {
                        self.overlay.render(
                            tick.render_status,
                            tick.render_kind,
                            smooth_count,
                            tick.show_spinner.then_some(self.spinner_angle),
                            p.x + PIP_OFFSET_X,
                            p.y + PIP_OFFSET_Y,
                        );
                        self.last_pos = Some(p);
                        self.last_word_count = smooth_count;
                        self.last_error_kind = tick.render_kind;
                    }
                }
            } else if self.last_status != Status::Idle || self.last_pos.is_some() {
                self.overlay.hide();
                self.last_pos = None;
                self.last_word_count = u32::MAX;
            }
        }
        self.last_status = tick.render_status;
        self.last_spinner = tick.show_spinner;
    }

    /// Fast cadence only while a real dictation needs the pip to track the
    /// cursor smoothly (a plain sleep, deliberately NOT message-woken:
    /// WM_MOUSEMOVE floods the queue while the follower pip is under the
    /// cursor, and waking per mouse message would spin the loop far faster
    /// than the 16 ms render cadence). Idle waits on
    /// MsgWaitForMultipleObjectsEx instead of sleeping, so it wakes
    /// immediately for EITHER a win32 message (tray click, menu command, the
    /// second-launch activate request, paint) or the wake event that
    /// `App::wake_ui` signals on every status change, with the long timeout
    /// purely as a backstop for unsignalled changes (a config flag flipped
    /// by Settings, history growth).
    pub(super) fn wait_for_next_tick(&self, tick: &TickSnapshot) {
        let wait = poll_interval(tick.active_visible);
        if tick.active_visible {
            std::thread::sleep(wait);
        } else {
            use windows::Win32::UI::WindowsAndMessaging::{
                MsgWaitForMultipleObjectsEx, MWMO_INPUTAVAILABLE, QS_ALLINPUT,
            };
            // MWMO_INPUTAVAILABLE: return immediately if input is ALREADY in
            // the queue, not only for input arriving after the call -- without
            // it a message posted between our PeekMessage drain and this wait
            // would sit unprocessed for the whole timeout.
            let _ = unsafe {
                MsgWaitForMultipleObjectsEx(
                    Some(&[self.wake_event]),
                    wait.as_millis() as u32,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                )
            };
        }
    }
}

/// A hotkey another process has claimed is surfaced the same way a dictation
/// error is (tray tooltip text, a pip glyph) even though the app itself is
/// genuinely Idle -- the re-arm loop in `hotkeys.rs` polls independently of
/// this session state machine, so it's synthesized here rather than being a
/// real `Status`/`ErrorKind` pair out of `App`. Only surfaced while Idle: an
/// active dictation always wins.
pub(super) fn compute_tick_snapshot(app: &App) -> TickSnapshot {
    let status = app.status();
    let error_kind = app.error_kind();
    let cfg = app.config.load();
    let target_count = app.word_count.load(Ordering::Acquire) as f32;

    let hotkey_blocked = status == Status::Idle && crate::hotkeys::hotkeys_blocked();
    let (render_status, render_kind) = if hotkey_blocked {
        (Status::Error, ErrorKind::HotkeyBlocked)
    } else {
        (status, error_kind)
    };

    // `active_visible` (a real dictation in progress) drives the fast
    // active-poll cadence at the bottom of the loop; `hotkey_pip_visible`
    // rides whatever cadence the loop is already ticking at instead of
    // forcing the fast one, since a stuck hotkey has no animation to keep
    // smooth and could otherwise pin the loop at 16ms indefinitely.
    let active_visible = cfg.mouse_follower_enabled && status != Status::Idle;
    let hotkey_pip_visible = cfg.mouse_follower_enabled && hotkey_blocked;
    let want_visible = active_visible || hotkey_pip_visible;

    let show_spinner = cfg.stt_provider.eq_ignore_ascii_case("local")
        && matches!(
            status,
            Status::Starting | Status::Listening | Status::Processing
        );

    TickSnapshot {
        status,
        error_kind,
        hotkey_blocked,
        render_status,
        render_kind,
        active_visible,
        want_visible,
        show_spinner,
        target_count,
        hide_tray_icon: cfg.hide_tray_icon,
    }
}
