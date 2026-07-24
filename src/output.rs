//! Hybrid text paste: Unicode keystrokes for short bursts, clipboard for
//! longer text (instant appearance, no character-by-character typing effect).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_BACK, VK_CONTROL,
};

use crate::focus;
use crate::state::App;
use crate::text::TextProcessor;
use crate::voice_commands::{self, ScratchThat};

/// KEYEVENTF_UNICODE (0x0004): wScan carries the Unicode character; wVk must
/// be 0. Defined here rather than imported so we don't depend on a specific
/// windows-rs release exporting it as a named constant.
const KEYEVENTF_UNICODE: KEYBD_EVENT_FLAGS = KEYBD_EVENT_FLAGS(4u32);
const VK_V: VIRTUAL_KEY = VIRTUAL_KEY(0x56);

/// Threshold (chars) above which we use clipboard paste instead of keystrokes.
/// Below this, character-by-character typing is imperceptible.
const CLIPBOARD_THRESHOLD: usize = 80;
const MAX_SAVED_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

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

                // Voice Commands (precision subset): a FINAL transcript that
                // ends with "scratch that" undoes the previous pasted chunk
                // instead of being pasted itself. Checked on the *raw*
                // transcript, before any text processing, so the command
                // phrase itself never goes through replacements/punctuation.
                match voice_commands::detect(&raw, current_cfg.voice_commands) {
                    ScratchThat::Triggered { remaining_raw } => {
                        handle_scratch_that(&app, &remaining_raw, &mut cache, &current_cfg);
                        continue;
                    }
                    ScratchThat::NotTriggered => {}
                }

                // Resolve the foreground window's exe at commit time (not
                // when the hotkey was pressed) -- the user may well have
                // switched windows mid-dictation.
                let exe_name = focus::foreground_exe_name();
                let processor = cache.get_or_build(&current_cfg, exe_name.as_deref());

                let Some(processed) = process_guarded(processor, &raw) else { continue; };
                if processed.is_empty() { continue; }
                paste_processed(&app, &processed, true, current_cfg.log_transcripts);
            }
            recv(app.replay_rx) -> replay => {
                let index = match replay {
                    Ok(index) => index,
                    Err(_) => break,
                };
                // `None` = most recent (index 0) -- the original "replay
                // last paste" behavior; `Some(i)` = a specific history entry,
                // e.g. from the tray's "Recent transcriptions" submenu.
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
                        paste_processed(&app, &entry.text, false, log_transcripts);
                    }
                    _ => tracing::warn!("replay requested, but no saved transcription is available"),
                }
            }
            default(Duration::from_millis(50)) => {}
        }
    }
}

/// Handles a recognized "scratch that" command: undoes the previously
/// pasted chunk (backspace count = its char length -- the history entry
/// already holds the fully-processed text, i.e. exactly what was sent to the
/// target window, auto_space/auto_newline trailer included) and, if any text
/// preceded the command phrase, processes and pastes that as the new chunk.
///
/// If there is no previous chunk to undo, this is a no-op (logged at debug)
/// -- we never invent backspaces without a known prior paste. Only ever
/// undoes the single most recent chunk; repeated commands require repeated
/// "scratch that"s (each becomes its own transcript / history entry).
fn handle_scratch_that(
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

    let undo_count = last.text.chars().count();
    tracing::info!("voice command: \"scratch that\" -- undoing last paste ({undo_count} char(s))");
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
    let Some(processed) = process_guarded(processor, remaining_raw) else {
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
fn process_guarded(processor: &TextProcessor, raw: &str) -> Option<String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| processor.process(raw))) {
        Ok(p) => Some(p),
        Err(_) => {
            tracing::error!("text processing PANICKED (caught; thread continues)");
            None
        }
    }
}

fn paste_processed(app: &App, processed: &str, save_as_last: bool, log_transcripts: bool) {
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
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        paste(processed, restore_delay_ms)
    }));
    match result {
        Ok(Ok(())) => {
            tracing::info!("paste OK");
            if save_as_last {
                app.history.lock().push(processed.to_string());
            }
        }
        Ok(Err(e)) => tracing::error!("paste failed: {e:#}"),
        Err(_) => tracing::error!("paste PANICKED (caught; thread continues)"),
    }
}

/// Per-config-snapshot cache of built [`TextProcessor`]s, keyed by which
/// profile (if any) matched -- `None` is the key for "no profile matched /
/// global settings". Avoids rebuilding the replacement regexes on every
/// single paste even once Per-App Profiles are in use.
struct ProcessorCache {
    entries: Vec<(Option<String>, TextProcessor)>,
}

impl ProcessorCache {
    fn new(cfg: &crate::config::Config) -> Self {
        // Pre-seed the global (no-match) entry -- the overwhelmingly common
        // case when no profile matches the foreground window.
        Self {
            entries: vec![(None, build_processor(cfg, None))],
        }
    }

