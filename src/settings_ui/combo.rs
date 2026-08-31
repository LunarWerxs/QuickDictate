//! Turning an egui key or pointer event into a hotkey combo string, and
//! deciding when two of those strings are the same hotkey.

use eframe::egui::{self};

// Split out of this file so each surface can be reviewed on its own; the

// ---- Hotkey recording ------------------------------------------------------

/// `(key, name)` pairs backing [`egui_key_name`]. A plain table, not a
/// `match`, because every arm here is the same trivial "spell it lowercase"
/// rule — the table IS the mapping, with no per-key logic to dispatch on.
const EGUI_KEY_NAMES: &[(egui::Key, &str)] = &[
    (egui::Key::A, "a"),
    (egui::Key::B, "b"),
    (egui::Key::C, "c"),
    (egui::Key::D, "d"),
    (egui::Key::E, "e"),
    (egui::Key::F, "f"),
    (egui::Key::G, "g"),
    (egui::Key::H, "h"),
    (egui::Key::I, "i"),
    (egui::Key::J, "j"),
    (egui::Key::K, "k"),
    (egui::Key::L, "l"),
    (egui::Key::M, "m"),
    (egui::Key::N, "n"),
    (egui::Key::O, "o"),
    (egui::Key::P, "p"),
    (egui::Key::Q, "q"),
    (egui::Key::R, "r"),
    (egui::Key::S, "s"),
    (egui::Key::T, "t"),
    (egui::Key::U, "u"),
    (egui::Key::V, "v"),
    (egui::Key::W, "w"),
    (egui::Key::X, "x"),
    (egui::Key::Y, "y"),
    (egui::Key::Z, "z"),
    (egui::Key::Num0, "0"),
    (egui::Key::Num1, "1"),
    (egui::Key::Num2, "2"),
    (egui::Key::Num3, "3"),
    (egui::Key::Num4, "4"),
    (egui::Key::Num5, "5"),
    (egui::Key::Num6, "6"),
    (egui::Key::Num7, "7"),
    (egui::Key::Num8, "8"),
    (egui::Key::Num9, "9"),
    (egui::Key::F1, "f1"),
    (egui::Key::F2, "f2"),
    (egui::Key::F3, "f3"),
    (egui::Key::F4, "f4"),
    (egui::Key::F5, "f5"),
    (egui::Key::F6, "f6"),
    (egui::Key::F7, "f7"),
    (egui::Key::F8, "f8"),
    (egui::Key::F9, "f9"),
    (egui::Key::F10, "f10"),
    (egui::Key::F11, "f11"),
    (egui::Key::F12, "f12"),
    (egui::Key::F13, "f13"),
    (egui::Key::F14, "f14"),
    (egui::Key::F15, "f15"),
    (egui::Key::F16, "f16"),
    (egui::Key::F17, "f17"),
    (egui::Key::F18, "f18"),
    (egui::Key::F19, "f19"),
    (egui::Key::F20, "f20"),
    (egui::Key::F21, "f21"),
    (egui::Key::F22, "f22"),
    (egui::Key::F23, "f23"),
    (egui::Key::F24, "f24"),
    (egui::Key::Space, "space"),
    (egui::Key::Enter, "enter"),
    (egui::Key::Tab, "tab"),
    (egui::Key::Backspace, "backspace"),
    (egui::Key::Delete, "delete"),
    (egui::Key::Insert, "insert"),
    (egui::Key::Home, "home"),
    (egui::Key::End, "end"),
    (egui::Key::PageUp, "pageup"),
    (egui::Key::PageDown, "pagedown"),
    (egui::Key::ArrowUp, "up"),
    (egui::Key::ArrowDown, "down"),
    (egui::Key::ArrowLeft, "left"),
    (egui::Key::ArrowRight, "right"),
];

/// Map an egui key to QuickDictate's hotkey name (matching `hotkeys::vk_for`);
/// `None` for keys the parser doesn't support (F25+, symbols, keypad).
fn egui_key_name(key: egui::Key) -> Option<&'static str> {
    EGUI_KEY_NAMES
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, name)| *name)
}

/// Prefix a key/button name with whichever modifiers were held, in the fixed
/// order `parse_combo` reads back.
fn with_modifiers(name: &str, mods: egui::Modifiers) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if mods.ctrl || mods.command {
        parts.push("ctrl");
    }
    if mods.alt {
        parts.push("alt");
    }
    if mods.shift {
        parts.push("shift");
    }
    parts.push(name);
    parts.join("+")
}

/// Build a combo string ("ctrl+shift+f14") from a captured key + modifiers.
pub(super) fn combo_from_event(key: egui::Key, mods: egui::Modifiers) -> Option<String> {
    Some(with_modifiers(egui_key_name(key)?, mods))
}

/// Build a combo string ("mouse4", "ctrl+mouse3") from a captured mouse
/// button + modifiers, or `None` for a button we refuse to bind.
///
/// Primary and Secondary are deliberately unbindable: binding a mouse button
/// also *suppresses* it system-wide (see [`crate::mouse_hook`]), and someone
/// who loses left-click can no longer click their way back here to undo it.
/// `hotkeys::parse_combo` rejects them for the same reason, so a hand-edited
/// `settings.json` can't sneak one in either. Useful side effect: the Primary
/// click that arms recording is never itself recordable.
pub(super) fn combo_from_pointer(
    button: egui::PointerButton,
    mods: egui::Modifiers,
) -> Option<String> {
    let name = match button {
        egui::PointerButton::Middle => "mouse3",
        egui::PointerButton::Extra1 => "mouse4",
        egui::PointerButton::Extra2 => "mouse5",
        egui::PointerButton::Primary | egui::PointerButton::Secondary => return None,
    };
    Some(with_modifiers(name, mods))
}

/// Whether two hotkey combo strings parse to the identical (modifiers, vk)
/// pair — the condition `SettingsApp::validate` rejects, since Windows can
/// only register one of two identical `RegisterHotKey` calls and the loser
/// just silently never fires. An unparsable combo is "not a conflict" here —
/// `validate` already surfaces that parse error on its own before this ever
/// runs. Comparing the *parsed* form (not the raw strings) is what makes this
/// case-insensitive and order-independent, matching `parse_combo`'s own
/// normalisation (e.g. "Ctrl+Shift+D" and "shift+ctrl+d" both parse to the
/// same pair).
pub(super) fn hotkeys_conflict(a: &str, b: &str) -> bool {
    matches!(
        (crate::hotkeys::parse_combo(a), crate::hotkeys::parse_combo(b)),
        (Ok(x), Ok(y)) if x == y
    )
}
