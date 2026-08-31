//! The anonymous per-install id sent as `X-Install-Id` on update checks.
//!
//! Crypto-random and derived from nothing about the machine, so it identifies
//! an install and not a person.

use std::sync::Arc;

use crate::config::Config;
use crate::state::App;

use super::*;

// ---------------------------------------------------------------------------
// Anonymous install id (X-Install-Id)
// ---------------------------------------------------------------------------

/// Crypto-random UUIDv4 via CNG (`BCryptGenRandom`, the same checked call as
/// `sync.rs::rand_bytes`). Deliberately **never** derived from hostname, MAC,
/// username, or any other machine identifier — the id must identify nothing
/// but itself. `None` if the system RNG fails (no id beats a predictable one).
pub(super) fn new_install_id() -> Option<String> {
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

/// Resolve the anonymous install id and cache it for [`fetch_latest_json`]:
/// reuse the one persisted in settings.json, or on the very first launch
/// generate a fresh UUID and persist it (via [`Config::save_install_id`],
/// which fills the template's empty slot in place rather than rewriting the
/// whole file). Called once from `main()` before any check can run — both
/// the startup auto-check and the tray/About manual path (which has no `App`
/// handle) read the cached value. An id that failed to persist is **not**
/// sent: it would change every launch and inflate the install count.
pub fn init_install_id(app: &App) {
    let cfg = app.config.load();
    let existing = cfg.install_id.trim();
    if !existing.is_empty() {
        let _ = INSTALL_ID.set(existing.to_string());
        return;
    }
    let Some(id) = new_install_id() else {
        tracing::warn!("update: system RNG failed; checks will carry no install id");
        return;
    };
    let mut new_cfg = (**cfg).clone();
    new_cfg.install_id = id.clone();
    match new_cfg.save_install_id(&Config::settings_path()) {
        Ok(()) => {
            app.config.store(Arc::new(new_cfg));
            let _ = INSTALL_ID.set(id);
            tracing::info!("update: generated anonymous install id");
        }
        Err(e) => {
            tracing::warn!("update: could not persist install id ({e}); checks will carry none");
        }
    }
}
