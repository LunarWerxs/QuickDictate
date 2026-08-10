//! Windows DPAPI seal/unseal, shared by everything in QuickDictate that keeps
//! a secret on disk.
//!
//! This used to live privately in `sync.rs` (sealing the settings-sync refresh
//! token). It moved here when `config.rs` gained the optional
//! `protect_keys_at_rest` setting, so both callers use one implementation
//! rather than two copies of the same FFI.
//!
//! Scope is **CurrentUser**: a sealed blob only decrypts for the Windows
//! account that sealed it, on the machine that sealed it. That is the whole
//! point for a credential, and it is also exactly why key sealing is opt-in:
//! QuickDictate is a portable app, and a sealed settings.json does not travel
//! with the folder.

/// DPAPI seal (`encrypt = true`) / unseal (`encrypt = false`), CurrentUser
/// scope, never prompting. `None` on any failure, including "this blob was
/// sealed by a different user or machine".
#[cfg(windows)]
pub fn dpapi(encrypt: bool, data: &[u8]) -> Option<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    unsafe {
        let inb = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        let res = if encrypt {
            CryptProtectData(
                &inb,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &inb,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if res.is_err() || out.pbData.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out.pbData as *mut core::ffi::c_void));
        Some(bytes)
    }
}

/// Marker prefix for a sealed value inside settings.json. A key that does not
/// start with this is plaintext, which is how every settings.json written
/// before v0.5.4 (and every one written with `protect_keys_at_rest` off) looks.
pub const SEALED_PREFIX: &str = "dpapi:";

/// Seal one API key for storage. Returns `None` if DPAPI failed, and the
/// caller must then keep the plaintext rather than write an unusable value.
#[cfg(windows)]
pub fn seal_secret(plaintext: &str) -> Option<String> {
    use base64::Engine;
    let sealed = dpapi(true, plaintext.as_bytes())?;
    Some(format!(
        "{SEALED_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(sealed)
    ))
}

/// Reverse of [`seal_secret`]. A value without the prefix is already plaintext
/// and is returned unchanged, so a half-migrated file keeps working. A value
/// with the prefix that will not decrypt (copied from another machine or
/// account) yields `None`, which the caller surfaces as "key unavailable"
/// rather than silently sending garbage to a provider.
#[cfg(windows)]
pub fn unseal_secret(stored: &str) -> Option<String> {
    use base64::Engine;
    let Some(b64) = stored.strip_prefix(SEALED_PREFIX) else {
        return Some(stored.to_string());
    };
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let plain = dpapi(false, &raw)?;
    String::from_utf8(plain).ok()
}

/// Whether a stored value is in sealed form.
pub fn is_sealed(stored: &str) -> bool {
    stored.starts_with(SEALED_PREFIX)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn seal_then_unseal_round_trips() {
        let sealed = seal_secret("sk-test-abc123").expect("DPAPI available");
        assert!(is_sealed(&sealed));
        assert_ne!(sealed, "sk-test-abc123");
        assert_eq!(unseal_secret(&sealed).as_deref(), Some("sk-test-abc123"));
    }

    #[test]
    fn plaintext_passes_through_unsealed() {
        assert!(!is_sealed("sk-plain"));
        assert_eq!(unseal_secret("sk-plain").as_deref(), Some("sk-plain"));
    }

    #[test]
    fn undecryptable_sealed_value_is_none_not_garbage() {
        // Well-formed prefix + base64, but not a real DPAPI blob.
        assert_eq!(unseal_secret("dpapi:AAAABBBBCCCC"), None);
    }

    #[test]
    fn empty_secret_round_trips() {
        let sealed = seal_secret("").expect("DPAPI available");
        assert_eq!(unseal_secret(&sealed).as_deref(), Some(""));
    }
}
