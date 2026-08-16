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

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

/// Resolve the data folder without touching the process-wide cache. Pure apart
/// from reading the environment, so it is directly testable.
fn resolve(configured: &str, default_dir: &Path) -> (PathBuf, Vec<String>) {
    let mut diags = Vec::new();

    if let Some(raw) = std::env::var(DATA_DIR_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        match expand(&raw) {
            Some(dir) => return (dir, diags),
            None => diags.push(format!(
                "WARN: {DATA_DIR_ENV}='{raw}' is not a usable absolute path; ignoring it."
            )),
        }
    }

    if !configured.trim().is_empty() {
        match expand(configured) {
            Some(dir) => return (dir, diags),
            None => diags.push(format!(
                "WARN: the configured data folder '{}' is not a usable absolute path \
                 (unset %VARIABLE% or a relative path); keeping files next to the app.",
                configured.trim()
            )),
        }
    }

    (default_dir.to_path_buf(), diags)
}

/// Resolve the data folder, create it, migrate anything left in the old one,
/// and lock the answer in for the rest of the process.
///
/// `default_dir` is where the files go when neither the environment nor the
/// setting names a folder -- `main` passes the directory holding settings.json,
/// which is the exe folder for every shipped install.
///
/// Call this from `main` immediately after the config is loaded and BEFORE the
/// logger opens a file -- [`data_dir`] answers with the pre-init default
/// (env var, else exe folder) until this runs, so a caller that beats it would
/// silently write to the old location.
///
/// Returns diagnostics in the `LEVEL: message` shape `main` replays through
/// tracing once the logger is up; this runs before logging exists.
pub(crate) fn init(configured: &str, default_dir: &Path) -> Vec<String> {
    let _ = DEFAULT_DIR.set(default_dir.to_path_buf());
    let (dir, mut diags) = resolve(configured, default_dir);

    // A configured folder we cannot create is a dead end: fall back rather than
    // failing every subsequent write one at a time.
    let dir = match std::fs::create_dir_all(&dir) {
        Ok(()) => dir,
        Err(e) => {
            let fallback = default_dir.to_path_buf();
            if dir == fallback {
                // The default folder itself is unwritable (read-only media, a
                // locked-down Program Files). Nothing better to fall back to;
                // the individual writes will report their own failures.
                diags.push(format!(
                    "WARN: could not create the data folder {}: {e}",
                    dir.display()
                ));
                dir
            } else {
                diags.push(format!(
                    "ALERT: could not create the data folder {} ({e}). Falling back to {} \
                     -- pick a different folder in Settings to move QuickDictate's files \
                     off it.",
                    dir.display(),
                    fallback.display()
                ));
                fallback
            }
        }
    };

    // Sweep every place the files could be sitting:
    //   * the folder recorded on the LAST run -- the only one that survives a
    //     second relocation (default -> A -> B strands everything in A without
    //     it), and the reason the marker file exists at all;
    //   * the exe folder, where a shipped install put everything;
    //   * the settings folder, which a dev run resolves elsewhere.
    // The last two are the same path for a normal install, hence the dedup.
    let mut sources: Vec<PathBuf> = Vec::new();
    for source in [
        previous_dir(),
        Some(exe_dir()),
        Some(default_dir.to_path_buf()),
    ]
    .into_iter()
    .flatten()
    {
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    for source in &sources {
        diags.extend(migrate_into(source, &dir));
    }

    diags.extend(record_active_dir(&dir));

    // `set` only fails if something already initialized it, which would mean a
    // second `init` call. Keep the first answer: re-pointing the data folder
    // mid-run would leave half the app writing to each location.
    if DATA_DIR.set(dir.clone()).is_err() {
        diags.push(
            "WARN: the data folder was already initialized; ignoring the second attempt."
                .to_string(),
        );
    }
    diags
}

/// The resolved data folder.
///
/// Before [`init`] runs this answers with the pre-init default -- the
/// environment override if set, otherwise the exe folder -- so an early caller
/// gets a sane path instead of a panic. It deliberately does NOT cache that
/// answer, so `init` can still install the configured folder afterwards.
pub(crate) fn data_dir() -> PathBuf {
    if let Some(dir) = DATA_DIR.get() {
        return dir.clone();
    }
    resolve("", &exe_dir()).0
}

/// A file inside the data folder.
pub(crate) fn data_file(name: &str) -> PathBuf {
    data_dir().join(name)
}

/// Where the files would go with `data_dir` left empty. Settings shows this as
/// the field's placeholder, so it must name the folder that would REALLY be
/// used, not merely the exe folder (a dev run resolves settings.json, and
/// therefore the default, to the working tree).
pub(crate) fn default_dir() -> PathBuf {
    DEFAULT_DIR.get().cloned().unwrap_or_else(exe_dir)
}

/// Best-effort writability probe for a folder the user just picked, so Settings
/// can refuse an unwritable choice up front instead of silently losing the next
/// stats flush. Creates the folder if needed.
pub(crate) fn check_writable(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let probe = dir.join(".quickdictate-write-test");
    std::fs::write(&probe, b"ok").map_err(|e| format!("cannot write to {}: {e}", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// A warning about a folder the user just picked, or `None` if it looks fine.
///
/// The one that matters is a pre-existing `logs\`. QuickDictate writes its
/// diagnostics into `<data folder>\logs`, so pointing this at a folder that
/// already has one means sharing a directory with whatever made it: the
/// migration correctly refuses to merge, and QuickDictate then writes its logs
/// in among somebody else's anyway. That is a surprise worth naming BEFORE the
/// save rather than discovering later, and it is only a warning because the
/// user picked the folder and may well have meant it.
pub(crate) fn folder_caution(dir: &Path) -> Option<String> {
    // Not a caution: this is our own folder, from a previous run.
    let ours = RELOCATABLE
        .iter()
        .skip(1) // "logs" is the thing being judged, so it cannot be the evidence
        .any(|name| dir.join(name).exists());
    if ours {
        return None;
    }
    if dir.join("logs").is_dir() {
        return Some(format!(
            "{} already contains a \"logs\" folder that isn't QuickDictate's. \
             QuickDictate will write its own diagnostics in there too. A folder of its \
             own is tidier.",
            dir.display()
        ));
    }
    None
}

/// Show the standard Windows folder picker and return what the user chose.
///
/// `None` means they cancelled, or the shell refused -- both are "carry on with
/// the current folder", never an error to report. Runs modally on the calling
/// thread, which is the Settings window's own thread: the picker pumps its own
/// message loop, so Settings stops repainting until it closes, exactly like
/// every other app's Browse button.
pub(crate) fn pick_folder(initial: Option<&Path>) -> Option<PathBuf> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_PICKFOLDERS,
        SIGDN_FILESYSPATH,
    };

    // The shell dialog needs an initialized apartment. This thread may already
    // have one (eframe/winit initializes COM for drag-and-drop), in which case
    // the call returns RPC_E_CHANGED_MODE or S_FALSE -- neither is a reason to
    // give up, so the result is deliberately ignored and any real failure
    // surfaces from CoCreateInstance below.
    //
    // SAFETY: FFI call with no out-parameters; a redundant initialize on an
    // already-initialized thread is defined behaviour (it just refcounts).
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    // SAFETY: every call below is a COM method on an interface pointer the
    // runtime just handed us, with arguments of the documented types. Each `?`
    // bails before the next call, so no method ever runs on a failed object.
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS).ok()?;
        dialog
            .SetTitle(PCWSTR(
                wide("Choose where QuickDictate keeps its files").as_ptr(),
            ))
            .ok()?;

        // Start in the folder currently in use. Best-effort: a path that no
        // longer exists just means the picker opens wherever it likes.
        if let Some(dir) = initial.filter(|d| d.exists()) {
            let wide_dir = wide(&dir.to_string_lossy());
            if let Ok(item) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide_dir.as_ptr()), None)
            {
                let _ = dialog.SetFolder(&item);
            }
        }

        // A cancelled dialog returns HRESULT_FROM_WIN32(ERROR_CANCELLED), which
        // is indistinguishable here from a real failure and needs the same
        // handling anyway: keep the folder we have.
        dialog.Show(None).ok()?;

        let item = dialog.GetResult().ok()?;
        let raw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let picked = raw.to_string().ok().map(PathBuf::from);
        // GetDisplayName allocates with CoTaskMemAlloc; the caller frees it.
        CoTaskMemFree(Some(raw.0 as *const std::ffi::c_void));
        picked
    }
}

