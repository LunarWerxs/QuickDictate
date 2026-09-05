//! Diagnostics on disk: where the log files live, how they rotate, and how
//! tracing is brought up from the loaded settings.
//!
//! Nothing here may run before `paths::data_dir` is resolved -- the folder the
//! logs live in is recorded in settings.json and nowhere else.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::paths;

#[cfg(test)]
mod tests;

/// Enough headroom for bursts without letting verbose logging retain a large
/// amount of formatted text in memory. The appender is deliberately lossy:
/// diagnostics must never back-pressure microphone or UI work.
const LOG_QUEUE_LINE_LIMIT: usize = 4_096;
const LOGS_DIR_NAME: &str = "logs";
const MAIN_LOG_NAME: &str = "quickdictate.log";
const OLD_LOG_NAME: &str = "quickdictate.log.old";
const PANIC_LOG_NAME: &str = "quickdictate-panic.log";
/// Numbered backup generations kept alongside the active log file
/// (`quickdictate.log.1` .. `quickdictate.log.{MAX_LOG_GENERATIONS}`), oldest
/// last. Combined with the per-generation `max_bytes` cap this bounds total
/// on-disk log usage to roughly `(MAX_LOG_GENERATIONS + 1) * max_bytes`, even
/// for a session that stays open for days at a verbose log level.
const MAX_LOG_GENERATIONS: u32 = 4;
/// Root-level diagnostic files written by older releases are kept separate
/// from the active files. In particular, a future size rotation must never
/// consume the only migrated copy of an old diagnostic.
const LEGACY_LOG_MIGRATIONS: [(&str, &str); 3] = [
    (MAIN_LOG_NAME, "quickdictate.legacy.log"),
    (OLD_LOG_NAME, "quickdictate.legacy.log.old"),
    (PANIC_LOG_NAME, "quickdictate-panic.legacy.log"),
];

/// Directory containing QuickDictate diagnostics. Settings opens this folder
/// directly so the active, rotated, panic, and migrated logs are all visible.
///
/// Lives inside the configured data folder (see [`crate::paths`]), which
/// defaults to the folder holding settings.json exactly as it always did.
pub(crate) fn logs_dir() -> PathBuf {
    paths::data_dir().join(LOGS_DIR_NAME)
}

/// Path of the active application log. Kept alongside [`logs_dir`] so Settings
/// does not need to duplicate the filename.
pub(crate) fn main_log_path() -> PathBuf {
    logs_dir().join(MAIN_LOG_NAME)
}

/// Path of the dedicated panic log (see [`install_panic_hook`]). Kept
/// alongside [`main_log_path`] so `crate::error_report` does not need to
/// duplicate the filename.
pub(crate) fn panic_log_path() -> PathBuf {
    logs_dir().join(PANIC_LOG_NAME)
}

/// Pick a destination that cannot be touched by active-log rotation and does
/// not collide with an earlier migration. The first collision gets `.1`, then
/// `.2`, and so on; existing files are never removed or overwritten.
fn available_legacy_path(logs_dir: &Path, preferred_name: &str) -> PathBuf {
    let preferred = logs_dir.join(preferred_name);
    if !preferred.exists() {
        return preferred;
    }

    for suffix in 1u64.. {
        let candidate = logs_dir.join(format!("{preferred_name}.{suffix}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("the legacy-log suffix space is effectively unbounded")
}

/// Create the diagnostics folder and move root-level logs left by older
/// releases into collision-safe legacy names. This is best-effort: a locked
/// legacy file must not prevent QuickDictate from starting.
///
/// Returned messages are replayed after tracing is initialized so migration
/// failures remain discoverable even though this runs before the logger opens.
fn prepare_logs_dir_at(exe_dir: &Path, logs_dir: &Path) -> Vec<String> {
    let mut diagnostics = Vec::new();
    if let Err(e) = std::fs::create_dir_all(logs_dir) {
        diagnostics.push(format!(
            "WARN: could not create diagnostics folder {}: {e}",
            logs_dir.display()
        ));
        return diagnostics;
    }

    for (root_name, legacy_name) in LEGACY_LOG_MIGRATIONS {
        let source = exe_dir.join(root_name);
        if !source.is_file() {
            continue;
        }
        let destination = available_legacy_path(logs_dir, legacy_name);
        match std::fs::rename(&source, &destination) {
            Ok(()) => diagnostics.push(format!(
                "INFO: moved legacy diagnostic {} to {}",
                source.display(),
                destination.display()
            )),
            Err(e) => diagnostics.push(format!(
                "WARN: could not move legacy diagnostic {} to {}: {e}",
                source.display(),
                destination.display()
            )),
        }
    }
    diagnostics
}

pub(crate) fn prepare_logs_dir() -> Vec<String> {
    let logs_dir = logs_dir();
    // Older releases dropped loose `quickdictate*.log` files in the folder they
    // ran from. Sweep BOTH candidates: the data folder's own parent (the
    // historical case) and the exe folder (which is a different place once the
    // user relocates the data folder, and is where those legacy files are).
    let mut roots: Vec<PathBuf> = Vec::new();
    for root in [
        logs_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        paths::exe_dir(),
    ] {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }

    let mut diagnostics = Vec::new();
    for root in &roots {
        diagnostics.extend(prepare_logs_dir_at(root, &logs_dir));
        if !logs_dir.is_dir() {
            // The folder could not be created. A second sweep would only
            // repeat the identical warning.
            break;
        }
    }
    diagnostics
}

/// Install a panic hook that writes panic info to a dedicated unbuffered
/// file (and via tracing, if it still works). Without this, panics in any
/// background thread silently disappear under `windows_subsystem = "windows"`.
pub(crate) fn install_panic_hook(write_panic_file: bool) {
    let panic_path = logs_dir().join(PANIC_LOG_NAME);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".into());
        let payload = info
            .payload()
            .downcast_ref::<&'static str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("<non-string panic>");
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let backtrace = std::backtrace::Backtrace::force_capture();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // 1) Synchronous append to a dedicated panic file. This survives even
        //    if the tracing pipeline is mid-shutdown. Gated on the same
        //    opt-in as every other on-disk log (see the call site).
        use std::io::Write as _;
        if write_panic_file {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&panic_path)
            {
                let _ = writeln!(
                    f,
                    "[{now}] PANIC thread='{thread}' at {location}: {payload}\n{backtrace:?}"
                );
                let _ = f.flush();
            }
        }

        // 2) Also fire through tracing so it lands in the main log if possible.
        tracing::error!(
            target: "panic",
            "PANIC thread='{thread}' at {location}: {payload}\n{backtrace:?}"
        );
        default(info);
    }));
}

