//! Keys that have passed their provider's account check, remembered on disk
//! so the check runs once per key rather than at every launch.
//!
//! Some providers accept a connection and only reject the ACCOUNT later.
//! ElevenLabs closes a session with `unaccepted_terms` once about ten seconds
//! of audio have gone in when the account never accepted the Scribe terms,
//! and nothing cheaper gives it away: the connect, a realtime token, the user
//! endpoint and a batch transcription all succeed for such a key (measured
//! 2026-09-18). So the check (`stt::dispatch::check_account`) streams that
//! much audio, which spends that much of the key's quota. A pass is a lasting
//! fact about the account, so it is recorded here and never checked again. A
//! failure is NOT recorded: a key whose owner has since accepted the terms is
//! simply checked again at the next launch, which keeps the key pool's rule
//! that no key is branded dead across runs (see `keys.rs`).
//!
//! Holds only a truncated SHA-256 of provider and key, never the key. Loading
//! tolerates a missing or unreadable file by treating every key as unchecked:
//! the worst case is one more check.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// File name inside the data folder. Listed in `paths::RELOCATABLE` so a
/// data-folder move carries it along.
pub const KEY_CHECKS_FILE: &str = "quickdictate-key-checks.json";

/// Bumped only if the shape below changes incompatibly. A reader that meets a
/// newer version treats every key as unchecked.
const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct ChecksFile {
    version: u32,
    /// Fingerprints (see [`fingerprint`]) of keys that passed.
    passed: Vec<String>,
}

/// The prewarm checks every new key at once, so passes land concurrently;
/// this keeps their read-modify-write of the file one at a time.
static WRITE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn path() -> PathBuf {
    crate::paths::data_file(KEY_CHECKS_FILE)
}

/// A stable, one-way name for `key` under `provider`: the first 16 bytes of
/// SHA-256 over both, hex. Scoped by provider so a string reused across
/// providers is checked for each.
fn fingerprint(provider: &str, key: &str) -> String {
    let digest = Sha256::digest(format!("{provider}\n{key}").as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// Whether `key` has passed `provider`'s account check before.
pub fn has_passed(provider: &str, key: &str) -> bool {
    has_passed_in(&path(), provider, key)
}

/// Remember that `key` passed `provider`'s account check. A failure to save is
/// logged and otherwise ignored: the key just gets checked again next launch.
pub fn record_pass(provider: &str, key: &str) {
    if let Err(e) = record_pass_in(&path(), provider, key) {
        tracing::warn!("key checks: {e}");
    }
}

fn load(path: &Path) -> Vec<String> {
    let Ok(json) = fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str::<ChecksFile>(&json) {
        Ok(file) if file.version <= FORMAT_VERSION => file.passed,
        _ => Vec::new(),
    }
}

pub(crate) fn has_passed_in(path: &Path, provider: &str, key: &str) -> bool {
    let fp = fingerprint(provider, key);
    load(path).contains(&fp)
}

pub(crate) fn record_pass_in(path: &Path, provider: &str, key: &str) -> Result<(), String> {
    let _guard = WRITE_LOCK.lock();
    let mut passed = load(path);
    let fp = fingerprint(provider, key);
    if passed.contains(&fp) {
        return Ok(());
    }
    passed.push(fp);
    let file = ChecksFile {
        version: FORMAT_VERSION,
        passed,
    };
    // WRITE_LOCK, held above, is the serialization the helper asks for.
    crate::paths::write_json_atomically(path, &file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qd-key-checks-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join(KEY_CHECKS_FILE)
    }

    #[test]
    fn a_pass_is_remembered_per_provider_and_never_stores_the_key() {
        let path = temp_file("roundtrip");
        assert!(!has_passed_in(&path, "elevenlabs", "sk_secret_one"));
        record_pass_in(&path, "elevenlabs", "sk_secret_one").unwrap();
        record_pass_in(&path, "elevenlabs", "sk_secret_one").unwrap();
        assert!(has_passed_in(&path, "elevenlabs", "sk_secret_one"));
        assert!(!has_passed_in(&path, "elevenlabs", "sk_secret_two"));
        assert!(!has_passed_in(&path, "deepgram", "sk_secret_one"));
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("secret"), "only fingerprints on disk");
        assert_eq!(load(&path).len(), 1, "a repeat pass is not a second entry");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_unreadable_or_newer_file_means_unchecked() {
        let path = temp_file("garbage");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json").unwrap();
        assert!(!has_passed_in(&path, "elevenlabs", "k"));
        let fp = fingerprint("elevenlabs", "k");
        fs::write(&path, format!(r#"{{"version":99,"passed":["{fp}"]}}"#)).unwrap();
        assert!(!has_passed_in(&path, "elevenlabs", "k"));
        // A garbage file is replaced by a good one on the next pass.
        fs::write(&path, "not json").unwrap();
        record_pass_in(&path, "elevenlabs", "k").unwrap();
        assert!(has_passed_in(&path, "elevenlabs", "k"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
