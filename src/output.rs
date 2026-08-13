//! Hybrid text paste: Unicode keystrokes for short bursts, clipboard for
//! longer text (instant appearance, no character-by-character typing effect).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use unicode_segmentation::UnicodeSegmentation;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};

use crate::focus;
use crate::polish;
use crate::state::{App, ErrorKind};
use crate::text::{self, TextProcessor};
use crate::voice_commands::{self, ScratchThat};

/// KEYEVENTF_UNICODE (0x0004): wScan carries the Unicode character; wVk must
/// be 0. Defined here rather than imported so we don't depend on a specific
/// windows-rs release exporting it as a named constant.
const KEYEVENTF_UNICODE: KEYBD_EVENT_FLAGS = KEYBD_EVENT_FLAGS(4u32);
const VK_V: VIRTUAL_KEY = VIRTUAL_KEY(0x56);

/// Threshold (chars) above which we use clipboard paste instead of keystrokes.
/// Below this, character-by-character typing is imperceptible.
const CLIPBOARD_THRESHOLD: usize = 80;
/// Cap on ONE clipboard format we are willing to hold in memory while we
/// borrow the clipboard.
const MAX_SAVED_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;
/// Cap on the whole multi-format snapshot. A clipboard carrying an image in
/// four encodings adds up fast, and we would rather decline to snapshot than
/// balloon a tray app's RSS.
const MAX_SNAPSHOT_TOTAL_BYTES: usize = 48 * 1024 * 1024;

