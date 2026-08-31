//! Synthesised keyboard input: the Unicode keystroke path for short bursts,
//! the modifiers that must be released first, and the Ctrl+V and backspaces
//! the clipboard and scratch-that paths send.

use anyhow::{anyhow, Result};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};

use super::*;

/// input is not silently reinterpreted as a shortcut.
///
/// Deliberately does NOT re-press them on drop. The physical key is still
/// down, so Windows delivers the real key-up when the user lets go and the
/// async key state resynchronizes on its own. Re-pressing risks leaving a
/// modifier stuck down if the user released mid-paste, which is much worse
/// than a modifier that came up half a second early.
pub(super) struct ReleasedModifiers;

impl ReleasedModifiers {
    pub(super) fn take() -> Self {
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

pub(super) fn send_unicode_text(text: &str) -> Result<()> {
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

pub(super) fn unicode_code_units(text: &str) -> Vec<u16> {
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

/// Sends `count` VK_BACK (backspace) key presses via `SendInput`, in chunks
/// so we never exceed a single `SendInput` call's practical event count.
/// Used to undo the previous pasted chunk for the "scratch that" voice
/// command -- works identically whether that chunk landed via the Unicode-
/// keystroke path or the clipboard path, since both end up as ordinary
/// characters in the target app that backspace deletes one at a time.
pub(super) fn send_backspaces(count: usize) -> Result<()> {
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

pub(super) fn send_ctrl_v() -> Result<()> {
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