    fn get_or_build(
        &mut self,
        cfg: &crate::config::Config,
        exe_name: Option<&str>,
    ) -> &TextProcessor {
        let key = cfg.active_profile(exe_name).map(|p| p.name.clone());
        if let Some(idx) = self.entries.iter().position(|(k, _)| *k == key) {
            return &self.entries[idx].1;
        }
        tracing::debug!("output: building TextProcessor for profile {:?}", key);
        self.entries
            .push((key.clone(), build_processor(cfg, exe_name)));
        &self.entries.last().unwrap().1
    }
}

fn build_processor(cfg: &crate::config::Config, exe_name: Option<&str>) -> TextProcessor {
    let effective = cfg.effective_settings(exe_name);
    TextProcessor::new(
        &effective.text_replacements,
        effective.auto_punct,
        effective.auto_space,
        effective.auto_newline,
    )
}

fn preview(s: &str) -> String {
    let trimmed: String = s.chars().take(60).collect();
    if s.chars().count() > 60 {
        format!("{trimmed}...")
    } else {
        trimmed
    }
}

/// Put `text` on the Windows clipboard (CF_UNICODETEXT) and leave it there.
/// Used by the tray's "Recent transcriptions": clicking an entry copies it so
/// the user can paste it wherever they want, instead of auto-pasting into the
/// focused window. Unlike [`paste_via_clipboard`], this does NOT restore any
/// prior clipboard contents — the whole point is to overwrite the clipboard.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    set_clipboard_unicode(text)
}

pub fn paste(text: &str, restore_delay_ms: u64) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let n = text.chars().count();
    if n < CLIPBOARD_THRESHOLD {
        tracing::debug!("paste: sending {} chars via Unicode keystrokes", n);
        send_unicode_text(text)
    } else {
        tracing::debug!("paste: {} chars via clipboard (instant)", n);
        paste_via_clipboard(text, restore_delay_ms)
    }
}

// ---------------------------------------------------------------------------
// Path A: Unicode keystrokes (short text — no clipboard, instant enough)
// ---------------------------------------------------------------------------

fn send_unicode_text(text: &str) -> Result<()> {
    let units = unicode_code_units(text);
    let mut inputs: Vec<INPUT> = Vec::with_capacity(units.len() * 2);
    for unit in units {
        inputs.push(unicode_key_input(unit, false));
        inputs.push(unicode_key_input(unit, true));
    }
    for chunk in inputs.chunks(4096) {
        unsafe {
            let sent = SendInput(chunk, std::mem::size_of::<INPUT>() as i32);
            if sent as usize != chunk.len() {
                return Err(anyhow!("SendInput sent {sent}/{} events", chunk.len()));
            }
        }
    }
    Ok(())
}

