//! The ONE place that decides where QuickDictate writes its files.
//!
//! QuickDictate ships as a single portable `.exe`, and until now every runtime
//! file it produced landed next to that exe: the `logs\` folder, the stats
//! json, the settings-sync credential blob, the update-check cache, and the dev
//! trigger's port file. That is fine when the exe lives in its own folder and
//! actively hostile when it does not -- an exe on the Desktop turns the Desktop
//! into QuickDictate's scratch directory.
//!
//! So the location is now a setting. [`init`] resolves it once at startup and
//! every other module asks [`data_dir`] instead of re-deriving the exe folder.
//!
//! Resolution order (first match wins):
//!   1. the `QUICKDICTATE_DATA_DIR` environment variable -- an escape hatch for
//!      tests, CI, and anyone scripting a portable install, and the only way to
//!      relocate the folder without a readable settings.json;
//!   2. `data_dir` in settings.json, with `%VARS%` expanded;
//!   3. the folder holding settings.json -- the historical behaviour. For a
//!      shipped exe that folder IS the exe folder, so an existing install that
//!      never touches the setting is byte-for-byte unchanged; for a dev run out
//!      of `target\debug\` it is the working tree, which is where the stats file
//!      already went.
//!
//! `settings.json` itself is deliberately NOT relocated by the setting: it has
//! to be found *before* anything in it can be read. [`Config::settings_path`]
//! owns that search and knows about [`app_data_dir`] as a fixed, well-known
//! location, which is what makes a completely empty exe folder possible.
//!
//! [`Config::settings_path`]: crate::config::Config::settings_path

mod migrate;
mod resolve;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub(crate) use resolve::{
    check_writable, data_dir, data_file, default_dir, folder_caution, init, pick_folder,
};

use migrate::*;

/// Environment override for the data folder. Wins over settings.json because
/// it is the only lever available when settings.json is not where you expect.
pub(crate) const DATA_DIR_ENV: &str = "QUICKDICTATE_DATA_DIR";

/// Folder name used under `%LOCALAPPDATA%`. Matches the folder `local_stt`
/// already creates for downloaded speech packs, so the "use AppData" preset
/// gathers everything QuickDictate owns under one root instead of two.
pub(crate) const APP_FOLDER_NAME: &str = "QuickDictate";

/// Records the data folder currently in use, so the NEXT start knows where the
/// files were and can move them along.
///
/// Without it the migration can only sweep the places it can derive -- the exe
/// folder and the settings folder -- which is enough for the first move and
/// wrong for every one after it: a user who goes default -> A -> B would have
/// their stats and sync credentials stranded in A while B starts empty. The UI
/// promises "QuickDictate moves the existing files across for you", so that has
/// to hold for the second move too.
///
/// It lives under `%LOCALAPPDATA%` rather than beside the exe on purpose. This
/// is app state, not user config, and the whole point of the feature is that
/// the exe's folder can be left alone -- dropping a bookkeeping file on
/// somebody's Desktop to support a feature about not cluttering their Desktop
/// would be self-defeating.
const ACTIVE_DIR_MARKER: &str = "active-data-dir.txt";

/// Every entry that lives in the data folder, as it is named on disk. Used by
/// the migration pass; a new runtime file must be added here or it will be left
/// behind in the old folder when the user relocates.
///
/// `logs` is a directory and is handled as one; the rest are plain files.
pub(crate) const RELOCATABLE: [&str; 5] = [
    "logs",
    "quickdictate-stats.json",
    "quickdictate-connections.dat",
    "quickdictate-update.txt",
    "quickdictate-dev-port.txt",
];

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
/// The folder the data would live in with no setting and no env override, i.e.
/// what "Default" means on this machine. Recorded by [`init`] so the Settings
/// UI can name it without re-running `Config::settings_path`, which stats the
/// filesystem up to eight times and would otherwise do so on every repaint.
static DEFAULT_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Directory holding the running executable, or `.` if it cannot be
/// determined. This is the historical data folder and remains the default.
pub(crate) fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `%LOCALAPPDATA%\QuickDictate`, the recommended off-Desktop location.
/// `None` only if Windows did not give us `LOCALAPPDATA`, which in practice
/// means a stripped service account.
pub(crate) fn app_data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join(APP_FOLDER_NAME))
}

/// Expand `%VAR%` references in a user-typed path and trim surrounding
/// whitespace/quotes. An unset variable makes the whole path unusable
/// (`None`) rather than silently collapsing to a relative path -- writing the
/// stats file into whatever the current directory happens to be is a worse
/// outcome than falling back to the exe folder with a diagnostic.
pub(crate) fn expand(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim().trim_matches('"');
    if trimmed.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(trimmed.len());
    let mut rest = trimmed;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            // `%%` is an empty name: emit a literal `%` and carry on.
            Some(0) => {
                out.push('%');
                rest = &after[1..];
            }
            Some(end) => {
                // An unset variable makes the WHOLE path unusable: silently
                // dropping it would splice together a path that points
                // somewhere real and wrong.
                let value = std::env::var_os(&after[..end])?;
                out.push_str(&value.to_string_lossy());
                rest = &after[end + 1..];
            }
            // Unpaired `%`: treat the remainder as literal text.
            None => {
                out.push('%');
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);

    let path = PathBuf::from(out.trim());
    // A relative data folder resolves against the process working directory,
    // which for a tray app launched from Explorer or the Run key is not a
    // place the user chose. Refuse it instead of scattering files somewhere
    // unpredictable.
    if path.as_os_str().is_empty() || path.is_relative() {
        return None;
    }
    Some(path)
}
