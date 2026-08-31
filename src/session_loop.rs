//! The hotkey/session loop: one event in, one session state change out.
//!
//! Also owns the queueing that keeps a hotkey press during slow local
//! inference from being swallowed, and the key-pool rebuild a settings change
//! makes necessary between sessions.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::hotkeys::{HotkeyEvent, HotkeyManager};
use crate::keys::KeyPool;
use crate::state::{App, Status};
use crate::stt::{self, SttHandle};

fn refresh_key_pool(app: &Arc<App>, keys: &mut Arc<KeyPool>) {
    let cfg = app.config.load();
    if keys.matches_config(&cfg) {
        return;
    }
    tracing::info!(
        "provider or keys changed; rebuilding the '{}' key pool",
        cfg.stt_provider
    );
    *keys = KeyPool::new(&cfg);
    if cfg.prewarm_keys {
        stt::spawn_prewarm(Arc::clone(app), Arc::clone(keys));
    }
}

fn status_after_release(provider: &str) -> Status {
    if provider.eq_ignore_ascii_case("local") {
        Status::Processing
    } else {
        Status::Idle
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingStart {
    Toggle,
    Hold,
}

fn handle_processing_hotkey(pending: &mut Option<PendingStart>, event: HotkeyEvent) -> bool {
    match event {
        HotkeyEvent::TogglePressed => *pending = Some(PendingStart::Toggle),
        HotkeyEvent::HoldPressed => *pending = Some(PendingStart::Hold),
        HotkeyEvent::HoldReleased => {
            if *pending == Some(PendingStart::Hold) {
                *pending = None;
            }
        }
        HotkeyEvent::ToggleLongPressed => {
            *pending = None;
            return false;
        }
    }
    true
}

fn start_queued_session_if_idle(
    app: &Arc<App>,
    keys: &mut Arc<KeyPool>,
    active: &mut Option<SttHandle>,
    pending: &mut Option<PendingStart>,
) {
    if app.status() != Status::Idle {
        return;
    }
    let Some(kind) = pending.take() else {
        return;
    };
    let _ = active.take();
    refresh_key_pool(app, keys);
    tracing::info!("Starting queued {kind:?} session after local processing");
    app.set_status(Status::Starting);
    *active = Some(stt::start_session(Arc::clone(app), Arc::clone(keys)));
}

/// Log the outcome of `handle_processing_hotkey` queuing a hotkey while local
/// processing was still finishing the previous dictation.
fn log_processing_hotkey_queue(evt: HotkeyEvent, prior_pending: Option<PendingStart>) {
    match evt {
        HotkeyEvent::TogglePressed => {
            tracing::info!(
                "queued toggle start while the local model finishes the previous dictation"
            );
        }
        HotkeyEvent::HoldPressed => {
            tracing::info!(
                "queued hold start while the local model finishes the previous dictation"
            );
        }
        HotkeyEvent::HoldReleased => {
            if prior_pending == Some(PendingStart::Hold) {
                tracing::info!(
                    "cancelled queued hold start because the key was released before local processing finished"
                );
            }
        }
        HotkeyEvent::ToggleLongPressed => unreachable!("not consumed above"),
    }
}

/// Apply one hotkey event to the live session: start, stop, or trigger a
/// saved-transcription replay, depending on the event and whether a session
/// is currently live.
fn handle_hotkey_event(
    app: &Arc<App>,
    keys: &mut Arc<KeyPool>,
    active: &mut Option<SttHandle>,
    pending_start: &mut Option<PendingStart>,
    evt: HotkeyEvent,
    has_live: bool,
) {
    match evt {
        HotkeyEvent::TogglePressed => {
            if has_live {
                app.set_status(status_after_release(&app.config.load().stt_provider));
                if let Some(h) = active.take() {
                    tracing::info!("Stopping session (toggle off)");
                    h.stop();
                }
            } else {
                // Drop any prior completed handle without touching its
                // shared state; the background task will finish on its own.
                let _ = active.take();
                refresh_key_pool(app, keys);
                tracing::info!("Starting session (toggle on)");
                app.set_status(Status::Starting);
                *active = Some(stt::start_session(Arc::clone(app), Arc::clone(keys)));
            }
        }
        HotkeyEvent::ToggleLongPressed => {
            *pending_start = None;
            if let Some(h) = active.take() {
                tracing::info!("Discarding active session for saved-transcription replay");
                app.invalidate_current_session();
                h.stop();
            }
            app.word_count.store(0, Ordering::Release);
            app.set_status(Status::Idle);
            // try_send, never send: this runs on the win32 message-pump
            // thread. A blocking send on a full queue would freeze the
            // tray, the hotkeys, and every window this process owns until
            // the paste worker drained. Dropping one replay request is a
            // far better outcome than a frozen app.
            if let Err(e) = app.replay_tx.try_send(None) {
                tracing::warn!("saved-transcription replay request dropped: {e}");
            }
        }
        HotkeyEvent::HoldPressed => {
            if !has_live {
                let _ = active.take();
                refresh_key_pool(app, keys);
                tracing::info!("Starting session (hold press)");
                app.set_status(Status::Starting);
                *active = Some(stt::start_session(Arc::clone(app), Arc::clone(keys)));
            }
        }
        HotkeyEvent::HoldReleased => {
            if has_live {
                app.set_status(status_after_release(&app.config.load().stt_provider));
                if let Some(h) = active.take() {
                    tracing::info!("Stopping session (hold release)");
                    h.stop();
                }
            } else {
                let _ = active.take();
                app.set_status(Status::Idle);
            }
        }
    }
}

/// The hotkey/session loop: waits for the next hotkey event (or a queued
/// session start once local processing frees up), applies it, and repeats
/// until shutdown is requested. Returns whatever session was still live so
/// the caller can stop it cleanly.
pub(crate) fn run_event_loop(
    app: &Arc<App>,
    keys: &mut Arc<KeyPool>,
    hotkeys: &HotkeyManager,
) -> Option<SttHandle> {
    let mut active: Option<SttHandle> = None;
    let mut pending_start: Option<PendingStart> = None;

    loop {
        if app.shutdown.load(Ordering::Acquire) {
            break;
        }
        start_queued_session_if_idle(app, keys, &mut active, &mut pending_start);
        let evt = match hotkeys.events.recv_timeout(Duration::from_millis(50)) {
            Ok(e) => e,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        // Processing may have completed while recv_timeout was blocked. Start
        // the already-queued session before interpreting a newly arrived event,
        // otherwise `pending_start` could survive into a later session.
        start_queued_session_if_idle(app, keys, &mut active, &mut pending_start);
        tracing::info!("hotkey event: {evt:?} (status={:?})", app.status());
        if app.status() == Status::Processing {
            let prior_pending = pending_start;
            if handle_processing_hotkey(&mut pending_start, evt) {
                log_processing_hotkey_queue(evt, prior_pending);
                continue;
            }
        }
        // Main owns the visible status. Streaming sessions may keep finalizing
        // while a newer one starts. Local batch inference is deliberately
        // serialized above: starting another epoch would make the generic
        // late-result guard discard the still-running local transcript.
        //
        // `active` tracks the *most recent* session. A handle whose `done`
        // flag is set means the session terminated on its own (clean or
        // errored); we treat it as "no live session" for hotkey purposes.
        let has_live = active.as_ref().map(|h| !h.is_done()).unwrap_or(false);
        handle_hotkey_event(app, keys, &mut active, &mut pending_start, evt, has_live);
    }

    active
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_release_stays_visible_while_batch_inference_finishes() {
        assert_eq!(status_after_release("local"), Status::Processing);
        assert_eq!(status_after_release("LOCAL"), Status::Processing);
        assert_eq!(status_after_release("elevenlabs"), Status::Idle);
    }

    #[test]
    fn local_processing_queues_toggle_and_cancellable_hold_starts() {
        let mut pending = None;
        assert!(handle_processing_hotkey(
            &mut pending,
            HotkeyEvent::TogglePressed
        ));
        assert_eq!(pending, Some(PendingStart::Toggle));

        assert!(handle_processing_hotkey(
            &mut pending,
            HotkeyEvent::HoldReleased
        ));
        assert_eq!(pending, Some(PendingStart::Toggle));

        assert!(handle_processing_hotkey(
            &mut pending,
            HotkeyEvent::HoldPressed
        ));
        assert_eq!(pending, Some(PendingStart::Hold));
        assert!(handle_processing_hotkey(
            &mut pending,
            HotkeyEvent::HoldReleased
        ));
        assert_eq!(pending, None);

        pending = Some(PendingStart::Toggle);
        assert!(!handle_processing_hotkey(
            &mut pending,
            HotkeyEvent::ToggleLongPressed
        ));
        assert_eq!(pending, None);
    }
}