fn unicode_code_units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn unicode_key_input(unit: u16, keyup: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if keyup {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Path B: Clipboard paste (longer text — appears all at once)
// ---------------------------------------------------------------------------

fn paste_via_clipboard(text: &str, restore_delay_ms: u64) -> Result<()> {
    // `clipboard_restore_delay_ms = 0` means "don't restore": set our text,
    // paste, and leave the transcription on the clipboard.
    if restore_delay_ms == 0 {
        set_clipboard_unicode(text)?;
        return send_ctrl_v();
    }

    // Save whatever the user already had on the clipboard so we can restore
    // it once the paste has landed. We only know how to snapshot plain
    // CF_UNICODETEXT -- see the comment on `saved_clipboard_text` for what
    // happens when the prior clipboard held something else.
    let prior = saved_clipboard_text();

    set_clipboard_unicode(text)?;
    // Clipboard "version" right after our write. If it differs at restore
    // time, some other process wrote the clipboard in between and restoring
    // `prior` would clobber it -- skip the restore in that case. (The
    // sequence number bumps on writes only; clipboard-history listeners
    // merely *read* and don't affect it.)
    let seq_ours = unsafe { GetClipboardSequenceNumber() };
    send_ctrl_v()?;
    // Wait for the target app to consume the paste before restoring.
    // SendInput only *queues* the Ctrl+V -- the target reads the clipboard
    // whenever it gets around to processing the keystroke, and a busy
    // browser/Electron app can easily take >100-200 ms. Restoring earlier
    // than that hands the stale prior contents to the late reader, which
    // then pastes the OLD clipboard instead of the transcription (the
    // original 60 ms delay caused exactly that in the field).
    std::thread::sleep(Duration::from_millis(restore_delay_ms));

    // Restore the prior clipboard contents now that the paste has settled.
    // Best-effort: a failure here shouldn't turn an otherwise-successful
    // paste into an error, so we only log it.
    match prior {
        Some(prior_text) => {
            let seq_now = unsafe { GetClipboardSequenceNumber() };
            if seq_now != seq_ours {
                tracing::debug!(
                    "clipboard changed since our paste (seq {seq_ours} -> {seq_now}); \
                     skipping prior-contents restore"
                );
            } else if let Err(e) = set_clipboard_unicode(&prior_text) {
                tracing::warn!("failed to restore prior clipboard text: {e:#}");
            }
        }
        None => {
            tracing::debug!("prior clipboard had no CF_UNICODETEXT to restore (or read failed)");
        }
    }
    Ok(())
}

fn open_clipboard() -> Result<()> {
    for _ in 0..10 {
        unsafe {
            if OpenClipboard(HWND::default()).is_ok() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err(anyhow!("OpenClipboard failed after retries"))
}

/// Read back the clipboard's current CF_UNICODETEXT contents, if any, as an
/// owned `String` (copied out before we touch the clipboard, since the
/// handle `GetClipboardData` returns belongs to the clipboard, not us, and
/// becomes invalid the moment `EmptyClipboard` runs).
///
/// Limitation: this only preserves plain text. If the prior clipboard held
/// something else (an image, HTML fragment, file-drop list, etc.) that data
/// is NOT saved -- there's no cheap way to snapshot "every format on the
/// clipboard" via the classic clipboard API, so that case is left clobbered
/// rather than handled here.
fn saved_clipboard_text() -> Option<String> {
    open_clipboard().ok()?;
    let result = (|| -> Option<String> {
        unsafe {
            IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).ok()?;
            let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            if h.is_invalid() {
                return None;
            }
            let hglob = windows::Win32::Foundation::HGLOBAL(h.0);
            let byte_size = GlobalSize(hglob);
            if byte_size == 0 || byte_size > MAX_SAVED_CLIPBOARD_BYTES {
                return None;
            }
            let src = GlobalLock(hglob) as *const u16;
            if src.is_null() {
                return None;
            }
            // GlobalSize is in bytes; CF_UNICODETEXT is a NUL-terminated
            // UTF-16 buffer. Find the terminator ourselves rather than
            // assuming the byte count is exactly `(chars + 1) * 2` -- some
            // producers over-allocate the block.
            let max_units = byte_size / std::mem::size_of::<u16>();
            let slice = std::slice::from_raw_parts(src, max_units);
            let end = slice.iter().position(|&u| u == 0).unwrap_or(max_units);
            let text = String::from_utf16_lossy(&slice[..end]);
            let _ = GlobalUnlock(hglob);
            Some(text)
        }
    })();
    unsafe {
        let _ = CloseClipboard();
    }
    result
}

fn set_clipboard_unicode(text: &str) -> Result<()> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0);
    let byte_size = utf16.len() * std::mem::size_of::<u16>();

    open_clipboard()?;
    let result = (|| -> Result<()> {
        unsafe {
            EmptyClipboard()?;
            let hglob = GlobalAlloc(GMEM_MOVEABLE, byte_size)?;
            if hglob.0.is_null() {
                return Err(anyhow!("GlobalAlloc null"));
            }
            let dst = GlobalLock(hglob) as *mut u16;
            if dst.is_null() {
                let _ = GlobalFree(hglob);
                return Err(anyhow!("GlobalLock null"));
            }
            std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len());
            let _ = GlobalUnlock(hglob);
            let h = HANDLE(hglob.0);
            match SetClipboardData(CF_UNICODETEXT.0 as u32, h) {
                Ok(_) => Ok(()),
                Err(_) => {
                    let _ = GlobalFree(hglob);
                    Err(anyhow!("SetClipboardData failed"))
                }
            }
        }
    })();
    unsafe {
        let _ = CloseClipboard();
    }
    result
}

/// Sends `count` VK_BACK (backspace) key presses via `SendInput`, in chunks
/// so we never exceed a single `SendInput` call's practical event count.
/// Used to undo the previous pasted chunk for the "scratch that" voice
/// command -- works identically whether that chunk landed via the Unicode-
/// keystroke path or the clipboard path, since both end up as ordinary
/// characters in the target app that backspace deletes one at a time.
fn send_backspaces(count: usize) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    let mut inputs: Vec<INPUT> = Vec::with_capacity(count * 2);
    for _ in 0..count {
        inputs.push(keybd_input(VK_BACK, KEYBD_EVENT_FLAGS(0)));
        inputs.push(keybd_input(VK_BACK, KEYEVENTF_KEYUP));
    }
    for chunk in inputs.chunks(4096) {
        unsafe {
            let sent = SendInput(chunk, std::mem::size_of::<INPUT>() as i32);
            if sent as usize != chunk.len() {
                return Err(anyhow!(
                    "SendInput (backspace) sent {sent}/{} events",
                    chunk.len()
                ));
            }
        }
    }
    Ok(())
}

fn send_ctrl_v() -> Result<()> {
    let inputs: [INPUT; 4] = [
        keybd_input(VK_CONTROL, KEYBD_EVENT_FLAGS(0)),
        keybd_input(VK_V, KEYBD_EVENT_FLAGS(0)),
        keybd_input(VK_V, KEYEVENTF_KEYUP),
        keybd_input(VK_CONTROL, KEYEVENTF_KEYUP),
    ];
    unsafe {
        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent as usize != inputs.len() {
            return Err(anyhow!("SendInput sent {sent}/{}", inputs.len()));
        }
    }
    Ok(())
}

fn keybd_input(vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::unicode_code_units;

    #[test]
    fn unicode_input_preserves_non_bmp_characters_as_surrogate_pairs() {
        assert_eq!(
            unicode_code_units("A😀Z"),
            vec![0x0041, 0xD83D, 0xDE00, 0x005A]
        );
    }
}
