//! "Start with Windows" via the per-user Run key.
//!
//! Reconciles `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\QuickDictate`
//! with the `run_at_startup` setting on every launch: set → value written
//! (quoted path to the current exe, so it survives the exe being moved and
//! then relaunched from the new location); unset → value removed. Per-user
//! key, so no elevation is needed.

use std::path::Path;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "QuickDictate";

/// The Run-key command string for `exe`: the path is quoted so a space in it
/// (e.g. "C:\Program Files\...") doesn't get split into multiple arguments
/// when Windows parses the Run-key value as a command line.
fn run_command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// What to do to the Run-key value for the given `enabled` setting.
enum RunKeyAction {
    Set(String),
    Remove,
}

/// Decide whether autostart should write or remove the Run-key value — pure,
/// no registry access.
fn run_key_action(enabled: bool, exe: &Path) -> RunKeyAction {
    if enabled {
        RunKeyAction::Set(run_command(exe))
    } else {
        RunKeyAction::Remove
    }
}

pub fn reconcile(enabled: bool) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("autostart: current_exe failed: {e}");
            return;
        }
    };
    let key = match windows_registry::CURRENT_USER.create(RUN_KEY) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!("autostart: open Run key failed: {e}");
            return;
        }
    };
    match run_key_action(enabled, &exe) {
        RunKeyAction::Set(cmd) => match key.set_string(VALUE_NAME, &cmd) {
            Ok(()) => tracing::info!("autostart: enabled ({cmd})"),
            Err(e) => tracing::warn!("autostart: set failed: {e}"),
        },
        RunKeyAction::Remove => {
            // Only log when there was actually something to remove.
            if key.get_string(VALUE_NAME).is_ok() {
                match key.remove_value(VALUE_NAME) {
                    Ok(()) => tracing::info!("autostart: disabled (Run entry removed)"),
                    Err(e) => tracing::warn!("autostart: remove failed: {e}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_command_quotes_a_path_with_spaces() {
        let exe = Path::new("C:\\Program Files\\QuickDictate\\QuickDictate.exe");
        assert_eq!(
            run_command(exe),
            "\"C:\\Program Files\\QuickDictate\\QuickDictate.exe\""
        );
    }

    #[test]
    fn run_command_quotes_a_path_without_spaces_too() {
        let exe = Path::new("C:\\QuickDictate.exe");
        assert_eq!(run_command(exe), "\"C:\\QuickDictate.exe\"");
    }

    #[test]
    fn enabling_produces_a_set_action_with_the_quoted_command() {
        let exe = Path::new("C:\\Program Files\\QuickDictate\\QuickDictate.exe");
        match run_key_action(true, exe) {
            RunKeyAction::Set(cmd) => assert_eq!(cmd, run_command(exe)),
            RunKeyAction::Remove => panic!("expected a Set action"),
        }
    }

    #[test]
    fn disabling_produces_a_remove_action() {
        let exe = Path::new("C:\\QuickDictate.exe");
        assert!(matches!(run_key_action(false, exe), RunKeyAction::Remove));
    }
}
