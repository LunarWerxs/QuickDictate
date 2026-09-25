//! Hybrid text paste: Unicode keystrokes for short bursts, clipboard for
//! longer text (instant appearance, no character-by-character typing effect).

mod clipboard;
mod input;
mod processor;
mod target;
mod worker;

#[cfg(test)]
mod tests;

use anyhow::Result;
use windows::Win32::UI::Input::KeyboardAndMouse::{KEYBD_EVENT_FLAGS, VIRTUAL_KEY};

use crate::app_compat::Delivery;
use crate::focus;

pub use processor::PasteOutcome;
pub(crate) use worker::request_stop;
pub use worker::spawn;

use clipboard::*;
use input::*;
use processor::*;
use target::*;

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

/// Put `text` on the Windows clipboard (CF_UNICODETEXT) and leave it there.
/// Used by the tray's "Recent transcriptions": clicking an entry copies it so
/// the user can paste it wherever they want, instead of auto-pasting into the
/// focused window. Unlike [`paste_via_clipboard`], this does NOT restore any
/// prior clipboard contents — the whole point is to overwrite the clipboard.
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    set_clipboard_unicode(text)
}

pub fn paste(text: &str, restore_delay_ms: u64, delivery: Delivery) -> Result<PasteOutcome> {
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

    // The app-compatibility list says this window drops every kind of
    // injected input, so typing would be lost without a trace.
    if delivery == Delivery::Manual {
        set_clipboard_unicode(text)?;
        return Ok(PasteOutcome::LeftForApp);
    }

    // Any modifier the user is physically holding (very much including the
    // modifier half of their own hold-to-talk hotkey) combines with whatever
    // we inject: Ctrl+V becomes Ctrl+Alt+V, and typed characters become menu
    // accelerators. Release them for the duration of the injection.
    let _modifiers = ReleasedModifiers::take();

    let n = text.chars().count();
    if !uses_clipboard(delivery, n) {
        tracing::debug!("paste: sending {} chars via Unicode keystrokes", n);
        return send_unicode_text(text).map(|()| PasteOutcome::Typed);
    }

    tracing::debug!("paste: {} chars via clipboard (instant)", n);
    match paste_via_clipboard(text, restore_delay_ms) {
        Ok(()) => Ok(PasteOutcome::Typed),
        Err(e) if delivery == Delivery::Clipboard => {
            // Listed as dropping synthetic keystrokes, so the keystroke
            // fallback below would lose the text silently: leave it on the
            // clipboard for the user instead.
            tracing::warn!(
                "paste: clipboard path failed ({e:#}); leaving the text on the clipboard"
            );
            set_clipboard_unicode(text)?;
            Ok(PasteOutcome::LeftForApp)
        }
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

/// Whether `n` characters go through the clipboard rather than keystrokes. A
/// forced delivery from the app-compatibility list overrides the length rule.
fn uses_clipboard(delivery: Delivery, n: usize) -> bool {
    match delivery {
        Delivery::Keystrokes => false,
        Delivery::Clipboard => true,
        Delivery::Auto | Delivery::Manual => n >= CLIPBOARD_THRESHOLD,
    }
}
