//! The recent-dictations list on disk, so a restart -- and in particular a
//! self-update, which restarts the app -- does not throw away everything you
//! said today.
//!
//! One small JSON file in the data folder, newest entry first, rewritten
//! whole after every change (the list is capped at 50 short strings, so a
//! full rewrite is a few kilobytes). Written atomically: to a `.tmp` beside
//! the file, flushed, then renamed over the old one, so a crash mid-write
//! leaves the previous file intact rather than a truncated one.
//!
//! Local only, by design. The file is dictated text, so it is never synced
//! (`persist_history` -- the on/off preference -- travels; the file does
//! not), never reported, and is deleted the moment the preference is turned
//! off. Loading tolerates a missing or unreadable file by starting empty:
//! history is a convenience, and no version of "could not read it" is worth
//! stopping the app for.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::state::HistoryEntry;

/// File name inside the data folder. Listed in `paths::RELOCATABLE` so a
/// data-folder move carries it along.
pub const HISTORY_FILE: &str = "quickdictate-history.json";

/// Bumped only if the shape below changes incompatibly. A reader that meets a
/// newer version starts empty rather than guessing.
const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct FileEntry {
    text: String,
    /// Unix milliseconds. Absent or zero reads as "unknown", which becomes
    /// the Unix epoch on load rather than a parse failure.
    #[serde(default)]
    when_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct HistoryFile {
    version: u32,
    /// Newest first, the same order `TranscriptHistory::snapshot` produces.
    entries: Vec<FileEntry>,
}

fn path() -> PathBuf {
    crate::paths::data_file(HISTORY_FILE)
}

/// The saved list, newest first. Missing, unreadable, unparsable, or from a
/// newer format: empty, with the reason logged.
pub fn load() -> Vec<(String, SystemTime)> {
    load_from(&path())
}

pub(crate) fn load_from(path: &Path) -> Vec<(String, SystemTime)> {
    let json = match fs::read_to_string(path) {
        Ok(json) => json,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(
                "history: could not read {}: {e}; starting empty",
                path.display()
            );
            return Vec::new();
        }
    };
    let file: HistoryFile = match serde_json::from_str(&json) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(
                "history: could not parse {}: {e}; starting empty",
                path.display()
            );
            return Vec::new();
        }
    };
    if file.version > FORMAT_VERSION {
        tracing::warn!(
            "history: {} is format v{} but this build reads v{}; starting empty",
            path.display(),
            file.version,
            FORMAT_VERSION
        );
        return Vec::new();
    }
    file.entries
        .into_iter()
        .filter(|e| !e.text.is_empty())
        .map(|e| (e.text, UNIX_EPOCH + Duration::from_millis(e.when_ms)))
        .collect()
}

/// Serializes every write and delete of the file, and the snapshot each write
/// saves. The output worker, a Settings save and the updater all save, from
/// different threads, into the one pid-named `.tmp`: unserialized, one save
/// truncates another's half-written temp file, and whichever renames last can
/// install garbage, or an older list over a newer one.
static FILE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Write the whole list (newest first) that `snapshot` returns. Returns the
/// error as text; callers log it and carry on, because a history that could
/// not be saved is still a history that works for the rest of this session.
pub fn save(snapshot: impl FnOnce() -> Vec<HistoryEntry>) -> Result<(), String> {
    save_to(&path(), snapshot)
}

/// `snapshot` runs under [`FILE_LOCK`], so the save that lands last also took
/// the newest snapshot: one that snapshotted earlier can never overwrite it.
pub(crate) fn save_to(
    path: &Path,
    snapshot: impl FnOnce() -> Vec<HistoryEntry>,
) -> Result<(), String> {
    let _file = FILE_LOCK.lock();
    let entries = snapshot();
    let file = HistoryFile {
        version: FORMAT_VERSION,
        entries: entries
            .iter()
            .map(|e| FileEntry {
                text: e.text.clone(),
                when_ms: e
                    .when
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            })
            .collect(),
    };
    write_json_atomically(path, &file)
}

/// Write `value` as JSON to a `.tmp` beside `path`, flush it, then rename it
/// over `path`, so a crash mid-write leaves the previous file intact rather
/// than a truncated one. The temp name is only unique per process, so the
/// caller must serialize writers of the same `path`.
pub(crate) fn write_json_atomically(path: &Path, value: &impl Serialize) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(value)
        .map_err(|e| format!("could not serialize {}: {e}", path.display()))?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut out =
        fs::File::create(&tmp).map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    out.write_all(&json)
        .and_then(|()| out.sync_all())
        .map_err(|e| format!("could not flush {}: {e}", tmp.display()))?;
    drop(out);
    // `rename` replaces an existing target on Windows (MOVEFILE_REPLACE_EXISTING
    // under the hood), so the old file is swapped out in one step.
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("could not save {}: {e}", path.display())
    })
}

