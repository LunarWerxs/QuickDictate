//! The one secret kept on disk: the refresh token, DPAPI-sealed.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::secretstore::dpapi;

use super::CREDS_FILE;

// ---- Persisted credentials (DPAPI-sealed) ----------------------------------

/// What we keep between sessions. The **refresh token** is the only secret; the
/// email/name are cached purely so the UI can show "Synced as …" instantly on
/// open without a network round-trip. Sealed as one DPAPI blob on disk.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Creds {
    pub refresh_token: String,
    #[serde(default)]
    pub sub: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub name: String,
    /// Profile-picture URL (from userinfo's `picture` claim, `photo` scope). Cached so the UI can
    /// re-fetch + show the avatar on open without a fresh sign-in.
    #[serde(default)]
    pub picture: String,
}

fn creds_path() -> Option<PathBuf> {
    Some(crate::paths::data_file(CREDS_FILE))
}

pub fn save_creds(c: &Creds) -> Result<()> {
    let json = serde_json::to_vec(c)?;
    let sealed = dpapi(true, &json).ok_or_else(|| anyhow!("DPAPI encrypt failed"))?;
    let path = creds_path().ok_or_else(|| anyhow!("cannot locate creds path"))?;
    // Write atomically (tmp + rename) so a reader (or a racing writer) never
    // observes a half-written / truncated sealed blob.
    let tmp = path.with_extension("dat.tmp");
    std::fs::write(&tmp, &sealed).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

pub fn load_creds() -> Option<Creds> {
    let sealed = std::fs::read(creds_path()?).ok()?;
    let json = dpapi(false, &sealed)?;
    serde_json::from_slice(&json).ok()
}

pub fn clear_creds() {
    if let Some(p) = creds_path() {
        let _ = std::fs::remove_file(p);
    }
}

/// Cheap "are we signed in?" — true iff a decryptable creds blob exists.
pub fn is_signed_in() -> bool {
    load_creds().is_some()
}

// ---- Crypto helpers (Windows CNG + DPAPI, no extra crates) -----------------
