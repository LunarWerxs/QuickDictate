//! The pseudonymous per-install id that keys the opt-in usage report
//! (`stats::report`). Update checks used to send it as `X-Install-Id`; they no
//! longer do (see `latest_request`), but the id is still generated here, at
//! startup, so it is the same one whenever the report is switched on.
//!
//! Crypto-random and derived from nothing about the machine, so it is tied to
//! no name. It is pseudonymous, not anonymous: it does link one install's
//! reports together.

use std::sync::Arc;

use crate::config::Config;
use crate::state::App;

// ---------------------------------------------------------------------------
// Pseudonymous install id
// ---------------------------------------------------------------------------

/// Crypto-random UUIDv4 via CNG (`BCryptGenRandom`, the same checked call as
/// `sync.rs::rand_bytes`). Deliberately **never** derived from hostname, MAC,
/// username, or any other machine identifier — the id must identify nothing
/// but itself. `None` if the system RNG fails (no id beats a predictable one).
/// The licence module reuses it for its own install id (`licence::store`).
pub(crate) fn new_install_id() -> Option<String> {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let mut b = [0u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            BCRYPT_ALG_HANDLE::default(),
            &mut b,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if !status.is_ok() {
        return None;
    }
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    ))
}

/// Make sure the pseudonymous install id exists: keep the one persisted in
/// settings.json, or on the very first launch generate a fresh UUID and
/// persist it (via [`Config::save_install_id`], which fills the template's
/// empty slot in place rather than rewriting the whole file). Called once
/// from `main()` before anything else can save settings.json. An id that
/// failed to persist is **not** kept: it would change every launch, and the
/// usage report would count one machine as many.
pub fn init_install_id(app: &App) {
    let cfg = app.config.load();
    if !cfg.install_id.trim().is_empty() {
        return;
    }
    let Some(id) = new_install_id() else {
        tracing::warn!("update: system RNG failed; no install id this launch");
        return;
    };
    let mut new_cfg = (**cfg).clone();
    new_cfg.install_id = id;
    match new_cfg.save_install_id(&Config::settings_path()) {
        Ok(()) => {
            app.config.store(Arc::new(new_cfg));
            tracing::info!("update: generated pseudonymous install id");
        }
        Err(e) => {
            tracing::warn!("update: could not persist install id ({e}); none this launch");
        }
    }
}