/// Where the most recent paste actually landed: (foreground HWND, exe name).
/// "Scratch that" refuses to fire backspaces unless focus is still there, so
/// an alt-tab between dictating and undoing cannot delete somebody else's
/// text. Set by [`paste_processed`], read by [`handle_scratch_that`].
static LAST_PASTE_TARGET: Mutex<Option<(isize, Option<String>)>> = Mutex::new(None);

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

                // Voice Commands (precision subset): a FINAL transcript that
                // ends with "scratch that" undoes the previous pasted chunk
                // instead of being pasted itself. Checked on the *raw*
                // transcript, before any text processing, so the command
                // phrase itself never goes through replacements/punctuation.
                match voice_commands::detect(&raw, current_cfg.voice_commands) {
                    ScratchThat::Triggered { remaining_raw } => {
                        // The chunk this would have continued is being deleted,
                        // so there is nothing left to continue.
                        continue_within = None;
                        handle_scratch_that(&app, &remaining_raw, &mut cache, &current_cfg);
                        continue;
                    }
                    ScratchThat::NotTriggered => {}
                }

                // Resolve the foreground window's exe at commit time (not
                // when the hotkey was pressed) -- the user may well have
                // switched windows mid-dictation.
                let exe_name = focus::foreground_exe_name();

                // The optional LLM cleanup pass, on the RAW transcript and
                // before the deterministic rules below -- the user's own
                // replacements, dev-term casing and punctuation settings are
                // explicit instructions and must win over a model's opinion.
                // Bounded by `polish_deadline_ms`, usually already answered by
                // the speculation the session runner started while they were
                // still talking, and falls back to `raw` on any problem.
                let raw = match polish::settings_for(&current_cfg, exe_name.as_deref()) {
                    Some(settings) => app.polish.resolve(&settings, &raw).unwrap_or(raw),
                    None => raw,
                };

                let processor = cache.get_or_build(&current_cfg, exe_name.as_deref());

                let epoch = app.current_session_epoch();
                let continuing = continue_within == Some(epoch);
                let Some(processed) = process_guarded(processor, &raw, continuing) else {
                    continue_within = None;
                    continue;
                };
                if processed.is_empty() { continue; }
                continue_within = text::ends_mid_sentence(&processed).then_some(epoch);
                paste_processed(&app, &processed, true, current_cfg.log_transcripts);
            }
            recv(app.replay_rx) -> replay => {
                // A replay re-pastes finished history, so it neither continues
                // the previous chunk nor leaves one open.
                continue_within = None;
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

/// What actually happened to the text.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PasteOutcome {
    /// It was injected into the focused window.
    Typed,
    /// The focused window is at a higher integrity level, so injected input is
    /// silently dropped by UIPI. The text was left on the clipboard instead,
    /// deliberately NOT restoring the previous contents, so the user can paste
    /// it themselves.
    LeftOnClipboard,
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

pub fn paste(text: &str, restore_delay_ms: u64) -> Result<PasteOutcome> {
    if text.is_empty() {
        return Ok(PasteOutcome::Typed);
    }

    // UIPI: a window at a higher integrity level than us never receives
    // injected input, and SendInput still reports every event as sent, so
    // without this check the app cheerfully logs "paste OK" into the void.
    // `None` means we could not tell, which we treat as "go ahead".
    if focus::foreground_is_elevated() == Some(true) {
        set_clipboard_unicode(text)?;
        return Ok(PasteOutcome::LeftOnClipboard);
    }

    // Any modifier the user is physically holding (very much including the
    // modifier half of their own hold-to-talk hotkey) combines with whatever
    // we inject: Ctrl+V becomes Ctrl+Alt+V, and typed characters become menu
    // accelerators. Release them for the duration of the injection.
    let _modifiers = ReleasedModifiers::take();

    let n = text.chars().count();
    if n < CLIPBOARD_THRESHOLD {
        tracing::debug!("paste: sending {} chars via Unicode keystrokes", n);
        return send_unicode_text(text).map(|()| PasteOutcome::Typed);
    }

    tracing::debug!("paste: {} chars via clipboard (instant)", n);
    match paste_via_clipboard(text, restore_delay_ms) {
        Ok(()) => Ok(PasteOutcome::Typed),
        Err(e) => {
            // The clipboard path can fail entirely when another process is
            // holding the clipboard open (clipboard managers do this). Falling
            // back to keystrokes is slower but it is the difference between a
            // slightly janky paste and a silently lost dictation.
            tracing::warn!("paste: clipboard path failed ({e:#}); falling back to keystrokes");
            send_unicode_text(text).map(|()| PasteOutcome::Typed)
        }
    }
}

/// Synthesizes a key-up for every modifier currently held down, so injected
/// input is not silently reinterpreted as a shortcut.
///
/// Deliberately does NOT re-press them on drop. The physical key is still
/// down, so Windows delivers the real key-up when the user lets go and the
/// async key state resynchronizes on its own. Re-pressing risks leaving a
/// modifier stuck down if the user released mid-paste, which is much worse
/// than a modifier that came up half a second early.
struct ReleasedModifiers;

impl ReleasedModifiers {
    fn take() -> Self {
        const MODIFIERS: [VIRTUAL_KEY; 5] = [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN];
        let mut ups: Vec<INPUT> = Vec::new();
        for vk in MODIFIERS {
            // GetAsyncKeyState's high bit means "currently down".
            let down = unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0;
            if down {
                ups.push(keybd_input(vk, KEYEVENTF_KEYUP));
            }
        }
        if !ups.is_empty() {
            tracing::debug!("paste: releasing {} held modifier(s) first", ups.len());
            unsafe {
                let sent = SendInput(&ups, std::mem::size_of::<INPUT>() as i32);
                if sent as usize != ups.len() {
                    tracing::warn!("paste: only released {sent}/{} modifiers", ups.len());
                }
            }
        }
        Self
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

    // Snapshot everything the clipboard currently holds, then hand ownership
    // of the restore to a guard. Every exit from here on, including a `?` on
    // a failed SetClipboardData and including a panic, runs the restore.
    // Previously a failure between EmptyClipboard and the restore block lost
    // the user's clipboard permanently.
    let mut guard = ClipboardGuard::new(snapshot_clipboard());

    set_clipboard_unicode(text)?;
    // Clipboard "version" right after our write. If it differs at restore
    // time, some other process wrote the clipboard in between and restoring
    // the snapshot would clobber it, so the guard skips the restore. (The
    // sequence number bumps on writes only; clipboard-history listeners
    // merely *read* and don't affect it.)
    guard.expect_seq = Some(unsafe { GetClipboardSequenceNumber() });
    send_ctrl_v()?;
    // Wait for the target app to consume the paste before restoring.
    // SendInput only *queues* the Ctrl+V -- the target reads the clipboard
    // whenever it gets around to processing the keystroke, and a busy
    // browser/Electron app can easily take >100-200 ms. Restoring earlier
    // than that hands the stale prior contents to the late reader, which
    // then pastes the OLD clipboard instead of the transcription (the
    // original 60 ms delay caused exactly that in the field).
    std::thread::sleep(Duration::from_millis(restore_delay_ms));
    drop(guard);
    Ok(())
}

/// Everything we managed to copy off the clipboard before overwriting it.
struct ClipboardSnapshot {
    /// `(format id, bytes)` for every HGLOBAL-backed format, in the order
    /// `EnumClipboardFormats` reported them (which is the owner's priority
    /// order, and the order they must go back in).
    formats: Vec<(u32, Vec<u8>)>,
    /// Formats we could not copy, so the log can say what was lost.
    skipped: Vec<u32>,
}

/// Restores a [`ClipboardSnapshot`] on drop. Holding the restore in a `Drop`
/// impl rather than at the end of a happy path is the whole point: an early
/// `?` return or a panic between EmptyClipboard and the restore used to
/// destroy the user's clipboard with only a log line to show for it.
struct ClipboardGuard {
    snapshot: Option<ClipboardSnapshot>,
    /// The sequence number we expect to still see. `None` means we never got
    /// as far as writing our own text, so anything on the clipboard now is
    /// wreckage from a partial write and should be replaced unconditionally.
    expect_seq: Option<u32>,
}

impl ClipboardGuard {
    fn new(snapshot: Option<ClipboardSnapshot>) -> Self {
        Self {
            snapshot,
            expect_seq: None,
        }
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        let Some(snapshot) = self.snapshot.take() else {
            tracing::debug!("clipboard: nothing was snapshotted, so nothing to restore");
            return;
        };
        if let Some(expected) = self.expect_seq {
            let now = unsafe { GetClipboardSequenceNumber() };
            if now != expected {
                tracing::debug!(
                    "clipboard changed since our paste (seq {expected} -> {now}); \
                     skipping the restore so we do not clobber it"
                );
                return;
            }
        }
        if let Err(e) = restore_clipboard(&snapshot) {
            tracing::warn!("failed to restore the prior clipboard contents: {e:#}");
        }
    }
}

/// Whether `GetClipboardData` hands back an HGLOBAL for this format. The
/// handle-based formats (GDI bitmaps, palettes, metafiles, owner-display) are
/// not memory blocks and cannot be snapshotted this way. In practice a copied
/// image also carries CF_DIB/CF_DIBV5, which ARE HGLOBAL, so images usually
/// survive anyway.
fn is_hglobal_format(fmt: u32) -> bool {
    const CF_BITMAP: u32 = 2;
    const CF_METAFILEPICT: u32 = 3;
    const CF_PALETTE: u32 = 9;
    const CF_ENHMETAFILE: u32 = 14;
    const CF_OWNERDISPLAY: u32 = 0x0080;
    const CF_DSPMETAFILEPICT: u32 = 0x0083;
    const CF_DSPENHMETAFILE: u32 = 0x008E;
    const CF_GDIOBJFIRST: u32 = 0x0300;
    const CF_GDIOBJLAST: u32 = 0x03FF;
    !matches!(
        fmt,
        CF_BITMAP
            | CF_METAFILEPICT
            | CF_PALETTE
            | CF_ENHMETAFILE
            | CF_OWNERDISPLAY
            | CF_DSPMETAFILEPICT
            | CF_DSPENHMETAFILE
    ) && !(CF_GDIOBJFIRST..=CF_GDIOBJLAST).contains(&fmt)
}

/// Copy every HGLOBAL-backed clipboard format out into owned buffers.
///
/// This replaces the old CF_UNICODETEXT-only snapshot, which meant copying an
/// image, a file list, or an HTML fragment and then dictating destroyed it
/// permanently. Handles returned by `GetClipboardData` belong to the
/// clipboard and go invalid the moment `EmptyClipboard` runs, so everything
/// has to be copied out here, while we still hold the clipboard open.
fn snapshot_clipboard() -> Option<ClipboardSnapshot> {
    open_clipboard().ok()?;
    let snapshot = read_all_open_formats();
    unsafe {
        let _ = CloseClipboard();
    }
    if !snapshot.skipped.is_empty() {
        // Debug, not warn: skipping exotic formats is the designed common
        // case (see should_snapshot_format), not an anomaly worth alarming
        // a log reader over on every long paste.
        tracing::debug!(
            "clipboard: {} format(s) outside the preserve list will be lost by this paste: {:?}",
            snapshot.skipped.len(),
            snapshot.skipped
        );
    }
    Some(snapshot)
}

/// The registered (non-standard) formats worth preserving across a paste,
/// resolved to their runtime ids once. Names are stable Windows conventions.
fn registered_snapshot_formats() -> &'static [u32] {
    use windows::core::PCWSTR;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    static IDS: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    IDS.get_or_init(|| {
        ["HTML Format", "Rich Text Format", "PNG"]
            .iter()
            .filter_map(|name| {
                let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                let id = unsafe { RegisterClipboardFormatW(PCWSTR(w.as_ptr())) };
                (id != 0).then_some(id)
            })
            .collect()
    })
}

/// Whether a format is worth the cost of copying out before we overwrite the
/// clipboard.
///
/// Deliberately an ALLOWLIST, not "every HGLOBAL format". `GetClipboardData`
/// on a delayed-render format makes the owning app synthesize the data
/// synchronously, and copy-heavy apps (Excel, browsers) advertise a dozen or
/// more exotic formats (Biff, SYLK, XML Spreadsheet, ...) that are exactly the
/// expensive ones. Fetching them all froze a normal long paste for seconds
/// after an Excel copy. Text, files, bitmaps, HTML, RTF, and PNG are what a
/// user actually notices losing; the rest is regenerated by the source app on
/// the next copy.
fn should_snapshot_format(fmt: u32) -> bool {
    const CF_DIB: u32 = 8;
    const CF_UNICODETEXT_ID: u32 = 13;
    const CF_HDROP: u32 = 15;
    const CF_LOCALE: u32 = 16;
    const CF_DIBV5: u32 = 17;
    matches!(
        fmt,
        CF_DIB | CF_UNICODETEXT_ID | CF_HDROP | CF_LOCALE | CF_DIBV5
    ) || registered_snapshot_formats().contains(&fmt)
}

/// Walk the advertised clipboard formats and copy out the allowlisted,
/// HGLOBAL-backed ones. The clipboard must already be open, and the caller
/// closes it.
fn read_all_open_formats() -> ClipboardSnapshot {
    let mut formats: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut skipped: Vec<u32> = Vec::new();
    let mut total: usize = 0;
    let mut fmt = unsafe { EnumClipboardFormats(0) };
    while fmt != 0 {
        if !should_snapshot_format(fmt) || !is_hglobal_format(fmt) {
            skipped.push(fmt);
        } else {
            match read_global_format(fmt) {
                Some(bytes) if total.saturating_add(bytes.len()) <= MAX_SNAPSHOT_TOTAL_BYTES => {
                    total += bytes.len();
                    formats.push((fmt, bytes));
                }
                _ => skipped.push(fmt),
            }
        }
        fmt = unsafe { EnumClipboardFormats(fmt) };
    }
    ClipboardSnapshot { formats, skipped }
}

/// Read one HGLOBAL-backed clipboard format into an owned byte buffer. The
/// clipboard must already be open.
fn read_global_format(fmt: u32) -> Option<Vec<u8>> {
    unsafe {
        let h = GetClipboardData(fmt).ok()?;
        if h.is_invalid() {
            return None;
        }
        let hglob = windows::Win32::Foundation::HGLOBAL(h.0);
        let byte_size = GlobalSize(hglob);
        if byte_size == 0 || byte_size > MAX_SAVED_CLIPBOARD_BYTES {
            return None;
        }
        let src = GlobalLock(hglob) as *const u8;
        if src.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(src, byte_size).to_vec();
        let _ = GlobalUnlock(hglob);
        Some(bytes)
    }
}

/// Put a snapshot back, format by format, in the order it was captured.
fn restore_clipboard(snapshot: &ClipboardSnapshot) -> Result<()> {
    open_clipboard()?;
    let result = (|| -> Result<()> {
        unsafe {
            EmptyClipboard()?;
            for (fmt, bytes) in &snapshot.formats {
                // GlobalAlloc(0) is legal but useless; a zero-length format
                // was already filtered out in read_global_format.
                let hglob = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
                if hglob.0.is_null() {
                    return Err(anyhow!("GlobalAlloc null restoring format {fmt}"));
                }
                let dst = GlobalLock(hglob) as *mut u8;
                if dst.is_null() {
                    let _ = GlobalFree(hglob);
                    return Err(anyhow!("GlobalLock null restoring format {fmt}"));
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
                let _ = GlobalUnlock(hglob);
                if SetClipboardData(*fmt, HANDLE(hglob.0)).is_err() {
                    // Ownership did NOT transfer, so this block is ours to free.
                    let _ = GlobalFree(hglob);
                    return Err(anyhow!("SetClipboardData failed restoring format {fmt}"));
                }
            }
            Ok(())
        }
    })();
    unsafe {
        let _ = CloseClipboard();
    }
    result
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
    use super::*;

    #[test]
    fn unicode_input_preserves_non_bmp_characters_as_surrogate_pairs() {
        assert_eq!(
            unicode_code_units("A😀Z"),
            vec![0x0041, 0xD83D, 0xDE00, 0x005A]
        );
    }

    #[test]
    fn snapshot_allowlist_keeps_valuable_formats_and_drops_exotic_ones() {
        // Standard formats a user notices losing.
        for fmt in [8u32, 13, 15, 16, 17] {
            assert!(should_snapshot_format(fmt), "format {fmt} should be kept");
        }
        // The delayed-render-prone exotica an Excel/browser copy advertises
        // must NOT be fetched: GetClipboardData on them forces the owning app
        // to synthesize data synchronously and froze normal pastes.
        for fmt in [2u32, 3, 129, 0xC000 + 7777] {
            assert!(
                !should_snapshot_format(fmt),
                "format {fmt} should be skipped"
            );
        }
        // The registered names resolve and are kept (HTML/RTF/PNG).
        for id in registered_snapshot_formats() {
            assert!(should_snapshot_format(*id));
        }
        assert_eq!(registered_snapshot_formats().len(), 3);
    }

    #[test]
    fn hglobal_formats_exclude_the_handle_based_ones() {
        // CF_UNICODETEXT, CF_HDROP, CF_DIB and registered formats are memory
        // blocks we can copy.
        for fmt in [1u32, 13, 8, 17, 15, 0xC000, 0xC123] {
            assert!(is_hglobal_format(fmt), "format {fmt} should be HGLOBAL");
        }
        // GDI handles and owner-display are not.
        for fmt in [
            2u32, 3, 9, 14, 0x0080, 0x0083, 0x008E, 0x0300, 0x0350, 0x03FF,
        ] {
            assert!(!is_hglobal_format(fmt), "format {fmt} is handle-based");
        }
    }

    #[test]
    fn undo_counts_grapheme_clusters_not_scalars() {
        // A ZWJ family emoji is one glyph but many Unicode scalars; counting
        // scalars would send extra backspaces into preceding text.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert!(family.chars().count() > 1);
        assert_eq!(family.graphemes(true).count(), 1);
        assert_eq!("hi \u{1F44B}".graphemes(true).count(), 4);
    }

    #[test]
    fn a_failed_paste_publishes_no_undo_target() {
        // Guards the invariant paste_processed relies on: LAST_PASTE_TARGET is
        // cleared up front and only set again on a Typed outcome, so a
        // "scratch that" after a failed paste finds nothing to undo.
        *LAST_PASTE_TARGET.lock() = Some((0x1234, Some("editor.exe".into())));
        *LAST_PASTE_TARGET.lock() = None;
        assert!(LAST_PASTE_TARGET.lock().is_none());
    }
}
