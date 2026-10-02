//! Where the licence facts live: one per-user registry key, outside
//! settings.json on purpose.
//!
//! settings.json travels with a portable folder and is synced between devices
//! (a subset of it), and none of this may: the mode is one person's answer on
//! one Windows account, and the certificate names this installation. So it all
//! sits in `HKCU\Software\LunarWerx\QuickDictate\Licence`. The key and the
//! certificate are sealed with DPAPI ([`crate::secretstore`], CurrentUser
//! scope) before they are written; everything else is a plain value, because
//! none of it is secret and the trust boundary in the module docs already says
//! a user can edit their own hive.
//!
//! Every read fails toward Personal: a missing key, a missing value, a value of
//! the wrong type, a blob that will not unseal, all read as "nothing here".

use super::cert;
use super::posture::{self, CertFacts, Facts, Mode, Plan, RenewalInputs};
use super::redeem::{self, Reply};
use super::RedeemReport;

const DEFAULT_PATH: &str = r"Software\LunarWerx\QuickDictate\Licence";

/// Redirects the whole store to another HKCU key, for TEST ISOLATION: the
/// headless screenshot script runs a scratch copy that must neither read nor
/// answer the real question. Honoured only under `Software\`.
const PATH_ENV: &str = "QUICKDICTATE_LICENCE_REGKEY";

const V_MODE: &str = "Mode";
const V_EVALUATION: &str = "EvaluationStarted";
const V_INSTALL_ID: &str = "InstallId";
const V_LICENSED_SINCE: &str = "LicensedSince";
const V_REFUSED: &str = "Refused";
const V_PLAN: &str = "Plan";
/// DPAPI-sealed. The only full copy of the key, kept so renewal can replay it.
const V_KEY: &str = "Key";
const V_KEY_LAST4: &str = "KeyLast4";
/// DPAPI-sealed.
const V_CERTIFICATE: &str = "Certificate";
const V_PENDING: &str = "CertificateError";
const V_RENEWED_AT: &str = "LastRenewal";
const V_RENEWED_OK: &str = "LastRenewalOk";

/// The salt joined to the machine id before hashing, so Connections never sees
/// the raw Windows `MachineGuid`, only a value specific to this product. Not a
/// secret (it is in every copy); it makes the fingerprint its own namespace.
const FINGERPRINT_SALT: &str = "QuickDictate-seat-v1";

pub(super) struct Store {
    path: String,
}

