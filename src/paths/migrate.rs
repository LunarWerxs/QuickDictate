//! Moving QuickDictate's files when the data folder changes, and the marker
//! that records where they went last time.

use std::path::{Path, PathBuf};

use super::*;

/// NUL-terminated UTF-16, the shape every `PCWSTR` argument above wants. The
/// returned buffer must outlive the call that borrows its pointer.
pub(super) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Path of the marker recording the active data folder. `None` when Windows
/// gives us no `LOCALAPPDATA`, in which case the multi-hop migration simply
/// does not happen -- a missing convenience, never a failure.
fn active_dir_marker() -> Option<PathBuf> {
    app_data_dir().map(|d| d.join(ACTIVE_DIR_MARKER))
}

/// The data folder recorded by the previous run, if it still exists and is not
/// the one we are about to use anyway.
pub(super) fn previous_dir() -> Option<PathBuf> {
    let raw = std::fs::read_to_string(active_dir_marker()?).ok()?;
    let path = PathBuf::from(raw.trim());
    (!path.as_os_str().is_empty() && path.is_dir()).then_some(path)
}

/// Remember the folder in use, for the next run's migration sweep. Best-effort:
/// a failure here costs a future multi-hop migration, not this run.
pub(super) fn record_active_dir(dir: &Path) -> Vec<String> {
    let Some(marker) = active_dir_marker() else {
        return Vec::new();
    };
    // Skip the write when it would not change anything -- this runs on every
    // start, and the common case is the same folder as last time.
    if std::fs::read_to_string(&marker).is_ok_and(|c| Path::new(c.trim()) == dir) {
        return Vec::new();
    }
    if let Some(parent) = marker.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return vec![format!(
                "WARN: could not create {} to record the data folder: {e}",
                parent.display()
            )];
        }
    }
    match std::fs::write(&marker, dir.to_string_lossy().as_bytes()) {
        Ok(()) => Vec::new(),
        Err(e) => vec![format!(
            "WARN: could not record the active data folder in {}: {e}. Moving it again later \
             may leave files behind.",
            marker.display()
        )],
    }
}

/// Move QuickDictate's runtime files out of `source_dir` and into `dest`.
///
/// Best-effort and non-destructive by design: an entry that already exists at
/// the destination is LEFT ALONE on both sides (never merged, never
/// overwritten), because the destination copy is the one the app is about to
/// use and the source copy is the only remaining record of the old one. A
/// failure here must not stop QuickDictate from starting.
pub(super) fn migrate_into(source_dir: &Path, dest: &Path) -> Vec<String> {
    if source_dir == dest {
        return Vec::new();
    }

    let mut diags = Vec::new();
    for name in RELOCATABLE {
        let source = source_dir.join(name);
        if !source.exists() {
            continue;
        }
        let target = dest.join(name);
        if target.exists() {
            diags.push(format!(
                "WARN: left {} in place -- {} already exists.",
                source.display(),
                target.display()
            ));
            continue;
        }
        match move_entry(&source, &target) {
            Ok(()) => diags.push(format!(
                "INFO: moved {} to {}",
                source.display(),
                target.display()
            )),
            Err(e) => diags.push(format!(
                "WARN: could not move {} to {}: {e}",
                source.display(),
                target.display()
            )),
        }
    }
    diags
}

/// Move a file or directory, falling back to copy-then-delete when `rename`
/// refuses. `rename` cannot cross volumes on Windows (ERROR_NOT_SAME_DEVICE),
/// and moving from an exe on `D:\` to `%LOCALAPPDATA%` on `C:\` is the common
/// case, not the exotic one.
pub(super) fn move_entry(source: &Path, target: &Path) -> std::io::Result<()> {
    if std::fs::rename(source, target).is_ok() {
        return Ok(());
    }
    if source.is_dir() {
        copy_dir_all(source, target)?;
        std::fs::remove_dir_all(source)
    } else {
        std::fs::copy(source, target)?;
        std::fs::remove_file(source)
    }
}

pub(super) fn copy_dir_all(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
