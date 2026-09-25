//! The paste worker thread: it takes a finished transcript off the channel,
//! applies the per-app text processing, and decides how it reaches the screen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use unicode_segmentation::UnicodeSegmentation;

use crate::app_compat;
use crate::focus;
use crate::polish;
use crate::state::{App, ErrorKind};
use crate::text::{self, TextProcessor};
use crate::voice_commands::{self, ScratchThat};

use super::*;

/// Set by `main` once every session has finished finalizing (see
/// [`request_stop`]). The worker deliberately runs on past `App::shutdown`
/// until then: a press still live when the app is told to quit delivers its
/// transcript DURING that finalize, and a worker that had already left would
/// drop those words unpasted and out of history.
static STOP: AtomicBool = AtomicBool::new(false);

/// Every session has finalized: paste whatever is still queued, then exit.
pub(crate) fn request_stop() {
    STOP.store(true, Ordering::Release);
}

pub fn spawn(app: Arc<App>) -> std::thread::JoinHandle<()> {
    crate::threads::spawn_named("qd-output", move || run(app))
}

fn run(app: Arc<App>) {
    // Text processors are cached per (config snapshot, matched profile name)
    // -- the replacement-list regexes are expensive enough that compiling
    // them per paste shows up on a profile. The cache is invalidated whenever
    // the underlying Arc<Config> pointer changes (settings saved) and grows
    // by at most one entry per distinct profile actually hit, which is small
    // in practice.
    let mut current_cfg = app.config.load_full();
    let mut cache = ProcessorCache::new(&current_cfg);

    // Set to the dictation epoch whose last paste stopped mid-thought (see
    // [`text::ends_mid_sentence`]). The NEXT transcript from that same epoch
    // then continues that sentence instead of opening a new one.
    //
    // Gated on the epoch, not on a timer or on window focus, because one
    // hotkey press is exactly the span over which continuation is meaningful:
    // the hybrid paste flow can split a single press into a release flush plus
    // several live-append commits, and pressing the hotkey again is the user
    // starting a new thought. If the epoch has already moved on by the time we
    // get here (they re-pressed while this transcript was in flight) the flag
    // simply doesn't apply, which is the pre-existing behavior.
    let mut continue_within: Option<u64> = None;

    while !STOP.load(Ordering::Acquire) {
        crossbeam_channel::select! {
            recv(app.transcript_rx) -> raw => {
                let raw = match raw {
                    Ok(v) => v,
                    Err(_) => break,
                };

                let cfg = app.config.load_full();
                if !Arc::ptr_eq(&cfg, &current_cfg) {
                    tracing::debug!("output: config changed, rebuilding TextProcessor cache");
                    cache = ProcessorCache::new(&cfg);
                    current_cfg = cfg;
                }

                // Voice command detection, the optional polish pass, guarded
                // processing, and the paste itself all live in
                // `process_transcript` -- split out purely to keep `run`'s
                // cognitive load down. Every `continue` it used to hold was
                // already the last thing in this arm, so an early `return`
                // from the helper has the identical effect.
                process_transcript(&app, raw, &mut cache, &current_cfg, &mut continue_within);
            }
            recv(app.replay_rx) -> replay => {
                // Split out purely to keep `run`'s cognitive load down; the
                // one `break` case is reported back as a return value since
                // it can no longer target the loop directly.
                if handle_replay(&app, replay, &mut continue_within) {
                    break;
                }
            }
            default(Duration::from_millis(50)) => {}
        }
    }
    // The last words of a press that was live at shutdown can land between the
    // final select and the stop request: paste them too, then leave.
    while let Ok(raw) = app.transcript_rx.try_recv() {
        process_transcript(&app, raw, &mut cache, &current_cfg, &mut continue_within);
    }
}

/// A replay request off `app.replay_rx`: re-paste finished history without
/// continuing or opening a pending chunk. Returns whether the channel was
/// disconnected (the caller's cue to break its loop, since a `break` here
/// can no longer target it directly). Split out of `run` purely to keep its
/// cognitive load down.
fn handle_replay(
    app: &Arc<App>,
    replay: Result<Option<usize>, crossbeam_channel::RecvError>,
    continue_within: &mut Option<u64>,
) -> bool {
    // A replay re-pastes finished history, so it neither continues the
    // previous chunk nor leaves one open.
    *continue_within = None;
    let index = match replay {
        Ok(index) => index,
        Err(_) => return true,
    };
    // `None` = most recent (index 0) -- the original "replay last paste"
    // behavior; `Some(i)` = a specific history entry, e.g. from the tray's
    // "Recent transcriptions" submenu.
    let entry = {
        let history = app.history.lock();
        match index {
            Some(i) => history.get(i),
            None => history.most_recent(),
        }
    };
    match entry {
        Some(entry) if !entry.text.is_empty() => {
            let log_transcripts = app.config.load().log_transcripts;
            if log_transcripts {
                tracing::info!(
                    "replaying saved transcription ({} char(s)): {:?}",
                    entry.text.chars().count(),
                    preview(&entry.text)
                );
            } else {
                tracing::info!(
                    "replaying saved transcription ({} char(s))",
                    entry.text.chars().count()
                );
            }
            paste_processed(app, &entry.text, false, log_transcripts);
        }
        _ => tracing::warn!("replay requested, but no saved transcription is available"),
    }
    false
}

