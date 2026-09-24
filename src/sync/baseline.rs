//! The synced document as this machine last saw it: the base of the three-way
//! merge (`schema::changes_since` for a push, `schema::remote_changes` for a
//! pull). Set by the pulls this machine applies and patched by the pushes it
//! makes, never simply copied from the cloud: another PC's change is "seen"
//! only once it has been applied here.
//!
//! Kept on disk in the data folder, so it survives a restart. Without it the
//! first pull after a restart would revert a change this PC saved while
//! offline. It holds only synced, secret-free preferences (the usage stats are
//! stripped; they merge on their own), and is deleted on disconnect.

use parking_lot::Mutex;
use serde_json::Value;

use super::STATS_KEY;

/// File name inside the data folder. Listed in `paths::RELOCATABLE`.
pub(crate) const BASELINE_FILE: &str = "quickdictate-sync-baseline.json";

/// `None` until first read from disk; then `Some(None)` for "no baseline".
static BASELINE: Mutex<Option<Option<Value>>> = Mutex::new(None);

fn path() -> std::path::PathBuf {
    crate::paths::data_file(BASELINE_FILE)
}

/// The document as this machine last saw it, if it has seen one.
pub(super) fn get() -> Option<Value> {
    let mut slot = BASELINE.lock();
    slot.get_or_insert_with(|| {
        std::fs::read_to_string(path())
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .filter(Value::is_object)
    })
    .clone()
}

/// Record `doc` as what this machine has now seen.
pub(super) fn set(doc: &Value) {
    let mut seen = doc.clone();
    if let Some(object) = seen.as_object_mut() {
        object.remove(STATS_KEY);
    }
    // One lock for the file and the memory copy, so they cannot disagree.
    let mut slot = BASELINE.lock();
    if let Err(e) = crate::paths::write_json_atomically(&path(), &seen) {
        tracing::warn!("connections: could not record the sync baseline ({e})");
    }
    *slot = Some(Some(seen));
}

/// Patch the baseline with what a push just sent, if there is one. With none,
/// the next pull sets it: adopting the cloud copy here instead would mark
/// another PC's changes as seen before they were ever applied on this one.
pub(super) fn record_push(patch: &Value) {
    if let Some(mut seen) = get() {
        super::schema::merge_patch(&mut seen, patch);
        set(&seen);
    }
}

/// Forget it (sign-out): the next account starts from a clean slate.
pub(super) fn clear() {
    *BASELINE.lock() = Some(None);
    match std::fs::remove_file(path()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("connections: could not remove the sync baseline ({e})"),
    }
}
