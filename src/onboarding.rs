//! First-run / empty-key onboarding.
//!
//! QuickDictate is bring-your-own-key and unusable until *some* provider has a
//! key. When no provider has any key at launch we log a clear line; the caller
//! (`main`) also opens the Settings window, which carries an in-window "add a
//! key to get started" banner, so the user is taken straight to where they fix
//! it — no separate pop-up to dismiss first. Once any key is present this never
//! fires (and if only one provider has keys, the app auto-selects it — see
//! `Config::resolve_provider`).

/// Log the friendly "no API key" notice. The Settings window is opened by the
/// caller (see `main`) so the user lands directly on the fix; here we just
/// leave a clear breadcrumb in the log.
pub fn notify_no_key() {
    tracing::warn!(
        "No API keys configured for any provider. Opening Settings\u{2026} — pick a \
         provider, paste your key, then Save & Restart."
    );
}
