//! Tests for log-file placement, rotation and the log-level filter.

use super::*;

/// `QUICKDICTATE_LOG=1` is how anyone would turn logging on, and it used to
/// produce an empty log file: setting the variable enabled file logging,
/// while its value went to `EnvFilter` as a directive, where "1" matches
/// nothing.
#[test]
fn switch_like_log_values_mean_the_default_level_not_silence() {
    for on in ["1", "true", "TRUE", "yes", "on", "y", " 1 "] {
        let filter = log_filter(Some(on)).to_string();
        assert!(
            filter.contains("info"),
            "QUICKDICTATE_LOG={on:?} produced the filter {filter:?}, which is not 'info' \
             and would leave the log file empty"
        );
    }
}

#[test]
fn a_real_directive_is_still_passed_through_verbatim() {
    // The documented power-user form has to keep working.
    let filter = log_filter(Some("info,quickdictate=debug")).to_string();
    assert!(filter.contains("quickdictate"), "{filter}");
    assert!(filter.contains("debug"), "{filter}");
}

#[test]
fn an_unset_or_broken_value_falls_back_to_info_rather_than_silence() {
    assert!(log_filter(None).to_string().contains("info"));
    assert!(log_filter(Some("")).to_string().contains("info"));
    assert!(log_filter(Some("   ")).to_string().contains("info"));
    // Garbage must not silence the log; that is the failure mode this whole
    // function exists to remove.
    assert!(log_filter(Some("=====")).to_string().contains("info"));
}

fn temp_log_test_dir(label: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "quickdictate-{label}-test-{}-{unique}",
        std::process::id()
    ))
}

#[test]
fn prepares_logs_folder_and_migrates_root_diagnostics() {
    let exe_dir = temp_log_test_dir("migration");
    let logs_dir = exe_dir.join(LOGS_DIR_NAME);
    std::fs::create_dir_all(&exe_dir).unwrap();
    std::fs::write(exe_dir.join(MAIN_LOG_NAME), b"current legacy").unwrap();
    std::fs::write(exe_dir.join(OLD_LOG_NAME), b"older legacy").unwrap();
    std::fs::write(exe_dir.join(PANIC_LOG_NAME), b"panic legacy").unwrap();

    let diagnostics = prepare_logs_dir_at(&exe_dir, &logs_dir);

    assert!(logs_dir.is_dir());
    assert_eq!(diagnostics.len(), LEGACY_LOG_MIGRATIONS.len());
    for (root_name, legacy_name) in LEGACY_LOG_MIGRATIONS {
        assert!(!exe_dir.join(root_name).exists());
        assert!(logs_dir.join(legacy_name).is_file());
    }
    assert_eq!(
        std::fs::read(logs_dir.join("quickdictate.legacy.log")).unwrap(),
        b"current legacy"
    );
    assert_eq!(
        std::fs::read(logs_dir.join("quickdictate.legacy.log.old")).unwrap(),
        b"older legacy"
    );
    assert_eq!(
        std::fs::read(logs_dir.join("quickdictate-panic.legacy.log")).unwrap(),
        b"panic legacy"
    );
    // Migrated files cannot be mistaken for either active rotation.
    assert!(!logs_dir.join(MAIN_LOG_NAME).exists());
    assert!(!logs_dir.join(OLD_LOG_NAME).exists());

    std::fs::remove_dir_all(exe_dir).unwrap();
}

#[test]
fn legacy_migration_never_overwrites_an_existing_destination() {
    let exe_dir = temp_log_test_dir("migration-collision");
    let logs_dir = exe_dir.join(LOGS_DIR_NAME);
    std::fs::create_dir_all(&logs_dir).unwrap();
    std::fs::write(exe_dir.join(MAIN_LOG_NAME), b"root legacy").unwrap();
    std::fs::write(logs_dir.join(MAIN_LOG_NAME), b"active").unwrap();
    std::fs::write(logs_dir.join("quickdictate.legacy.log"), b"first migration").unwrap();

    let diagnostics = prepare_logs_dir_at(&exe_dir, &logs_dir);

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        std::fs::read(logs_dir.join(MAIN_LOG_NAME)).unwrap(),
        b"active"
    );
    assert_eq!(
        std::fs::read(logs_dir.join("quickdictate.legacy.log")).unwrap(),
        b"first migration"
    );
    assert_eq!(
        std::fs::read(logs_dir.join("quickdictate.legacy.log.1")).unwrap(),
        b"root legacy"
    );
    assert!(!exe_dir.join(MAIN_LOG_NAME).exists());

    // With no root-level file left, a second startup is a no-op.
    assert!(prepare_logs_dir_at(&exe_dir, &logs_dir).is_empty());
    assert!(!logs_dir.join("quickdictate.legacy.log.2").exists());

    std::fs::remove_dir_all(exe_dir).unwrap();
}