/// Initialize tracing. The file appender is only attached when logging is
/// enabled (either by `cfg.enable_logging = true` in settings.json, or by the
/// `QUICKDICTATE_LOG` env var, which also controls the level filter).
///
/// The stdout layer is always attached -- it's cheap and shows up in debug
/// builds with a console attached; under `windows_subsystem = "windows"` it
/// silently goes nowhere.
/// Single-active-file writer with `MAX_LOG_GENERATIONS` numbered backups.
/// Unlike a startup-only check, the per-write cap below keeps a long-running
/// process bounded too. It is owned by tracing-appender's one background
/// worker, so no extra synchronization is needed here.
struct SizeCappedLogWriter {
    file: Option<std::fs::File>,
    dir: PathBuf,
    max_bytes: u64,
    bytes_written: u64,
}

impl SizeCappedLogWriter {
    fn open(dir: &Path, max_log_mb: u64) -> io::Result<Self> {
        Self::open_with_max_bytes(dir, max_log_mb.saturating_mul(1024 * 1024))
    }

    fn open_with_max_bytes(dir: &Path, max_bytes: u64) -> io::Result<Self> {
        let path = Self::generation_path(dir, 0);

        // Preserve the previous startup behavior as well as rotating during
        // this run. Rotation is diagnostic-only and best-effort: if an old log
        // is locked, keep appending rather than preventing QuickDictate launch.
        if max_bytes != 0
            && std::fs::metadata(&path)
                .map(|meta| meta.len() > max_bytes)
                .unwrap_or(false)
        {
            let _ = Self::rotate_generations(dir, max_bytes);
        }

        let file = Self::open_file(&path)?;
        let bytes_written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self {
            file: Some(file),
            dir: dir.to_path_buf(),
            max_bytes,
            bytes_written,
        })
    }

    fn open_file(path: &Path) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    }

    /// Generation `0` is the active file (`quickdictate.log`); `1..=N` are
    /// the numbered backups (`quickdictate.log.1`, ..., oldest last).
    fn generation_path(dir: &Path, generation: u32) -> PathBuf {
        if generation == 0 {
            dir.join(MAIN_LOG_NAME)
        } else {
            dir.join(format!("{MAIN_LOG_NAME}.{generation}"))
        }
    }

    /// Shifts every numbered backup up by one slot (dropping whatever sat in
    /// the oldest slot), then moves the active file into slot 1. Returns an
    /// error only if the critical active-to-slot-1 rename fails; shifting the
    /// older numbered backups is best-effort so a single locked generation
    /// (antivirus, an open Explorer handle) cannot stop rotation entirely.
    fn rotate_generations(dir: &Path, max_bytes: u64) -> io::Result<()> {
        let oldest = Self::generation_path(dir, MAX_LOG_GENERATIONS);
        let _ = std::fs::remove_file(&oldest);

        for generation in (1..MAX_LOG_GENERATIONS).rev() {
            let from = Self::generation_path(dir, generation);
            let to = Self::generation_path(dir, generation + 1);
            let _ = std::fs::rename(&from, &to);
        }

        let active = Self::generation_path(dir, 0);
        let first_backup = Self::generation_path(dir, 1);
        let rotation_result = match std::fs::rename(&active, &first_backup) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };

        Self::enforce_total_bytes_bound(dir, max_bytes);
        rotation_result
    }

    /// Safety net for the case where a single write larger than `max_bytes`
    /// lands whole in one generation: the per-write check in `write` only
    /// rotates *before* a write that would push the active file over the
    /// cap (see its comment), so one oversized line is never split, and can
    /// leave that generation over `max_bytes`. This prunes the oldest
    /// numbered backups, if needed, until total disk usage is back under
    /// `(MAX_LOG_GENERATIONS + 1) * max_bytes`, so the bound holds even
    /// under that edge case.
    fn enforce_total_bytes_bound(dir: &Path, max_bytes: u64) {
        if max_bytes == 0 {
            return;
        }
        let total_budget = max_bytes.saturating_mul(u64::from(MAX_LOG_GENERATIONS) + 1);
        for generation in (1..=MAX_LOG_GENERATIONS).rev() {
            let total: u64 = (0..=MAX_LOG_GENERATIONS)
                .map(|g| {
                    std::fs::metadata(Self::generation_path(dir, g))
                        .map(|meta| meta.len())
                        .unwrap_or(0)
                })
                .sum();
            if total <= total_budget {
                break;
            }
            let _ = std::fs::remove_file(Self::generation_path(dir, generation));
        }
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            let _ = file.flush();
        }

        let rotation_result = Self::rotate_generations(&self.dir, self.max_bytes);

        // Always try to restore a usable writer. If rotation itself failed
        // (for example an antivirus briefly locked the file), disable further
        // attempts for this run so every subsequent log line does not repeat
        // filesystem work. The next launch gets another chance.
        let path = Self::generation_path(&self.dir, 0);
        let file = Self::open_file(&path)?;
        self.bytes_written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        self.file = Some(file);
        if rotation_result.is_err() {
            self.max_bytes = 0;
        }
        Ok(())
    }
}