/// One transcript off `app.transcript_rx`: voice-command detection, the
/// optional polish pass, guarded processing, and the paste. Split out of
/// `run` purely to keep its cognitive load down -- see the call site.
fn process_transcript(
    app: &Arc<App>,
    raw: String,
    cache: &mut ProcessorCache,
    current_cfg: &crate::config::Config,
    continue_within: &mut Option<u64>,
) {
    // Voice Commands (precision subset): a FINAL transcript that ends with
    // "scratch that" undoes the previous pasted chunk instead of being
    // pasted itself. Checked on the *raw* transcript, before any text
    // processing, so the command phrase itself never goes through
    // replacements/punctuation.
    match voice_commands::detect(&raw, current_cfg.voice_commands) {
        ScratchThat::Triggered { remaining_raw } => {
            // The chunk this would have continued is being deleted, so
            // there is nothing left to continue.
            *continue_within = None;
            handle_scratch_that(app, &remaining_raw, cache, current_cfg);
            return;
        }
        ScratchThat::NotTriggered => {}
    }

    // Resolve the foreground window's exe at commit time (not when the
    // hotkey was pressed) -- the user may well have switched windows
    // mid-dictation.
    let exe_name = focus::foreground_exe_name();

    // The optional LLM cleanup pass, on the RAW transcript and before the
    // deterministic rules below -- the user's own replacements, dev-term
    // casing and punctuation settings are explicit instructions and must
    // win over a model's opinion. Bounded by `polish_deadline_ms`, usually
    // already answered by the speculation the session runner started while
    // they were still talking, and falls back to `raw` on any problem.
    let raw = match polish::settings_for(current_cfg, exe_name.as_deref()) {
        Some(settings) => app.polish.resolve(&settings, &raw).unwrap_or(raw),
        None => raw,
    };

    let processor = cache.get_or_build(current_cfg, exe_name.as_deref());

    let epoch = app.current_session_epoch();
    let continuing = *continue_within == Some(epoch);
    let Some(processed) = process_guarded(processor, &raw, continuing) else {
        *continue_within = None;
        return;
    };
    if processed.is_empty() {
        return;
    }
    *continue_within = text::ends_mid_sentence(&processed).then_some(epoch);
    paste_processed(app, &processed, true, current_cfg.log_transcripts);
}

/// Handles a recognized "scratch that" command: undoes the previously
/// pasted chunk (backspace count = its grapheme-cluster length -- the history
/// entry already holds the fully-processed text, i.e. exactly what was sent to
/// the target window, auto_space/auto_newline trailer included) and, if any
/// text preceded the command phrase, processes and pastes that as the new
/// chunk.
///
/// Refuses to fire unless that entry is the newest paste on record and focus
/// is still where it landed (see [`UndoTargets`]): blind backspaces into a
/// window QuickDictate did not write to would delete the user's own content.
///
/// If there is no previous chunk to undo, this is a no-op (logged at debug)
/// -- we never invent backspaces without a known prior paste. Only ever
/// undoes the single most recent chunk; repeated commands require repeated
/// "scratch that"s, each reaching one paste further back.
pub(super) fn handle_scratch_that(
    app: &App,
    remaining_raw: &str,
    cache: &mut ProcessorCache,
    cfg: &crate::config::Config,
) {
    let last = { app.history.lock().most_recent() };
    let Some(last) = last else {
        tracing::debug!("voice command: \"scratch that\" heard, but no previous paste to undo");
        return;
    };
    if !focus_unchanged_since_paste(last.id) {
        return;
    }

    // Backspace deletes one GRAPHEME CLUSTER in most editors, not one Unicode
    // scalar, so a family emoji (7 scalars, 1 glyph) would over-delete into
    // whatever preceded it if we counted `chars()`.
    let undo_count = last.text.graphemes(true).count();
    tracing::info!("voice command: \"scratch that\" -- undoing last paste ({undo_count} glyph(s))");
    if let Err(e) = send_backspaces(undo_count) {
        tracing::error!("voice command: backspace undo failed: {e:#}");
        return;
    }
    // Drop the now-undone entry so a second "scratch that" doesn't see the
    // same (already-removed) text as still "most recent" and re-undo it, and
    // its target with it so the paste before becomes the undoable one.
    app.undo_last_history();
    UNDO_TARGETS.lock().pop();

    if remaining_raw.trim().is_empty() {
        return;
    }

    let exe_name = focus::foreground_exe_name();
    let processor = cache.get_or_build(cfg, exe_name.as_deref());
    let Some(processed) = process_guarded(processor, remaining_raw, false) else {
        return;
    };
    if processed.is_empty() {
        return;
    }
    paste_processed(app, &processed, true, cfg.log_transcripts);
}

