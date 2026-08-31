//! The paste worker thread: it takes a finished transcript off the channel,
//! applies the per-app text processing, and decides how it reaches the screen.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use unicode_segmentation::UnicodeSegmentation;

use crate::focus;
use crate::polish;
use crate::state::{App, ErrorKind};
use crate::text::{self, TextProcessor};
use crate::voice_commands::{self, ScratchThat};

use super::*;

#[allow(
    clippy::expect_used,
    reason = "a thread that cannot be spawned at startup is unrecoverable; the panic message is the only diagnostic there is"
)]
pub fn spawn(app: Arc<App>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("qd-output".into())
        .spawn(move || run(app))
        .expect("spawn output thread")
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

    while !app.shutdown.load(Ordering::Acquire) {
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
/// Refuses to fire if focus has moved since that paste (see
/// [`LAST_PASTE_TARGET`]): blind backspaces into a window QuickDictate did not
/// write to would delete the user's own content.
///
/// If there is no previous chunk to undo, this is a no-op (logged at debug)
/// -- we never invent backspaces without a known prior paste. Only ever
/// undoes the single most recent chunk; repeated commands require repeated
/// "scratch that"s (each becomes its own transcript / history entry).
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

    // Only undo if focus is still where the text landed. Backspaces are blind:
    // if the user alt-tabbed, clicked into another field, or typed more since
    // the paste, they would delete content QuickDictate never wrote.
    let now_hwnd = focus::foreground_window_id();
    let now_exe = focus::foreground_exe_name();
    let target = LAST_PASTE_TARGET.lock().clone();
    match target {
        Some((hwnd, ref exe)) if Some(hwnd) == now_hwnd && *exe == now_exe => {}
        Some(_) => {
            tracing::warn!(
                "voice command: \"scratch that\" ignored -- focus moved since the last paste \
                 (now {:?}); refusing to send backspaces into a different window",
                now_exe.as_deref().unwrap_or("<unknown>")
            );
            return;
        }
        None => {
            tracing::debug!("voice command: \"scratch that\" heard, but no paste target recorded");
            return;
        }
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
    // same (already-removed) text as still "most recent" and re-undo it.
    app.history.lock().pop_most_recent();

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
    // window may already have changed) but only PUBLISHED if the text
    // actually got typed, so an undo can never chase a paste that failed.
    let target = focus::foreground_window_id().map(|h| (h, exe_at_paste_time()));
    *LAST_PASTE_TARGET.lock() = None;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        paste(processed, restore_delay_ms)
    }));
    // The transcript goes into history on EVERY outcome, not just success.
    // A failed paste used to be lost three ways at once (not typed, not on the
    // clipboard, not replayable); keeping it here means the tray's "Recent
    // transcriptions" can always recover the words the user actually said.
    if save_as_last {
        app.history.lock().push(processed.to_string());
    }
    match result {
        Ok(Ok(PasteOutcome::Typed)) => {
            tracing::info!("paste OK");
            *LAST_PASTE_TARGET.lock() = target;
        }
        Ok(Ok(PasteOutcome::LeftOnClipboard)) => {
            tracing::error!(
                "the focused window runs elevated, so Windows discards injected keystrokes; \
                 the transcription is on the clipboard, press Ctrl+V to paste it"
            );
            app.raise_error(ErrorKind::Elevated);
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

/// The foreground exe at paste time, resolved once so the value stored in
/// [`LAST_PASTE_TARGET`] and the one used for profile matching agree.
fn exe_at_paste_time() -> Option<String> {
    focus::foreground_exe_name()
}
