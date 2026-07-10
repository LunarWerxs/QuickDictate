//! "Start with Windows" via the per-user Run key.
//!
//! Reconciles `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\QuickDictate`
//! with the `run_at_startup` setting on every launch: set → value written
//! (quoted path to the current exe, so it survives the exe being moved and
//! then relaunched from the new location); unset → value removed. Per-user
//! key, so no elevation is needed.

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "QuickDictate";

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
    if enabled {
        let cmd = format!("\"{}\"", exe.display());
        match key.set_string(VALUE_NAME, &cmd) {
            Ok(()) => tracing::info!("autostart: enabled ({cmd})"),
            Err(e) => tracing::warn!("autostart: set failed: {e}"),
        }
    } else {
        // Only log when there was actually something to remove.
        if key.get_string(VALUE_NAME).is_ok() {
            match key.remove_value(VALUE_NAME) {
                Ok(()) => tracing::info!("autostart: disabled (Run entry removed)"),
                Err(e) => tracing::warn!("autostart: remove failed: {e}"),
            }
        }
    }
}