impl Store {
    /// This user's store (or the test redirect, see [`PATH_ENV`]).
    pub(super) fn user() -> Self {
        let path = std::env::var(PATH_ENV)
            .ok()
            .filter(|p| p.starts_with(r"Software\"))
            .unwrap_or_else(|| DEFAULT_PATH.to_string());
        Self { path }
    }

    #[cfg(test)]
    pub(super) fn at(path: &str) -> Self {
        Self { path: path.into() }
    }

    fn open(&self) -> Option<windows_registry::Key> {
        windows_registry::CURRENT_USER.open(&self.path).ok()
    }

    fn create(&self) -> Option<windows_registry::Key> {
        match windows_registry::CURRENT_USER.create(&self.path) {
            Ok(k) => Some(k),
            Err(e) => {
                tracing::warn!("licence: cannot open its registry key ({e})");
                None
            }
        }
    }

    fn get_u64(&self, name: &str) -> u64 {
        self.open().and_then(|k| k.get_u64(name).ok()).unwrap_or(0)
    }

    fn get_string(&self, name: &str) -> Option<String> {
        self.open()
            .and_then(|k| k.get_string(name).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn put_u64(&self, name: &str, value: u64) {
        if let Some(k) = self.create() {
            if let Err(e) = k.set_u64(name, value) {
                tracing::warn!("licence: cannot write {name} ({e})");
            }
        }
    }

    fn put_string(&self, name: &str, value: &str) {
        if let Some(k) = self.create() {
            if let Err(e) = k.set_string(name, value) {
                tracing::warn!("licence: cannot write {name} ({e})");
            }
        }
    }

    /// Delete a value. Needs write access: `open` is read-only, and removing
    /// through it fails without a word.
    fn remove(&self, name: &str) {
        let key = windows_registry::CURRENT_USER
            .options()
            .read()
            .write()
            .open(&self.path);
        if let Ok(k) = key {
            let _ = k.remove_value(name);
        }
    }

    /// Unseal a DPAPI value. Only sealed values count: a plaintext one was not
    /// written by this module.
    fn get_sealed(&self, name: &str) -> Option<String> {
        let stored = self.get_string(name)?;
        if !crate::secretstore::is_sealed(&stored) {
            return None;
        }
        crate::secretstore::unseal_secret(&stored)
    }

    /// Seal and write. `false` when DPAPI failed, in which case nothing is
    /// written: a secret is never stored in the clear.
    fn put_sealed(&self, name: &str, plaintext: &str) -> bool {
        match crate::secretstore::seal_secret(plaintext) {
            Some(sealed) => {
                self.put_string(name, &sealed);
                true
            }
            None => {
                tracing::warn!("licence: DPAPI could not seal {name}; not stored");
                false
            }
        }
    }

    // ---- The facts ------------------------------------------------------

    pub(super) fn mode(&self) -> Option<Mode> {
        posture::parse_mode(self.get_string(V_MODE).as_deref())
    }

    pub(super) fn set_mode(&self, mode: Mode) {
        let v = match mode {
            Mode::Personal => "personal",
            Mode::Business => "business",
        };
        self.put_string(V_MODE, v);
    }

    /// Start the business evaluation on a Business copy that never held a
    /// licence and has no clock. Returns whether it wrote anything.
    ///
    /// Board #7127: when Connections holds the evaluation clock server-side,
    /// this is the one function that asks it instead of starting a local one.
    pub(super) fn start_evaluation_if_due(&self, now: u64) -> bool {
        if self.mode() != Some(Mode::Business)
            || self.get_u64(V_EVALUATION) != 0
            || self.get_u64(V_LICENSED_SINCE) != 0
        {
            return false;
        }
        self.put_u64(V_EVALUATION, now);
        tracing::info!("licence: business evaluation started");
        true
    }

    /// This installation's id, created once: `qd-` and a random UUID, derived
    /// from nothing about the machine. It is the certificate's `sub`.
    pub(super) fn install_id(&self) -> Option<String> {
        if let Some(id) = self.get_string(V_INSTALL_ID) {
            return Some(id);
        }
        let id = format!("qd-{}", crate::update::new_install_id()?);
        self.put_string(V_INSTALL_ID, &id);
        // Read back: an id that did not persist would change on every call,
        // and a certificate minted for it could never verify again.
        self.get_string(V_INSTALL_ID)
    }

    /// The stored certificate, if it verifies for this install at `now`.
    fn verified_certificate(&self, now: u64) -> Option<cert::Verified> {
        let sub = self.get_string(V_INSTALL_ID)?;
        let c = self.get_sealed(V_CERTIFICATE)?;
        match cert::verify(&c, &sub, now) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!("licence: stored certificate not used ({e:?})");
                None
            }
        }
    }

    pub(super) fn load_facts(&self, now: u64) -> Facts {
        if self.open().is_none() {
            return Facts::default();
        }
        Facts {
            mode: self.mode(),
            evaluation_started_unix: self.get_u64(V_EVALUATION),
            licensed_since_unix: self.get_u64(V_LICENSED_SINCE),
            refused_unix: self.get_u64(V_REFUSED),
            last_plan: parse_plan(self.get_string(V_PLAN).as_deref()),
            certificate: self.verified_certificate(now).map(|v| CertFacts {
                plan: v.plan,
                exp_unix: v.exp_unix,
            }),
            key_last4: self.get_string(V_KEY_LAST4),
            pending_reason: self.get_string(V_PENDING),
        }
    }

    pub(super) fn renewal_inputs(&self, now: u64) -> RenewalInputs {
        RenewalInputs {
            has_key: self.get_string(V_KEY).is_some(),
            refused: self.get_u64(V_REFUSED) != 0,
            cert_exp_unix: self.verified_certificate(now).map(|v| v.exp_unix),
            last_attempt_unix: self.get_u64(V_RENEWED_AT),
            last_attempt_ok: self.get_u64(V_RENEWED_OK) != 0,
        }
    }

    /// The stored key, unsealed, in canonical form.
    pub(super) fn stored_key(&self) -> Option<String> {
        redeem::normalize_key(&self.get_sealed(V_KEY)?)
    }

    pub(super) fn note_renewal_attempt(&self, now: u64, ok: bool) {
        self.put_u64(V_RENEWED_AT, now);
        self.put_u64(V_RENEWED_OK, u64::from(ok));
    }

    /// Record what a redeem (`is_renewal == false`: the user typed `key`) or a
    /// renewal (the stored key replayed) answered, and say it in words.
    pub(super) fn apply_redeem(
        &self,
        key: &str,
        reply: Reply,
        now: u64,
        is_renewal: bool,
    ) -> RedeemReport {
        match reply {
            Reply::Accepted {
                certificate,
                certificate_error,
                ..
            } => self.apply_accepted(key, certificate, certificate_error, now),
            Reply::Refused => {
                // A refusal ends the stored licence only when it is about the
                // stored key. A different key typed in and refused changes
                // nothing about the licence already held.
                if is_renewal || self.stored_key().as_deref() == Some(key) {
                    if self.get_u64(V_REFUSED) == 0 {
                        self.put_u64(V_REFUSED, now);
                    }
                    self.note_renewal_attempt(now, true);
                    tracing::info!("licence: Connections refused the stored key");
                }
                RedeemReport::Rejected {
                    message: "That key wasn't accepted: it is unknown, or its licence has \
                              expired or been cancelled."
                        .into(),
                }
            }
            Reply::Rejected { message } => {
                if is_renewal {
                    self.note_renewal_attempt(now, false);
                }
                RedeemReport::Rejected { message }
            }
            Reply::Unavailable => {
                if is_renewal {
                    self.note_renewal_attempt(now, false);
                }
                RedeemReport::Unavailable
            }
        }
    }

    fn apply_accepted(
        &self,
        key: &str,
        certificate: Option<String>,
        certificate_error: Option<String>,
        now: u64,
    ) -> RedeemReport {
        if self.stored_key().as_deref() != Some(key) {
            // A new key replaces the old licence wholesale, its certificate
            // included: a certificate for the old key must not linger as if it
            // were this one's.
            self.remove(V_CERTIFICATE);
            self.remove(V_PLAN);
        }
        self.put_sealed(V_KEY, key);
        self.put_string(V_KEY_LAST4, &redeem::key_last4(key));
        self.remove(V_REFUSED);
        if self.get_u64(V_LICENSED_SINCE) == 0 {
            self.put_u64(V_LICENSED_SINCE, now);
        }
        let verified = match (certificate.as_deref(), self.install_id()) {
            (Some(c), Some(sub)) => cert::verify(c, &sub, now).ok().map(|v| (c, v)),
            _ => None,
        };
        match verified {
            Some((c, v)) => {
                self.put_sealed(V_CERTIFICATE, c);
                self.put_string(V_PLAN, v.plan.label());
                self.remove(V_PENDING);
                self.note_renewal_attempt(now, true);
                RedeemReport::Licensed { plan: v.plan }
            }
            None => {
                let reason = certificate_error
                    .unwrap_or_else(|| "the certificate it sent did not verify".into());
                self.put_string(V_PENDING, &reason);
                self.note_renewal_attempt(now, false);
                RedeemReport::AcceptedNoCertificate { reason }
            }
        }
    }
}

fn parse_plan(raw: Option<&str>) -> Option<Plan> {
    match raw? {
        "perpetual" => Some(Plan::Perpetual),
        "monthly" => Some(Plan::Monthly),
        _ => None,
    }
}

/// An opaque, stable identifier for this machine: lowercase hex SHA-256 of
/// `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` joined with
/// [`FINGERPRINT_SALT`]. `None` only when that value cannot be read, which
/// callers treat as "try again", never as a bad key.
pub(super) fn device_fingerprint() -> Option<String> {
    let guid = windows_registry::LOCAL_MACHINE
        .open(r"SOFTWARE\Microsoft\Cryptography")
        .and_then(|k| k.get_string("MachineGuid"))
        .ok()?;
    Some(fingerprint_from_guid(guid.trim()))
}

/// The pure half of [`device_fingerprint`], so the salting and hex can be
/// pinned without HKLM.
pub(super) fn fingerprint_from_guid(guid: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{guid}{FINGERPRINT_SALT}").as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
