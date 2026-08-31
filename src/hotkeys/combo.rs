//! Parsing a combo string like `ctrl+shift+d` into modifier and virtual-key
//! codes, and the name tables behind it.

use anyhow::{anyhow, bail, Result};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};

use crate::mouse_hook::{VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2};

/// Parse a hotkey combo like "f14" or "ctrl+shift+d" into (modifiers, vk).
/// `pub(crate)` so the settings window can validate user input before saving.
pub fn parse_combo(combo: &str) -> Result<(u32, u32)> {
    let mut modifiers: u32 = 0;
    let mut vk: u32 = 0;
    for part_raw in combo.split('+') {
        let part = part_raw.trim().to_ascii_lowercase();
        if part.is_empty() {
            continue;
        }
        let m = match part.as_str() {
            "ctrl" | "control" => Some(MOD_CONTROL.0),
            "alt" | "menu" => Some(MOD_ALT.0),
            "shift" => Some(MOD_SHIFT.0),
            "win" | "windows" | "super" => Some(MOD_WIN.0),
            _ => None,
        };
        if let Some(bits) = m {
            modifiers |= bits;
            continue;
        }
        let candidate =
            vk_for(&part).ok_or_else(|| anyhow!("unknown key '{part}' in '{combo}'"))?;
        if let Some(label) = forbidden_mouse_label(candidate) {
            bail!(
                "{label} can't be used as a hotkey \u{2014} binding a mouse button also \
                 suppresses it everywhere else, and losing {label} would take the \
                 machine away from you, including the clicks needed to come back \
                 here and undo it. Use the middle button or a thumb button \
                 (mouse3 / mouse4 / mouse5) instead."
            );
        }
        if vk != 0 {
            bail!("multiple non-modifier keys in '{combo}'");
        }
        vk = candidate;
    }
    if vk == 0 {
        bail!("no main key in '{combo}'");
    }
    Ok((modifiers | MOD_NOREPEAT.0, vk))
}

/// Mouse buttons we deliberately refuse to bind, mapped to a human label for
/// the error message. Binding a mouse button *suppresses* it system-wide (see
/// [`crate::mouse_hook`]), and a person who loses left- or right-click can no
/// longer click their way back into Settings to undo it. The middle and thumb
/// buttons are safe to claim; these two never are.
///
/// Keyed on the **virtual-key code**, not the spelling, so every alias route
/// into `vk_for` lands on the same policy — a new alias can never accidentally
/// open a back door to binding left-click.
fn forbidden_mouse_label(vk: u32) -> Option<&'static str> {
    Some(match vk {
        VK_LBUTTON => "Left click",
        VK_RBUTTON => "Right click",
        _ => return None,
    })
}

pub(super) fn vk_for(name: &str) -> Option<u32> {
    vk_for_mouse_button(name)
        .or_else(|| vk_for_alnum(name))
        .or_else(|| vk_for_function_key(name))
        .or_else(|| vk_for_editing_key(name))
        .or_else(|| vk_for_navigation_key(name))
        .or_else(|| vk_for_numpad_key(name))
}

// Mouse buttons. These are real virtual-key codes, but `RegisterHotKey`
// rejects them, so `run_hotkey_loop` routes anything matching
// `is_mouse_vk` to the low-level hook instead. Canonical spellings are
// mouse3/mouse4/mouse5; the aliases cover what mouse vendors and Windows
// itself call them. Left/right resolve here too — honestly, rather than by
// omission — and `parse_combo` is what refuses them, so the error explains
// itself instead of reading as "unknown key".
fn vk_for_mouse_button(name: &str) -> Option<u32> {
    match name {
        "mouse1" | "leftclick" | "leftmouse" | "lmb" | "lbutton" => Some(VK_LBUTTON),
        "mouse2" | "rightclick" | "rightmouse" | "rmb" | "rbutton" => Some(VK_RBUTTON),
        "mouse3" | "middleclick" | "middlemouse" | "mousemiddle" | "mmb" | "mbutton" => {
            Some(VK_MBUTTON)
        }
        "mouse4" | "mouseback" | "backmouse" | "xbutton1" | "x1" => Some(VK_XBUTTON1),
        "mouse5" | "mouseforward" | "forwardmouse" | "xbutton2" | "x2" => Some(VK_XBUTTON2),
        _ => None,
    }
}

// Letters a-z, digits 0-9.
fn vk_for_alnum(name: &str) -> Option<u32> {
    if name.len() != 1 {
        return None;
    }
    let c = name.as_bytes()[0];
    if c.is_ascii_lowercase() {
        return Some((0x41 + (c - b'a')) as u32);
    }
    if c.is_ascii_digit() {
        return Some((0x30 + (c - b'0')) as u32);
    }
    None
}

fn vk_for_function_key(name: &str) -> Option<u32> {
    Some(match name {
        "f1" => 0x70,
        "f2" => 0x71,
        "f3" => 0x72,
        "f4" => 0x73,
        "f5" => 0x74,
        "f6" => 0x75,
        "f7" => 0x76,
        "f8" => 0x77,
        "f9" => 0x78,
        "f10" => 0x79,
        "f11" => 0x7A,
        "f12" => 0x7B,
        "f13" => 0x7C,
        "f14" => 0x7D,
        "f15" => 0x7E,
        "f16" => 0x7F,
        "f17" => 0x80,
        "f18" => 0x81,
        "f19" => 0x82,
        "f20" => 0x83,
        "f21" => 0x84,
        "f22" => 0x85,
        "f23" => 0x86,
        "f24" => 0x87,
        _ => return None,
    })
}

fn vk_for_editing_key(name: &str) -> Option<u32> {
    Some(match name {
        "space" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "escape" | "esc" => 0x1B,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        _ => return None,
    })
}

fn vk_for_navigation_key(name: &str) -> Option<u32> {
    Some(match name {
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "page_up" => 0x21,
        "pagedown" | "page_down" => 0x22,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        _ => return None,
    })
}

fn vk_for_numpad_key(name: &str) -> Option<u32> {
    Some(match name {
        "numpad0" => 0x60,
        "numpad1" => 0x61,
        "numpad2" => 0x62,
        "numpad3" => 0x63,
        "numpad4" => 0x64,
        "numpad5" => 0x65,
        "numpad6" => 0x66,
        "numpad7" => 0x67,
        "numpad8" => 0x68,
        "numpad9" => 0x69,
        _ => return None,
    })
}
