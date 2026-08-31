//! Deciding where the data folder is, and answering questions about it.
//!
//! The resolution order and its fallbacks, the one-time `init`, the
//! accessors every other module asks, and the writability and
//! is-this-a-sensible-folder checks the Settings picker shows.

use std::path::{Path, PathBuf};

use super::*;

/// Resolve the data folder without touching the process-wide cache. Pure apart
/// from reading the environment, so it is directly testable.
pub(super) fn resolve(configured: &str, default_dir: &Path) -> (PathBuf, Vec<String>) {
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