/// Delete the file, if there is one. Called when `persist_history` is turned
/// off, so the preference and the disk agree. Not an error if it was already
/// gone.
pub fn remove() {
    remove_at(&path());
}

pub(crate) fn remove_at(path: &Path) {
    let _file = FILE_LOCK.lock();
    match fs::remove_file(path) {
        Ok(()) => tracing::info!("history: removed {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("history: could not remove {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qd-history-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        dir.join(HISTORY_FILE)
    }

    fn entry(id: u64, text: &str, secs: u64) -> HistoryEntry {
        HistoryEntry {
            id,
            text: text.to_string(),
            when: UNIX_EPOCH + Duration::from_secs(secs),
        }
    }

    #[test]
    fn round_trips_newest_first_with_timestamps() {
        let path = scratch("roundtrip");
        save_to(&path, || {
            vec![entry(2, "second", 200), entry(1, "first", 100)]
        })
        .unwrap();
        let back = load_from(&path);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].0, "second");
        assert_eq!(back[0].1, UNIX_EPOCH + Duration::from_secs(200));
        assert_eq!(back[1].0, "first");
        // The tmp file must not be left behind.
        let parent = path.parent().unwrap();
        let leftovers: Vec<_> = fs::read_dir(parent)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp file left behind: {leftovers:?}");
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn a_second_save_replaces_the_first() {
        let path = scratch("replace");
        save_to(&path, || vec![entry(1, "old", 1)]).unwrap();
        save_to(&path, || vec![entry(2, "new", 2), entry(1, "old", 1)]).unwrap();
        let back = load_from(&path);
        assert_eq!(
            back.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            ["new", "old"]
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// Regression: saves from different threads shared one `.tmp` and raced.
    /// A save that snapshots while another is still mid-save must land after
    /// it, so the newer list is what stays on disk.
    #[test]
    fn a_later_snapshot_is_never_overwritten_by_an_earlier_one() {
        let path = scratch("ordering");
        let (inside_tx, inside_rx) = std::sync::mpsc::channel();
        let slow = {
            let path = path.clone();
            std::thread::spawn(move || {
                save_to(&path, || {
                    inside_tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(100));
                    vec![entry(1, "older", 1)]
                })
            })
        };
        inside_rx.recv().unwrap();
        save_to(&path, || vec![entry(2, "newer", 2), entry(1, "older", 1)]).unwrap();
        slow.join().unwrap().unwrap();
        assert_eq!(load_from(&path)[0].0, "newer");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// Regression: concurrent saves truncated each other's temp file, and the
    /// losing rename failed with NotFound. Every save must succeed and leave a
    /// whole, parseable file.
    #[test]
    fn concurrent_saves_all_succeed_and_leave_a_whole_file() {
        let path = scratch("concurrent");
        let savers: Vec<_> = (0..8u64)
            .map(|id| {
                let path = path.clone();
                std::thread::spawn(move || {
                    (0..5)
                        .map(|_| {
                            save_to(&path, || vec![entry(id, &"x".repeat(id as usize + 1), id)])
                        })
                        .collect::<Result<Vec<()>, String>>()
                })
            })
            .collect();
        for saver in savers {
            saver.join().unwrap().unwrap();
        }
        let back = load_from(&path);
        assert_eq!(back.len(), 1);
        assert!(back[0].0.chars().all(|c| c == 'x'));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_file_is_empty_not_an_error() {
        let path = scratch("missing");
        assert!(load_from(&path).is_empty());
    }

    #[test]
    fn garbage_and_future_formats_start_empty() {
        let path = scratch("garbage");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{not json").unwrap();
        assert!(load_from(&path).is_empty());
        fs::write(
            &path,
            format!(
                r#"{{"version":{},"entries":[{{"text":"x","when_ms":1}}]}}"#,
                FORMAT_VERSION + 1
            ),
        )
        .unwrap();
        assert!(load_from(&path).is_empty());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn empty_texts_are_dropped_on_load() {
        let path = scratch("empty-text");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"version":1,"entries":[{"text":"","when_ms":1},{"text":"kept"}]}"#,
        )
        .unwrap();
        let back = load_from(&path);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, "kept");
        assert_eq!(back[0].1, UNIX_EPOCH);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn remove_is_quiet_when_nothing_is_there() {
        let path = scratch("remove");
        remove_at(&path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        save_to(&path, || vec![entry(1, "x", 1)]).unwrap();
        remove_at(&path);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
