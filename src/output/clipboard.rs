//! The clipboard paste path, and the snapshot/restore around it.
//!
//! Pasting through the clipboard means borrowing something the user owns, so
//! everything here exists to give it back exactly as it was.

use std::time::Duration;

use anyhow::{anyhow, Result};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;

use super::*;

// ---------------------------------------------------------------------------
// Path B: Clipboard paste (longer text — appears all at once)
// ---------------------------------------------------------------------------

pub(super) fn paste_via_clipboard(text: &str, restore_delay_ms: u64) -> Result<()> {
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
pub(super) fn is_hglobal_format(fmt: u32) -> bool {
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
pub(super) fn registered_snapshot_formats() -> &'static [u32] {
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
pub(super) fn should_snapshot_format(fmt: u32) -> bool {
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

pub(super) fn set_clipboard_unicode(text: &str) -> Result<()> {
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