#[test]
fn log_writer_rotates_into_generation_one() {
    let dir = temp_log_test_dir("rotate-basic");
    std::fs::create_dir_all(&dir).unwrap();

    let mut writer = SizeCappedLogWriter::open_with_max_bytes(&dir, 10).unwrap();
    writer.write_all(b"12345678").unwrap();
    writer.write_all(b"abcd").unwrap();
    writer.flush().unwrap();
    drop(writer);

    assert_eq!(
        std::fs::read(dir.join("quickdictate.log.1")).unwrap(),
        b"12345678"
    );
    assert_eq!(
        std::fs::read(dir.join("quickdictate.log")).unwrap(),
        b"abcd"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn log_writer_creates_successive_numbered_generations() {
    let dir = temp_log_test_dir("rotate-succession");
    std::fs::create_dir_all(&dir).unwrap();

    // Each write is exactly at the cap, so every write after the first
    // forces exactly one rotation, walking a fresh letter down through
    // the numbered backups one slot per write.
    let mut writer = SizeCappedLogWriter::open_with_max_bytes(&dir, 8).unwrap();
    for chunk in [
        b"AAAAAAAA",
        b"BBBBBBBB",
        b"CCCCCCCC",
        b"DDDDDDDD",
        b"EEEEEEEE",
    ] {
        writer.write_all(chunk).unwrap();
    }
    writer.flush().unwrap();
    drop(writer);

    assert_eq!(
        std::fs::read(dir.join("quickdictate.log")).unwrap(),
        b"EEEEEEEE"
    );
    assert_eq!(
        std::fs::read(dir.join("quickdictate.log.1")).unwrap(),
        b"DDDDDDDD"
    );
    assert_eq!(
        std::fs::read(dir.join("quickdictate.log.2")).unwrap(),
        b"CCCCCCCC"
    );
    assert_eq!(
        std::fs::read(dir.join("quickdictate.log.3")).unwrap(),
        b"BBBBBBBB"
    );
    assert_eq!(
        std::fs::read(dir.join("quickdictate.log.4")).unwrap(),
        b"AAAAAAAA"
    );
    assert!(!dir.join("quickdictate.log.5").exists());

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn log_writer_prunes_oldest_generation_once_count_exceeds_max() {
    let dir = temp_log_test_dir("rotate-prune");
    std::fs::create_dir_all(&dir).unwrap();

    // One more write than the previous test: MAX_LOG_GENERATIONS backup
    // slots are already full, so this rotation must drop the oldest
    // ("AAAAAAAA") entirely rather than growing a 5th numbered file.
    let mut writer = SizeCappedLogWriter::open_with_max_bytes(&dir, 8).unwrap();
    for chunk in [
        b"AAAAAAAA",
        b"BBBBBBBB",
        b"CCCCCCCC",
        b"DDDDDDDD",
        b"EEEEEEEE",
        b"FFFFFFFF",
    ] {
        writer.write_all(chunk).unwrap();
    }
    writer.flush().unwrap();
    drop(writer);

    assert_eq!(
        std::fs::read(dir.join("quickdictate.log")).unwrap(),
        b"FFFFFFFF"
    );
    assert_eq!(
        std::fs::read(dir.join("quickdictate.log.1")).unwrap(),
        b"EEEEEEEE"
    );
    assert_eq!(
        std::fs::read(dir.join("quickdictate.log.2")).unwrap(),
        b"DDDDDDDD"
    );
    assert_eq!(
        std::fs::read(dir.join("quickdictate.log.3")).unwrap(),
        b"CCCCCCCC"
    );
    assert_eq!(
        std::fs::read(dir.join("quickdictate.log.4")).unwrap(),
        b"BBBBBBBB"
    );
    assert!(
        !dir.join("quickdictate.log.5").exists(),
        "must not grow a generation beyond MAX_LOG_GENERATIONS"
    );

    // Never on disk anywhere: pruned, not just unreferenced.
    for entry in std::fs::read_dir(&dir).unwrap() {
        let content = std::fs::read(entry.unwrap().path()).unwrap();
        assert_ne!(content, b"AAAAAAAA");
    }

    let total: u64 = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    assert!(total <= 8 * (MAX_LOG_GENERATIONS as u64 + 1));

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rotate_generations_keeps_total_bytes_bounded_after_an_oversized_write() {
    let dir = temp_log_test_dir("rotate-bytes-bound");
    std::fs::create_dir_all(&dir).unwrap();
    let max_bytes: u64 = 8;

    // Simulate a single tracing event larger than the cap (the per-write
    // check in `write` cannot split or reject one oversized buffer; see
    // its comment), landing whole in the active file, plus a history of
    // normal-sized backups already at the cap.
    std::fs::write(dir.join(MAIN_LOG_NAME), vec![b'X'; 20]).unwrap();
    std::fs::write(dir.join("quickdictate.log.1"), b"AAAAAAAA").unwrap();
    std::fs::write(dir.join("quickdictate.log.2"), b"BBBBBBBB").unwrap();
    std::fs::write(dir.join("quickdictate.log.3"), b"CCCCCCCC").unwrap();

    SizeCappedLogWriter::rotate_generations(&dir, max_bytes).unwrap();

    // The oversized generation is kept (rotation never truncates a log
    // line), but the safety net prunes enough of the oldest survivors
    // that the total stays within (MAX_LOG_GENERATIONS + 1) * max_bytes.
    assert_eq!(
        std::fs::read(dir.join("quickdictate.log.1")).unwrap().len(),
        20
    );
    let total: u64 = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    assert!(total <= max_bytes * (MAX_LOG_GENERATIONS as u64 + 1));

    std::fs::remove_dir_all(dir).unwrap();
}
