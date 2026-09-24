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

/// What the pip shows between release and the transcript landing. The local
/// model gets `Processing`, which also queues the next press (it cannot run two
/// sessions at once). Any other provider that says nothing until commit
/// (Google POSTs the recording then, OpenAI answers ~0.9 s after it) gets
/// `Finalizing`: the same spinner, no queue. A streaming provider has already
/// shown its words, so its pip goes as soon as you let go.
///
/// `provider` is the one the press actually runs on (a Per-App Profile may
/// pick another than `cfg.stt_provider`); `None`, before the press has
/// resolved it, falls back to the global choice.
fn status_after_release(cfg: &crate::config::Config, provider: Option<&str>) -> Status {
    let id = provider.unwrap_or(&cfg.stt_provider);
    if id.eq_ignore_ascii_case("local") {
        Status::Processing
    } else if !stt::provider_id_streams_interim_text(id, cfg) {
        Status::Finalizing
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

/// Decide whether `evt` only updates the queue or must be applied to the
/// session now: true means it was absorbed into the queue.
///
/// The queue lives only while local processing runs. An event met in any
/// other status first drops whatever is still queued: processing ended
/// without the loop seeing Idle (usually in Error, which never consumes the
/// queue), and this fresh press or release supersedes it. Kept, it would
/// start a session nobody asked for the next time status reached Idle, and a
/// queued hold would start after its key was already up.
fn absorb_into_queue(status: Status, pending: &mut Option<PendingStart>, evt: HotkeyEvent) -> bool {
    if status != Status::Processing {
        if let Some(stale) = pending.take() {
            tracing::info!("dropped queued {stale:?} start: processing ended in {status:?}");
        }
        return false;
    }
    let prior_pending = *pending;
    if !handle_processing_hotkey(pending, evt) {
        return false;
    }
    log_processing_hotkey_queue(evt, prior_pending);
    true
}

/// Start a new session and make it the tracked one. Any prior handle is a
/// completed session (callers check `has_live` first); it is dropped without
/// touching its shared state, and its background task finishes on its own.
fn begin_session(
    app: &Arc<App>,
    keys: &mut Arc<KeyPool>,
    active: &mut Option<SttHandle>,
    what: std::fmt::Arguments<'_>,
) {
    let _ = active.take();
    refresh_key_pool(app, keys);
    tracing::info!("{what}");
    app.set_status(Status::Starting);
    *active = Some(stt::start_session(Arc::clone(app), Arc::clone(keys)));
}

/// Stop the live session: show the provider's post-release status first,
/// then signal the session to finish.
fn end_session(app: &Arc<App>, active: &mut Option<SttHandle>, why: &str) {
    let provider = active.as_ref().and_then(SttHandle::provider);
    app.set_status(status_after_release(&app.config.load(), provider));
    if let Some(h) = active.take() {
        tracing::info!("Stopping session ({why})");
        h.stop();
    }
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
    begin_session(
        app,
        keys,
        active,
        format_args!("Starting queued {kind:?} session after local processing"),
    );
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
    evt: HotkeyEvent,
    has_live: bool,
) {
    match evt {
        HotkeyEvent::TogglePressed if has_live => end_session(app, active, "toggle off"),
        HotkeyEvent::TogglePressed => begin_session(
            app,
            keys,
            active,
            format_args!("Starting session (toggle on)"),
        ),
        HotkeyEvent::ToggleLongPressed => start_replay(app, active),
        HotkeyEvent::HoldPressed if !has_live => begin_session(
            app,
            keys,
            active,
            format_args!("Starting session (hold press)"),
        ),
        HotkeyEvent::HoldPressed => {}
        HotkeyEvent::HoldReleased if has_live => end_session(app, active, "hold release"),
        HotkeyEvent::HoldReleased => {
            let _ = active.take();
            app.set_status(Status::Idle);
        }
    }
}

/// The long press: discard any live session and ask the paste worker to
/// replay the saved transcription. The queue was already cleared on the way
/// here (by `handle_processing_hotkey` or `absorb_into_queue`).
fn start_replay(app: &Arc<App>, active: &mut Option<SttHandle>) {
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
        let status = app.status();
        tracing::info!("hotkey event: {evt:?} (status={status:?})");
        if absorb_into_queue(status, &mut pending_start, evt) {
            continue;
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
        handle_hotkey_event(app, keys, &mut active, evt, has_live);
    }

    active
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_release_stays_visible_while_batch_inference_finishes() {
        let with = |id: &str| crate::config::Config {
            stt_provider: id.into(),
            ..crate::config::Config::default()
        };
        assert_eq!(
            status_after_release(&with("local"), None),
            Status::Processing
        );
        assert_eq!(
            status_after_release(&with("LOCAL"), None),
            Status::Processing
        );
        // Commit-only cloud providers keep the pip up while the transcript is
        // on its way, without queueing the next press behind it.
        assert_eq!(
            status_after_release(&with("google"), None),
            Status::Finalizing
        );
        assert_eq!(
            status_after_release(&with("openai"), None),
            Status::Finalizing
        );
        for streaming in ["elevenlabs", "deepgram", "assemblyai", "dashscope"] {
            assert_eq!(status_after_release(&with(streaming), None), Status::Idle);
        }
    }

    #[test]
    fn the_pip_follows_the_provider_the_press_resolved_not_the_global_one() {
        let global_streaming = crate::config::Config {
            stt_provider: "elevenlabs".into(),
            ..crate::config::Config::default()
        };
        // A profile put this press on the local model: its release must queue
        // and spin, exactly as if Local were the global choice.
        assert_eq!(
            status_after_release(&global_streaming, Some("local")),
            Status::Processing
        );
        assert_eq!(
            status_after_release(&global_streaming, Some("google")),
            Status::Finalizing
        );
        let global_local = crate::config::Config {
            stt_provider: "local".into(),
            ..crate::config::Config::default()
        };
        assert_eq!(
            status_after_release(&global_local, Some("deepgram")),
            Status::Idle
        );
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

    /// Regression: a press queued during local processing outlived processing
    /// that ended in Error, then started a session nobody asked for the next
    /// time status reached Idle -- a queued hold after its key was already up.
    #[test]
    fn an_event_outside_processing_drops_the_stale_queue() {
        for queued in [PendingStart::Hold, PendingStart::Toggle] {
            for status in [Status::Error, Status::Idle, Status::Finalizing] {
                for evt in [HotkeyEvent::HoldReleased, HotkeyEvent::TogglePressed] {
                    let mut pending = Some(queued);
                    assert!(!absorb_into_queue(status, &mut pending, evt));
                    assert_eq!(pending, None, "{queued:?} survived {evt:?} in {status:?}");
                }
            }
        }
    }

    #[test]
    fn processing_still_absorbs_presses_into_the_queue() {
        let mut pending = None;
        assert!(absorb_into_queue(
            Status::Processing,
            &mut pending,
            HotkeyEvent::HoldPressed
        ));
        assert_eq!(pending, Some(PendingStart::Hold));
        assert!(absorb_into_queue(
            Status::Processing,
            &mut pending,
            HotkeyEvent::HoldReleased
        ));
        assert_eq!(pending, None);
        pending = Some(PendingStart::Toggle);
        // The long press is the one event processing does not absorb: it
        // clears the queue and goes on to the replay.
        assert!(!absorb_into_queue(
            Status::Processing,
            &mut pending,
            HotkeyEvent::ToggleLongPressed
        ));
        assert_eq!(pending, None);
    }
}
