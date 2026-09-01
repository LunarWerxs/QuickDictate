//! Capturing a hotkey while a field is recording — the next real keypress
//! or mouse button, with Escape as cancel.

use crate::settings_ui::*;

impl SettingsApp {
    /// While a hotkey field is recording, capture the next real keypress **or
    /// mouse button** into it (Escape cancels). Modifier-only presses are
    /// ignored (egui only fires `Key` events for actual keys, carrying
    /// modifiers alongside).
    ///
    /// Mouse buttons arrive as `PointerButton` rather than `Key`, which is why
    /// they were previously uncapturable: this only ever inspected `Key`
    /// events, so clicking the middle or a thumb button while armed did
    /// nothing at all. Left/right click stay unbindable (see
    /// [`combo_from_pointer`]), which also means the very click that arms
    /// recording can never be recorded as the binding.
    pub(crate) fn capture_hotkey(&mut self, ctx: &egui::Context) {
        let Some(field) = self.recording else {
            // Not armed: make sure any lease from a finished recording is
            // dropped, so mouse hotkeys are live again immediately.
            crate::mouse_hook::end_capture_lease();
            return;
        };
        // Armed: hold the global mouse hook passive. Without this, pressing an
        // ALREADY-BOUND mouse button in order to rebind it would fire the
        // hotkey (and get swallowed before egui ever saw it) instead of being
        // recorded. Renewed every frame, and self-expiring if this window goes
        // away mid-record.
        crate::mouse_hook::capture_lease();
        let captured = ctx.input(|i| {
            for ev in &i.events {
                match ev {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        modifiers,
                        ..
                    } => {
                        if *key == egui::Key::Escape {
                            return Some(None);
                        }
                        if let Some(combo) = combo_from_event(*key, *modifiers) {
                            return Some(Some(combo));
                        }
                    }
                    egui::Event::PointerButton {
                        button,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        if let Some(combo) = combo_from_pointer(*button, *modifiers) {
                            return Some(Some(combo));
                        }
                    }
                    _ => {}
                }
            }
            None
        });
        match captured {
            Some(Some(combo)) => {
                match field {
                    HotkeyField::Toggle => self.draft.toggle_hotkey = combo,
                    HotkeyField::Hold => self.draft.hold_hotkey = combo,
                }
                self.recording = None;
            }
            Some(None) => self.recording = None, // Escape cancelled
            None => ctx.request_repaint(),       // keep listening
        }
    }
}