/// NUL-terminated UTF-16, the shape every `PCWSTR` argument above wants. The
/// returned buffer must outlive the call that borrows its pointer.
fn wide(s: &str) -> Vec<u16> {
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
fn previous_dir() -> Option<PathBuf> {
    let raw = std::fs::read_to_string(active_dir_marker()?).ok()?;
    let path = PathBuf::from(raw.trim());
    (!path.as_os_str().is_empty() && path.is_dir()).then_some(path)
}

/// Remember the folder in use, for the next run's migration sweep. Best-effort:
/// a failure here costs a future multi-hop migration, not this run.
fn record_active_dir(dir: &Path) -> Vec<String> {
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
fn migrate_into(source_dir: &Path, dest: &Path) -> Vec<String> {
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
fn move_entry(source: &Path, target: &Path) -> std::io::Result<()> {
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

fn copy_dir_all(source: &Path, target: &Path) -> std::io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `expand` and `resolve` read process-wide environment state, so the tests
    /// that mutate it must not run concurrently with each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_dir(name: &str) -> PathBuf {
        // Process-id suffixed: several `cargo test` processes (a mutants run,
        // a parallel session) can be live at once and must not share scratch.
        std::env::temp_dir().join(format!(
            "quickdictate-paths-{}-{}-{:?}",
            name,
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn expand_resolves_environment_variables() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("QD_TEST_ROOT", "C:\\qd-root");
        assert_eq!(
            expand("%QD_TEST_ROOT%\\data"),
            Some(PathBuf::from("C:\\qd-root\\data"))
        );
        std::env::remove_var("QD_TEST_ROOT");
    }

    #[test]
    fn expand_rejects_unset_variables_rather_than_guessing() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("QD_TEST_DEFINITELY_UNSET");
        assert_eq!(expand("%QD_TEST_DEFINITELY_UNSET%\\data"), None);
    }

    #[test]
    fn expand_rejects_relative_and_empty_paths() {
        // A relative path would resolve against the working directory, which a
        // tray app launched from the Run key does not control.
        assert_eq!(expand("data"), None);
        assert_eq!(expand("..\\elsewhere"), None);
        assert_eq!(expand("   "), None);
        assert_eq!(expand(""), None);
    }

    #[test]
    fn expand_strips_quotes_a_paste_from_explorer_carries() {
        assert_eq!(
            expand("\"C:\\Users\\me\\QuickDictate\""),
            Some(PathBuf::from("C:\\Users\\me\\QuickDictate"))
        );
    }

    #[test]
    fn expand_keeps_a_literal_percent_pair() {
        assert_eq!(
            expand("C:\\100%%\\data"),
            Some(PathBuf::from("C:\\100%\\data"))
        );
    }

    #[test]
    fn resolve_prefers_the_environment_over_the_setting() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(DATA_DIR_ENV, "C:\\from-env");
        let (dir, diags) = resolve("C:\\from-settings", Path::new("C:\\default"));
        std::env::remove_var(DATA_DIR_ENV);
        assert_eq!(dir, PathBuf::from("C:\\from-env"));
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn resolve_falls_back_to_the_default_with_no_setting() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(DATA_DIR_ENV);
        let (dir, diags) = resolve("", Path::new("C:\\default"));
        assert_eq!(dir, PathBuf::from("C:\\default"));
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn resolve_reports_and_ignores_an_unusable_setting() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(DATA_DIR_ENV);
        let (dir, diags) = resolve("relative\\path", Path::new("C:\\default"));
        assert_eq!(
            dir,
            PathBuf::from("C:\\default"),
            "an unusable setting must not move anything"
        );
        assert!(
            diags.iter().any(|d| d.starts_with("WARN:")),
            "the user has to learn their setting was ignored: {diags:?}"
        );
    }

    /// The second relocation is the one that used to lose data. Moving
    /// default -> A migrated fine, because `init` can DERIVE the exe and
    /// settings folders. Moving A -> B could not: nothing recorded that A had
    /// ever been used, so B started empty while the real stats and sync
    /// credentials sat in A. Drives the same two functions `init` chains.
    #[test]
    fn a_second_relocation_still_finds_the_files() {
        let root = temp_dir("secondhop");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("quickdictate-stats.json"), b"lifetime totals").unwrap();
        std::fs::create_dir_all(a.join("logs")).unwrap();
        std::fs::write(a.join("logs\\quickdictate.log"), b"history").unwrap();

        // What `init` does with the folder the previous run recorded.
        let diags = migrate_into(&a, &b);

        assert!(
            diags.iter().all(|d| d.starts_with("INFO:")),
            "a clean second hop reports only INFO: {diags:?}"
        );
        assert_eq!(
            std::fs::read(b.join("quickdictate-stats.json")).unwrap(),
            b"lifetime totals",
            "the stats must follow the user to the new folder"
        );
        assert!(b.join("logs\\quickdictate.log").exists());
        assert!(!a.join("quickdictate-stats.json").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_active_dir_marker_round_trips_and_ignores_a_vanished_folder() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = temp_dir("marker");
        let fake_appdata = root.join("appdata");
        let recorded = root.join("recorded");
        std::fs::create_dir_all(&fake_appdata).unwrap();
        std::fs::create_dir_all(&recorded).unwrap();

        let old = std::env::var_os("LOCALAPPDATA");
        std::env::set_var("LOCALAPPDATA", &fake_appdata);

        assert!(record_active_dir(&recorded).is_empty());
        assert_eq!(previous_dir(), Some(recorded.clone()));

        // A folder the user deleted between runs must not be offered as a
        // migration source: `migrate_into` would just churn over nothing.
        std::fs::remove_dir_all(&recorded).unwrap();
        assert_eq!(previous_dir(), None);

        match old {
            Some(v) => std::env::set_var("LOCALAPPDATA", v),
            None => std::env::remove_var("LOCALAPPDATA"),
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn migrate_into_is_a_no_op_when_source_and_destination_match() {
        let dir = temp_dir("noop");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(migrate_into(&dir, &dir).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_into_never_overwrites_an_existing_destination() {
        let root = temp_dir("nooverwrite");
        let source = root.join("source");
        let dest = root.join("dest");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(source.join("quickdictate-stats.json"), b"old").unwrap();
        std::fs::write(dest.join("quickdictate-stats.json"), b"live").unwrap();

        let diags = migrate_into(&source, &dest);

        assert_eq!(
            std::fs::read(dest.join("quickdictate-stats.json")).unwrap(),
            b"live",
            "the destination copy is the one in use and must survive"
        );
        assert!(
            source.join("quickdictate-stats.json").exists(),
            "the source copy is the only record of the old data; do not delete it"
        );
        assert!(diags.iter().any(|d| d.starts_with("WARN:")), "{diags:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn migration_moves_files_and_the_logs_folder() {
        let root = temp_dir("migrate");
        let dest = root.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let source_dir = root.join("source");
        std::fs::create_dir_all(source_dir.join("logs")).unwrap();
        std::fs::write(source_dir.join("logs\\quickdictate.log"), b"log").unwrap();
        std::fs::write(source_dir.join("quickdictate-stats.json"), b"{}").unwrap();

        let diags = migrate_into(&source_dir, &dest);
        assert!(
            diags.iter().all(|d| d.starts_with("INFO:")),
            "a clean move reports only INFO: {diags:?}"
        );

        assert!(dest.join("logs\\quickdictate.log").exists());
        assert!(dest.join("quickdictate-stats.json").exists());
        assert!(!source_dir.join("logs").exists());
        assert!(!source_dir.join("quickdictate-stats.json").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `%LOCALAPPDATA%\QuickDictate` is what the "Use AppData" preset writes
    /// and where the active-dir marker lives, so getting it wrong silently
    /// moves both. `cargo mutants` caught that nothing asserted it.
    #[test]
    fn app_data_dir_is_localappdata_plus_the_app_folder() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("LOCALAPPDATA");

        std::env::set_var("LOCALAPPDATA", "C:\\Users\\someone\\AppData\\Local");
        assert_eq!(
            app_data_dir(),
            Some(PathBuf::from(
                "C:\\Users\\someone\\AppData\\Local\\QuickDictate"
            ))
        );

        // An empty variable is as useless as an absent one: joining onto it
        // would produce a bare relative "QuickDictate".
        std::env::set_var("LOCALAPPDATA", "");
        assert_eq!(app_data_dir(), None);

        std::env::remove_var("LOCALAPPDATA");
        assert_eq!(app_data_dir(), None);

        match old {
            Some(v) => std::env::set_var("LOCALAPPDATA", v),
            None => std::env::remove_var("LOCALAPPDATA"),
        }
    }

    #[test]
    fn data_file_and_default_dir_hang_off_the_resolved_roots() {
        assert_eq!(
            data_file("quickdictate-stats.json"),
            data_dir().join("quickdictate-stats.json")
        );
        assert!(data_file("x").is_absolute() || data_dir().is_relative());
        // Before `init`, the default is the exe folder (see the doc comment).
        assert_eq!(
            default_dir(),
            DEFAULT_DIR.get().cloned().unwrap_or_else(exe_dir)
        );
    }

    /// The cross-volume path: `rename` cannot move between drives on Windows,
    /// and moving from an exe on `D:\` to `%LOCALAPPDATA%` on `C:\` is the
    /// common case. The file-level fallback was covered; the DIRECTORY one was
    /// not, which `cargo mutants` proved by replacing `copy_dir_all`'s body
    /// with `Ok(())` and having every test still pass.
    #[test]
    fn copy_dir_all_reproduces_a_whole_tree() {
        let root = temp_dir("copydir");
        let source = root.join("logs");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("quickdictate.log"), b"active").unwrap();
        std::fs::write(source.join("quickdictate.log.1"), b"rotated").unwrap();
        std::fs::write(source.join("nested\\deep.txt"), b"deep").unwrap();

        let target = root.join("moved-logs");
        copy_dir_all(&source, &target).unwrap();

        assert_eq!(
            std::fs::read(target.join("quickdictate.log")).unwrap(),
            b"active"
        );
        assert_eq!(
            std::fs::read(target.join("quickdictate.log.1")).unwrap(),
            b"rotated"
        );
        assert_eq!(
            std::fs::read(target.join("nested\\deep.txt")).unwrap(),
            b"deep",
            "the recursion must reach nested directories, not just the top level"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn folder_caution_fires_only_for_somebody_elses_logs() {
        let root = temp_dir("caution");
        let empty = root.join("empty");
        let foreign = root.join("foreign");
        let ours = root.join("ours");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(foreign.join("logs")).unwrap();
        std::fs::create_dir_all(ours.join("logs")).unwrap();
        std::fs::write(ours.join("quickdictate-stats.json"), b"{}").unwrap();

        assert_eq!(folder_caution(&empty), None, "an empty folder is fine");
        assert!(
            folder_caution(&foreign).is_some(),
            "a pre-existing logs\\ that is not ours deserves a warning"
        );
        assert_eq!(
            folder_caution(&ours),
            None,
            "our OWN folder from a previous run must not warn"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn wide_is_nul_terminated_utf16() {
        assert_eq!(wide(""), vec![0u16]);
        assert_eq!(wide("Ok"), vec![b'O' as u16, b'k' as u16, 0]);
        let encoded = wide("C:\\folder");
        assert_eq!(*encoded.last().unwrap(), 0, "PCWSTR requires a NUL");
        assert_eq!(encoded.len(), "C:\\folder".chars().count() + 1);
    }

    #[test]
    fn move_entry_falls_back_to_copy_when_rename_is_refused() {
        let root = temp_dir("copyfallback");
        let source = root.join("a\\file.txt");
        let target = root.join("b\\file.txt");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&source, b"payload").unwrap();
        move_entry(&source, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"payload");
        assert!(!source.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn check_writable_accepts_a_fresh_folder_and_leaves_no_probe_behind() {
        let dir = temp_dir("writable");
        check_writable(&dir).unwrap();
        assert!(dir.exists());
        assert!(!dir.join(".quickdictate-write-test").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_relocatable_entry_is_named_once() {
        let mut seen: Vec<&str> = RELOCATABLE.to_vec();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "a data file is listed twice");
    }
}