impl Write for SizeCappedLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.max_bytes != 0
            && self.bytes_written != 0
            && self.bytes_written.saturating_add(buf.len() as u64) > self.max_bytes
        {
            self.rotate()?;
        }

        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file is not open"))?;
        let written = file.write(buf)?;
        self.bytes_written = self.bytes_written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().map_or(Ok(()), std::io::Write::flush)
    }
}

/// Turn `QUICKDICTATE_LOG` into a tracing filter.
///
/// The variable does double duty: merely SETTING it turns file logging on (see
/// `main`), and its VALUE is a `tracing` filter directive. Those two jobs
/// disagreed for the obvious value. `QUICKDICTATE_LOG=1` switched logging on
/// and then handed "1" to `EnvFilter`, which is not a level or a target, so the
/// filter matched nothing and the log file was created and stayed EMPTY: the
/// documented way to turn logging on produced no logging at all.
///
/// So the switch-like values are recognised as "just turn it on, at the default
/// level", and everything else is still passed through verbatim, which keeps
/// `QUICKDICTATE_LOG=info,quickdictate=debug` working for anyone who wants
/// per-partial detail. An unparseable directive falls back to `info` rather
/// than silencing the log, for the same reason.
pub(crate) fn log_filter(raw: Option<&str>) -> EnvFilter {
    const DEFAULT: &str = "info";
    let Some(value) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return EnvFilter::new(DEFAULT);
    };
    // "I want logs", spelled the handful of ways people actually spell it.
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "y"
    ) {
        return EnvFilter::new(DEFAULT);
    }
    EnvFilter::try_new(value).unwrap_or_else(|_| EnvFilter::new(DEFAULT))
}

pub(crate) fn init_logging(
    file_logging: bool,
    max_log_mb: u64,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = log_filter(std::env::var("QUICKDICTATE_LOG").ok().as_deref());

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_thread_names(true);

    if file_logging {
        let dir = logs_dir();
        let path = main_log_path();
        match SizeCappedLogWriter::open(&dir, max_log_mb) {
            Ok(file_appender) => {
                let (file_writer, guard) =
                    tracing_appender::non_blocking::NonBlockingBuilder::default()
                        .buffered_lines_limit(LOG_QUEUE_LINE_LIMIT)
                        .lossy(true)
                        .thread_name("qd-log-writer")
                        .finish(file_appender);
                let file_layer = tracing_subscriber::fmt::layer()
                    .with_target(false)
                    .with_thread_names(true)
                    .with_ansi(false)
                    .with_writer(file_writer);
                let _ = tracing_subscriber::registry()
                    .with(filter)
                    .with(stdout_layer)
                    .with(file_layer)
                    .try_init();
                tracing::info!(
                    "File logging enabled at {} ({} MiB cap, {} queued lines max)",
                    path.display(),
                    max_log_mb,
                    LOG_QUEUE_LINE_LIMIT,
                );
                Some(guard)
            }
            Err(e) => {
                let _ = tracing_subscriber::registry()
                    .with(filter)
                    .with(stdout_layer)
                    .try_init();
                tracing::warn!(
                    "File logging requested but {} could not be opened: {e}",
                    path.display()
                );
                None
            }
        }
    } else {
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(stdout_layer)
            .try_init();
        None
    }
}