/// Whether history entry `entry_id` is the newest paste on record AND focus is
/// still in the window and control it was typed into. Backspaces are blind: if
/// the user alt-tabbed or clicked into another native field since the paste,
/// they would delete content QuickDictate never wrote. (Typing more into the
/// same field, or switching tabs inside an app that draws its own fields, is
/// invisible from here; see [`focus::foreground_focus_ids`].)
fn focus_unchanged_since_paste(entry_id: u64) -> bool {
    let target = UNDO_TARGETS.lock().target_for(entry_id).cloned();
    let Some(target) = target else {
        tracing::debug!("voice command: \"scratch that\" heard, but no paste target recorded");
        return false;
    };
    let now = PasteTarget::current();
    if now.as_ref() == Some(&target) {
        return true;
    }
    tracing::warn!(
        "voice command: \"scratch that\" ignored -- focus moved since the last paste \
         (now {:?}); refusing to send backspaces into a different window",
        now.and_then(|t| t.exe).as_deref().unwrap_or("<unknown>")
    );
    false
}

/// [`TextProcessor::process`] behind the same panic boundary as `paste()`:
/// it runs on network-derived transcript text, so a pathological input must
/// cost one paste, not the output thread. `None` means the processing
/// panicked (already logged).
fn process_guarded(processor: &TextProcessor, raw: &str, continuing: bool) -> Option<String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        processor.process_chunk(raw, continuing)
    })) {
        Ok(p) => Some(p),
        Err(_) => {
            tracing::error!("text processing PANICKED (caught; thread continues)");
            None
        }
    }
}

pub(super) fn paste_processed(
    app: &App,
    processed: &str,
    save_as_last: bool,
    log_transcripts: bool,
) {
    if log_transcripts {
        tracing::info!(
            "pasting {} char(s): {:?}",
            processed.chars().count(),
            preview(processed)
        );
    } else {
        tracing::info!("pasting {} char(s)", processed.chars().count());
    }
    let restore_delay_ms = app.config.load().clipboard_restore_delay_ms;

    // Where this is about to land, for "scratch that". Captured BEFORE
    // injection (by the time the keystrokes are consumed the foreground
    // window may already have changed) but only RECORDED once the text
    // actually got typed, so an undo can never chase a paste that failed.
    let target = PasteTarget::current();

    // The app-compatibility list: windows that ignore Ctrl+V, see another
    // clipboard or drop injected keys get the delivery that works there.
    let compat = app_compat::lookup(&app_compat::WindowFacts::current());
    if let Some(entry) = &compat {
        app_compat::announce(entry);
    }
    let delivery = compat
        .as_ref()
        .map_or(app_compat::Delivery::Auto, |entry| entry.delivery);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        paste(processed, restore_delay_ms, delivery)
    }));
    // The transcript goes into history on EVERY outcome, not just success.
    // A failed paste used to be lost three ways at once (not typed, not on the
    // clipboard, not replayable); keeping it here means the tray's "Recent
    // transcriptions" can always recover the words the user actually said.
    if save_as_last {
        app.record_history(processed.to_string());
    }
    let typed = matches!(result, Ok(Ok(PasteOutcome::Typed)));
    track_undo_target(app, (typed && save_as_last).then_some(target).flatten());
    match result {
        Ok(Ok(PasteOutcome::Typed)) => tracing::info!("paste OK"),
        Ok(Ok(PasteOutcome::LeftOnClipboard)) => {
            tracing::error!(
                "the focused window runs elevated, so Windows discards injected keystrokes; \
                 the transcription is on the clipboard, press Ctrl+V to paste it"
            );
            app.raise_error(ErrorKind::Elevated);
        }
        Ok(Ok(PasteOutcome::LeftForApp)) => {
            let entry = compat.as_ref();
            tracing::error!(
                "{} does not accept dictated text ({}); the transcription is on the clipboard, \
                 paste it yourself{}",
                entry.map_or("the focused window", |e| e.name.as_str()),
                entry
                    .and_then(|e| e.message.as_deref())
                    .unwrap_or("app-compatibility list"),
                entry
                    .and_then(|e| e.url.as_deref())
                    .map(|url| format!(" (more: {url})"))
                    .unwrap_or_default()
            );
            app.raise_error(ErrorKind::AppBlocked);
        }
        Ok(Err(e)) => {
            tracing::error!("paste failed: {e:#}");
            app.raise_error(ErrorKind::Generic);
        }
        Err(_) => {
            tracing::error!("paste PANICKED (caught; thread continues)");
            app.raise_error(ErrorKind::Generic);
        }
    }
}

/// Keep [`UNDO_TARGETS`] in step with the paste that just ran. `target` is
/// `Some` only for a paste that was typed AND recorded a history entry, which
/// becomes the newest undoable paste. Anything else (a replay, which records
/// no entry, a failed paste, text left on the clipboard) put an unknown number
/// of characters after every earlier paste, so none of those is safe to undo.
fn track_undo_target(app: &App, target: Option<PasteTarget>) {
    let entry_id = target
        .is_some()
        .then(|| app.history.lock().most_recent().map(|entry| entry.id))
        .flatten();
    let mut targets = UNDO_TARGETS.lock();
    match target.zip(entry_id) {
        Some((target, id)) => targets.push(id, target),
        None => targets.clear(),
    }
}
