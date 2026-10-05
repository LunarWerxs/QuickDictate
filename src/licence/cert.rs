//! Offline licence certificates: a signed statement from Connections that this
//! installation holds a licence, verified locally with no network at all.
//!
//! The format and the checks are the ones SageThumbs 2K's `licence_cert.rs`
//! pinned against live certificates: `<base64url payload>.<base64url
//! signature>`, Ed25519, signed over the base64url PAYLOAD TEXT, with no key id.
//!
//! FAIL DIRECTION. Every error here means "no certificate", never "not
//! licensed": the caller falls back to what it knew before (a copy that once
//! held a licence keeps working, see [`super::posture`]). A certificate can
//! only ever ADD standing.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::Value;

use super::posture::Plan;

/// The Connections platform's Ed25519 public key, as the raw 32 bytes (the
/// tail of the SPKI DER `MCowBQYDK2VwAyEA7QfTmePW6SbRDXvdjl1tHRMZYpHZ/dNhu2XIwJhuZ6w=`).
/// The same bytes SageThumbs 2K embeds: one platform key signs every
/// product's certificates, which is why [`AUDIENCE`] and the product are
/// checked as well as the signature.
///
/// Compiled in on purpose: an offline check cannot fetch a key. There is no
/// key id, so if Connections ever rotates it, new certificates stop verifying
/// in copies already shipped. That degrades to "no certificate", which the
/// fail direction above makes survivable.
const VERIFY_KEY: [u8; 32] = [
    0xed, 0x07, 0xd3, 0x99, 0xe3, 0xd6, 0xe9, 0x26, 0xd1, 0x0d, 0x7b, 0xdd, 0x8e, 0x5d, 0x6d, 0x1d,
    0x13, 0x19, 0x62, 0x91, 0xd9, 0xfd, 0xd3, 0x61, 0xbb, 0x65, 0xc8, 0xc0, 0x98, 0x6e, 0x67, 0xac,
];

/// The audience every licence certificate carries. The same key signs other
/// short-lived token kinds: skip this and one of those verifies as a licence.
const AUDIENCE: &str = "connections-licence";

/// What a good certificate says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Verified {
    pub product_id: &'static str,
    pub plan: Plan,
    /// The certificate's own `exp`, in Unix seconds.
    pub exp_unix: u64,
}

/// Why a certificate did not verify. Every variant means "no certificate";
/// they are told apart only for the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CertError {
    /// Not `<payload>.<signature>`.
    Malformed,
    /// A segment was not unpadded base64url, or the signature not 64 bytes.
    BadEncoding,
    /// Ed25519 said no. Also what a key rotation looks like from here.
    BadSignature,
    /// Signed, but not a payload we understand.
    BadPayload,
    /// Signed for something other than a licence.
    WrongAudience,
    /// A licence for some other product.
    WrongProduct,
    /// A licence for another installation.
    NotThisInstall,
    /// Correct, but past its `exp`. Replaying the key mints a fresh one.
    Expired,
}

/// The claims this app acts on. Unknown fields are ignored, so Connections can
/// add some without breaking copies already shipped; a missing or mistyped
/// field this app needs is a payload it does not understand.
pub(super) struct Claims {
    aud: String,
    product: String,
    sub: String,
    exp: u64,
}

impl Claims {
    pub(super) fn from_json(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        let text = |k: &str| obj.get(k).and_then(Value::as_str).map(String::from);
        Some(Self {
            aud: text("aud")?,
            product: text("product")?,
            sub: text("sub")?,
            exp: obj.get("exp").and_then(Value::as_u64)?,
        })
    }
}

/// Verify `cert` for the installation `expected_sub` at `now_unix`. A pure
/// function of its inputs: the clock is a parameter so the tests can pin it.
pub(crate) fn verify(cert: &str, expected_sub: &str, now_unix: u64) -> Result<Verified, CertError> {
    let (payload_b64, sig_b64) = cert.trim().split_once('.').ok_or(CertError::Malformed)?;
    if payload_b64.is_empty() || sig_b64.is_empty() || sig_b64.contains('.') {
        return Err(CertError::Malformed);
    }
    let claims = decode_and_verify(payload_b64, sig_b64)?;
    check_claims(&claims, expected_sub, now_unix)
}

/// The checks after the signature: audience, product, installation, expiry.
/// Split out so a test can drive each refusal with claims no real key signed.
pub(super) fn check_claims(
    claims: &Claims,
    expected_sub: &str,
    now_unix: u64,
) -> Result<Verified, CertError> {
    if claims.aud != AUDIENCE {
        return Err(CertError::WrongAudience);
    }
    let product = super::product_by_id(&claims.product).ok_or(CertError::WrongProduct)?;
    if claims.sub != expected_sub {
        return Err(CertError::NotThisInstall);
    }
    if claims.exp <= now_unix {
        return Err(CertError::Expired);
    }
    Ok(Verified {
        product_id: product.id,
        plan: product.plan,
        exp_unix: claims.exp,
    })
}

fn decode_and_verify(payload_b64: &str, sig_b64: &str) -> Result<Claims, CertError> {
    let sig_raw = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| CertError::BadEncoding)?;
    let sig_bytes: [u8; 64] = sig_raw
        .as_slice()
        .try_into()
        .map_err(|_| CertError::BadEncoding)?;
    let key = VerifyingKey::from_bytes(&VERIFY_KEY).map_err(|_| CertError::BadSignature)?;
    // Signed over the base64url TEXT, not the decoded JSON: verify the bytes of
    // the payload segment exactly as they arrived, then decode.
    key.verify(payload_b64.as_bytes(), &Signature::from_bytes(&sig_bytes))
        .map_err(|_| CertError::BadSignature)?;
    let json = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| CertError::BadEncoding)?;
    let value: Value = serde_json::from_slice(&json).map_err(|_| CertError::BadPayload)?;
    Claims::from_json(&value).ok_or(CertError::BadPayload)
}
