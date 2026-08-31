//! Tests for folder resolution, the writability check, and migration.

use super::resolve::resolve;
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
