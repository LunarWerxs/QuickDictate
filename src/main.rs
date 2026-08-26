#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// A release build has no console (`windows_subsystem = "windows"` above), so a
// panic on a background thread writes to a stderr that goes nowhere: dictation
// just stops, with no error and nothing on screen. `.unwrap()`/`.expect()` are
// therefore SILENT failure here, not loud ones, and are linted crate-wide.
// Tests are exempt via clippy.toml (an unwrap there IS the assertion). A
// genuinely infallible site takes a local `#[allow(..., reason = "...")]`,
// where the reason string is the argument for why it cannot fire.
#![warn(clippy::unwrap_used, clippy::expect_used)]

mod about;
mod audio;
mod autostart;
mod config;
mod dev_trigger;
mod focus;
/// Mutation fuzzing of the untrusted-input parsers, wired in as ordinary tests
/// so it runs on every `cargo test` (and therefore in CI) without a named job.
#[cfg(test)]
mod fuzz;
mod hotkeys;
mod icon;
mod keys;
mod local_stt;
mod mouse_hook;
mod onboarding;
mod output;
mod paths;
mod polish;
mod secretstore;
mod settings_ui;
mod sound;
mod state;
mod stats;
mod stt;
mod sync;
mod text;
mod theme;
mod ui;
mod update;
mod voice_commands;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, LPARAM, WAIT_ABANDONED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject, INFINITE};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, PostMessageW, RegisterWindowMessageW, MB_ICONERROR, MB_ICONWARNING, MB_OK,
};

use crate::audio::AudioSource;
use crate::config::Config;
use crate::hotkeys::{HotkeyEvent, HotkeyManager};
use crate::keys::KeyPool;
use crate::state::{App, Status};
use crate::stt::SttHandle;

/// Name of the named mutex that guards against a second QuickDictate process.
/// Held for the whole process lifetime (see `main`) -- a second launch that
/// finds this already taken signals the running instance to reveal Settings
/// (see `single_instance_guard`) instead of starting a duplicate. Fixed,
/// process-wide name so it's stable across versions and install locations.
const SINGLE_INSTANCE_MUTEX_NAME: &str = "QuickDictate.SingleInstance";

/// How long a second launch retries `FindWindowW` for before giving up. Only
/// matters if the first instance is still mid-boot (overlay window not yet
/// created) when the second one is spawned.
const ACTIVATE_RETRY_ATTEMPTS: u32 = 10;
const ACTIVATE_RETRY_INTERVAL: Duration = Duration::from_millis(200);
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

fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Claims the single-instance named mutex. If another QuickDictate process
/// already holds it, asks that instance to reveal its Settings window (the
/// guaranteed way back in, including when the tray icon is hidden -- see
/// `Config::hide_tray_icon`) and returns `false`, meaning the caller must
/// exit immediately without touching audio, hotkeys, tray, or logging.
///
/// Exception: when this process was launched as a deliberate self-respawn --
/// the self-updater's relaunch (`--updated <tag>`) or Settings' "Save &
/// Restart" (`--relaunch`) -- it waits on the held mutex until the old process
/// completes its clean shutdown. This serial hand-off prevents two instances
/// from touching settings, stats, audio, or hotkeys at the same time.
///
/// On success (`true`), the mutex is held for the whole process lifetime with
/// no explicit cleanup needed: windows-rs's `HANDLE` is a bare `Copy` wrapper
/// around the raw handle value with no `Drop` impl, so it is never closed by
/// us -- Windows closes it (and releases the mutex) automatically when the
/// process exits, however it exits.
fn single_instance_guard() -> bool {
    let name = wide_z(SINGLE_INSTANCE_MUTEX_NAME);
    // SAFETY: FFI call with a valid, nul-terminated wide string and no
    // security attributes (default security descriptor).
    let handle = match unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) } {
        Ok(handle) => handle,
        Err(e) => {
            // Couldn't even ask the question -- fail open rather than block the
            // user from launching QuickDictate at all.
            tracing::warn!("single-instance: CreateMutexW failed: {e}; continuing anyway");
            return true;
        }
    };
    let already_running =
        unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS;
    if !already_running {
        // We own the mutex now; see doc comment above re: no cleanup needed.
        return true;
    }

    // A deliberate self-respawn — the self-updater's relaunch
    // (`update::relaunch` → `<exe> --updated <tag>`) or Settings' "Save &
    // Restart" (`<exe> --relaunch`) — is a serial hand-off: the other instance
    // is the old process, already latched to shut down. Wait until Windows
    // releases/abandons its owned mutex before loading any mutable app state.
    // This also makes the stats flush a true boundary: no late old-process
    // write can race a child that has already loaded an earlier snapshot.
    if std::env::args().any(|a| a == "--updated" || a == "--relaunch") {
        tracing::info!("single-instance: deliberate respawn waiting for old instance to exit");
        let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
        if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED {
            // INFINITE cannot time out. Fail open on the only remaining case
            // (WAIT_FAILED) so a rare OS error cannot make the app disappear
            // completely after the old process has already committed to exit.
            tracing::error!(
                "single-instance: respawn mutex wait failed ({wait:?}); continuing cautiously"
            );
        }
        tracing::info!("single-instance: respawn hand-off complete");
        return true;
    }

    // Another instance is already running. Find its overlay window (the one
    // always-alive top-level window QuickDictate owns) and ask it to reveal
    // Settings, exactly like the tray menu's "Settings…" item would.
    let class_name = wide_z(crate::ui::OVERLAY_CLASS_NAME);
    let msg_name = wide_z(crate::ui::ACTIVATE_MESSAGE_NAME);
    let msg_id = unsafe { RegisterWindowMessageW(PCWSTR(msg_name.as_ptr())) };

    for attempt in 0..ACTIVATE_RETRY_ATTEMPTS {
        let found = unsafe { FindWindowW(PCWSTR(class_name.as_ptr()), PCWSTR::null()) };
        if let Ok(hwnd) = found {
            if !hwnd.0.is_null() && msg_id != 0 {
                let post = unsafe { PostMessageW(hwnd, msg_id, WPARAM(0), LPARAM(0)) };
                if let Err(e) = post {
                    tracing::warn!("single-instance: PostMessageW failed: {e}");
                }
                return false;
            }
        }
        // First instance may still be mid-boot (overlay not created yet).
        if attempt + 1 < ACTIVATE_RETRY_ATTEMPTS {
            std::thread::sleep(ACTIVATE_RETRY_INTERVAL);
        }
    }
    tracing::warn!(
        "single-instance: another instance is running but its window was not found after {}ms; exiting anyway",
        ACTIVATE_RETRY_ATTEMPTS as u64 * ACTIVATE_RETRY_INTERVAL.as_millis() as u64
    );
    false
}

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

fn prepare_logs_dir() -> Vec<String> {
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
fn install_panic_hook(write_panic_file: bool) {
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
fn log_filter(raw: Option<&str>) -> EnvFilter {
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

fn init_logging(
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

fn refresh_key_pool(app: &Arc<App>, keys: &mut Arc<KeyPool>) {
    let cfg = app.config.load();
    if keys.matches_config(&cfg) {
        return;
    }
    tracing::info!(
        "provider or keys changed; rebuilding the '{}' key pool",
        cfg.stt_provider
    );
    *keys = KeyPool::new(&cfg);
    if cfg.prewarm_keys {
        stt::spawn_prewarm(Arc::clone(app), Arc::clone(keys));
    }
}

fn status_after_release(provider: &str) -> Status {
    if provider.eq_ignore_ascii_case("local") {
        Status::Processing
    } else {
        Status::Idle
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingStart {
    Toggle,
    Hold,
}

fn handle_processing_hotkey(pending: &mut Option<PendingStart>, event: HotkeyEvent) -> bool {
    match event {
        HotkeyEvent::TogglePressed => *pending = Some(PendingStart::Toggle),
        HotkeyEvent::HoldPressed => *pending = Some(PendingStart::Hold),
        HotkeyEvent::HoldReleased => {
            if *pending == Some(PendingStart::Hold) {
                *pending = None;
            }
        }
        HotkeyEvent::ToggleLongPressed => {
            *pending = None;
            return false;
        }
    }
    true
}

fn start_queued_session_if_idle(
    app: &Arc<App>,
    keys: &mut Arc<KeyPool>,
    active: &mut Option<SttHandle>,
    pending: &mut Option<PendingStart>,
) {
    if app.status() != Status::Idle {
        return;
    }
    let Some(kind) = pending.take() else {
        return;
    };
    let _ = active.take();
    refresh_key_pool(app, keys);
    tracing::info!("Starting queued {kind:?} session after local processing");
    app.set_status(Status::Starting);
    *active = Some(stt::start_session(Arc::clone(app), Arc::clone(keys)));
}

fn should_open_settings_on_start(is_settings_relaunch: bool, has_usable_key: bool) -> bool {
    is_settings_relaunch || !has_usable_key
}

fn main() -> Result<()> {
    // A side-effect-free canary for release CI and the self-updater. Handle it before the
    // single-instance mutex, settings, microphone, hotkeys, tray, or logging are initialized.
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--version" || arg == "version")
    {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // Single-instance guard: claims a named mutex before anything else
    // (settings.json load, logging, audio, hotkeys, tray). If another
    // QuickDictate is already running, this asks it to reveal Settings and
    // exits immediately -- no audio/hotkey/tray/logging side effects at all
    // for the second launch. This is also the guaranteed way back in when
    // `hide_tray_icon` has hidden the notification-area icon: launching the
    // exe again always reaches a running instance's Settings window.
    if !single_instance_guard() {
        std::process::exit(0);
    }

    // Load (and possibly generate) settings.json before initializing tracing,
    // because `enable_logging` is read out of the config. This now also has to
    // come before the diagnostics folder is prepared: `data_dir` decides where
    // that folder IS, and settings.json is the only place it is recorded.
    let (mut cfg, mut startup_diags) = Config::load_or_create();

    // Resolve the data folder and move anything left behind in the old one.
    // Everything past this line (logging, stats, sync credentials, the update
    // cache) resolves through `paths::data_dir`, so nothing may write to disk
    // before it runs.
    // `Path::parent` of a bare relative name like "settings.json" is `Some("")`,
    // NOT `None` -- so an `unwrap_or_else` alone would hand the empty path
    // through as if it were a real folder and every data file would land on a
    // relative path in whatever directory the process happened to start in.
    // Filter the empty case out explicitly.
    let settings_root = Config::settings_path()
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(paths::exe_dir);
    startup_diags.extend(paths::init(&cfg.data_dir, &settings_root));

    // Prepare the diagnostics folder before either logger can open a file.
    // Migration messages are replayed once tracing is initialized below.
    startup_diags.extend(prepare_logs_dir());

    // `--provider <id>` overrides settings.json's stt_provider for this run,
    // which is useful for local provider testing and automation.
    let args: Vec<String> = std::env::args().collect();
    let explicit_provider = args.iter().any(|a| a == "--provider");
    if let Some(i) = args.iter().position(|a| a == "--provider") {
        if let Some(p) = args.get(i + 1) {
            cfg.stt_provider = p.trim().to_ascii_lowercase();
        }
    }

    // Auto-default: if the user didn't force a provider and the configured one
    // has no keys, switch to whichever provider *does* have keys (so someone
    // who only pasted, e.g., Google keys opens straight into Google). An
    // explicit --provider is always respected.
    let mut auto_provider: Option<String> = None;
    if !explicit_provider {
        if let Some(p) = cfg.resolve_provider() {
            if p != cfg.stt_provider {
                auto_provider = Some(p.clone());
                cfg.stt_provider = p;
            }
        }
    }

    let file_logging = cfg.enable_logging || std::env::var_os("QUICKDICTATE_LOG").is_some();
    let _log_guard = init_logging(file_logging, cfg.max_log_mb);
    if explicit_provider {
        tracing::info!("provider override from command line: {}", cfg.stt_provider);
    }
    if let Some(p) = &auto_provider {
        tracing::info!(
            "configured provider had no keys; auto-selected '{p}' (the only provider with keys)"
        );
    }

    // Replay the config-loading diagnostics through tracing now that it's up.
    // "ALERT: " lines (a corrupt settings.json that was backed up and replaced
    // with defaults) also get a message box — with windows_subsystem="windows"
    // a log line alone is invisible, and the user must learn their keys/prefs
    // were sidelined. Shown from a worker thread so startup isn't blocked.
    for line in startup_diags {
        if let Some(rest) = line.strip_prefix("INFO: ") {
            tracing::info!("{rest}");
        } else if let Some(rest) = line.strip_prefix("WARN: ") {
            tracing::warn!("{rest}");
        } else if let Some(rest) = line.strip_prefix("ERROR: ") {
            tracing::error!("{rest}");
        } else if let Some(rest) = line.strip_prefix("ALERT: ") {
            tracing::error!("{rest}");
            let body = rest.to_string();
            std::thread::spawn(move || {
                update::msg_box(
                    "QuickDictate — settings problem",
                    &body,
                    MB_OK | MB_ICONWARNING,
                );
            });
        } else {
            tracing::info!("{line}");
        }
    }

    // The panic FILE honours the same opt-in as every other log. SECURITY.md
    // promises local logging is opt-in, and a panic hook that always writes to
    // disk quietly breaks that promise: the payload of a future panic near
    // key-handling code would land in a file next to settings.json regardless.
    // The tracing path inside the hook is always installed and stays silent
    // unless logging is on, so crashes are still diagnosable the moment the
    // user turns logging on and reproduces.
    install_panic_hook(cfg.enable_logging || std::env::var_os("QUICKDICTATE_LOG").is_some());
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    let cfg_arc = Arc::new(cfg);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("qd-tokio")
        .build()?;
    let rt_handle = rt.handle().clone();

    // Pre-warm the audio pipeline. The WASAPI stream stays open for the
    // app's lifetime so sessions never pay mic-initialization latency.
    // With windows_subsystem="windows" a bare `?` here would exit with no
    // visible trace of why, so a missing/broken microphone gets a message
    // box before we bail.
    // Publish the microphone preference BEFORE the source opens, so the very
    // first stream already lands on the right device.
    audio::set_preferred_input(&cfg_arc.input_device);
    let audio = match AudioSource::new() {
        Ok(a) => Arc::new(a),
        Err(e) => {
            tracing::error!("audio init failed: {e:#}");
            update::msg_box(
                "QuickDictate — no microphone",
                &format!(
                    "QuickDictate could not open a microphone and has to exit.\n\n\
                     {e:#}\n\n\
                     Plug in or enable a microphone (check Windows Sound settings \
                     and the microphone privacy toggle), then start QuickDictate again."
                ),
                MB_OK | MB_ICONERROR,
            );
            return Err(e);
        }
    };

    let app = App::new((*cfg_arc).clone(), rt_handle.clone(), Arc::clone(&audio));
    let mut keys = KeyPool::new(&app.config.load());

    // Resolve (or first-generate + persist) the anonymous install id that
    // update checks send as X-Install-Id (see SECURITY.md). Must run before
    // anything else can save settings.json or fire a check — including the
    // tray/About manual path, which has no App handle and reads the cached
    // value from update::INSTALL_ID.
    update::init_install_id(&app);

    // Publish the App handle so the manual update path (the About window, on its
    // own thread) can signal a clean shutdown when it relaunches into a new
    // version. Must precede the UI (and hence any manual install) coming up.
    update::set_app_handle(&app);

    // First-run / empty-key onboarding (§6), or a deliberate Settings "Save &
    // Restart" hand-off: reopen Settings in the replacement process so the user
    // returns to the window they initiated the restart from.
    let has_usable_key = keys.has_usable_key();
    let is_settings_relaunch = std::env::args().any(|arg| arg == "--relaunch");
    if !has_usable_key {
        onboarding::notify_no_key();
    }
    if should_open_settings_on_start(is_settings_relaunch, has_usable_key) {
        settings_ui::show_settings(Arc::clone(&app));
    }

    // Self-update housekeeping (clean up the old exe after a swap, show the
    // "you're now on vX" notice when relaunched with --updated), then the
    // daily-throttled background update check if the user hasn't disabled it.
    update::handle_startup_artifacts();
    if app.config.load().update_auto_check {
        update::spawn_startup_check(Arc::clone(&app));
    }

    // Keep the HKCU Run entry in sync with the run_at_startup setting.
    autostart::reconcile(app.config.load().run_at_startup);

    // Prewarm: probe the active provider's keys in the background so dead ones
    // are pre-marked and a validated key is queued before the first hotkey.
    if app.config.load().prewarm_keys {
        stt::spawn_prewarm(Arc::clone(&app), Arc::clone(&keys));
    }
    {
        let cfg = app.config.load();
        if cfg.stt_provider.eq_ignore_ascii_case("local") {
            local_stt::request_prewarm(&cfg.local_model);
        }
    }

    // Output (clipboard paste) worker.
    let _output_join = output::spawn(Arc::clone(&app));

    // UI (tray + cursor pip).
    let _ui_join = ui::spawn(Arc::clone(&app));

    // Hotkeys.
    let cfg_now = app.config.load();
    let toggle_combo = if cfg_now.hotkeys_enabled && !cfg_now.is_hold_mode() {
        Some(cfg_now.toggle_hotkey.clone())
    } else {
        None
    };
    let hold_combo = if cfg_now.hotkeys_enabled {
        Some(cfg_now.hold_hotkey.clone())
    } else {
        None
    };
    let reinsert_hold_duration = Duration::from_millis(cfg_now.reinsert_hold_ms);
    let hotkeys = HotkeyManager::start(
        toggle_combo,
        hold_combo,
        reinsert_hold_duration,
        cfg_now.mouse_hotkey_passthrough,
    )?;
    let _dev_trigger = dev_trigger::maybe_spawn(Arc::clone(&app), hotkeys.external_tx.clone());

    tracing::info!(
        "QuickDictate ready (mode={}, toggle={}, hold={})",
        cfg_now.mode,
        cfg_now.toggle_hotkey,
        cfg_now.hold_hotkey
    );

    let mut active: Option<SttHandle> = None;
    let mut pending_start: Option<PendingStart> = None;

    loop {
        if app.shutdown.load(Ordering::Acquire) {
            break;
        }
        start_queued_session_if_idle(&app, &mut keys, &mut active, &mut pending_start);
        let evt = match hotkeys.events.recv_timeout(Duration::from_millis(50)) {
            Ok(e) => e,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        // Processing may have completed while recv_timeout was blocked. Start
        // the already-queued session before interpreting a newly arrived event,
        // otherwise `pending_start` could survive into a later session.
        start_queued_session_if_idle(&app, &mut keys, &mut active, &mut pending_start);
        tracing::info!("hotkey event: {evt:?} (status={:?})", app.status());
        if app.status() == Status::Processing {
            let prior_pending = pending_start;
            if handle_processing_hotkey(&mut pending_start, evt) {
                match evt {
                    HotkeyEvent::TogglePressed => {
                        tracing::info!(
                        "queued toggle start while the local model finishes the previous dictation"
                    );
                    }
                    HotkeyEvent::HoldPressed => {
                        tracing::info!(
                        "queued hold start while the local model finishes the previous dictation"
                    );
                    }
                    HotkeyEvent::HoldReleased => {
                        if prior_pending == Some(PendingStart::Hold) {
                            tracing::info!(
                            "cancelled queued hold start because the key was released before local processing finished"
                        );
                        }
                    }
                    HotkeyEvent::ToggleLongPressed => unreachable!("not consumed above"),
                }
                continue;
            }
        }
        // Main owns the visible status. Streaming sessions may keep finalizing
        // while a newer one starts. Local batch inference is deliberately
        // serialized above: starting another epoch would make the generic
        // late-result guard discard the still-running local transcript.
        //
        // `active` tracks the *most recent* session. A handle whose `done`
        // flag is set means the session terminated on its own (clean or
        // errored); we treat it as "no live session" for hotkey purposes.
        let has_live = active.as_ref().map(|h| !h.is_done()).unwrap_or(false);
        match evt {
            HotkeyEvent::TogglePressed => {
                if has_live {
                    app.set_status(status_after_release(&app.config.load().stt_provider));
                    if let Some(h) = active.take() {
                        tracing::info!("Stopping session (toggle off)");
                        h.stop();
                    }
                } else {
                    // Drop any prior completed handle without touching its
                    // shared state; the background task will finish on its own.
                    let _ = active.take();
                    refresh_key_pool(&app, &mut keys);
                    tracing::info!("Starting session (toggle on)");
                    app.set_status(Status::Starting);
                    active = Some(stt::start_session(Arc::clone(&app), Arc::clone(&keys)));
                }
            }
            HotkeyEvent::ToggleLongPressed => {
                pending_start = None;
                if let Some(h) = active.take() {
                    tracing::info!("Discarding active session for saved-transcription replay");
                    app.invalidate_current_session();
                    h.stop();
                }
                app.word_count.store(0, Ordering::Release);
                app.set_status(Status::Idle);
                // try_send, never send: this runs on the win32 message-pump
                // thread. A blocking send on a full queue would freeze the
                // tray, the hotkeys, and every window this process owns until
                // the paste worker drained. Dropping one replay request is a
                // far better outcome than a frozen app.
                if let Err(e) = app.replay_tx.try_send(None) {
                    tracing::warn!("saved-transcription replay request dropped: {e}");
                }
            }
            HotkeyEvent::HoldPressed => {
                if !has_live {
                    let _ = active.take();
                    refresh_key_pool(&app, &mut keys);
                    tracing::info!("Starting session (hold press)");
                    app.set_status(Status::Starting);
                    active = Some(stt::start_session(Arc::clone(&app), Arc::clone(&keys)));
                }
            }
            HotkeyEvent::HoldReleased => {
                if has_live {
                    app.set_status(status_after_release(&app.config.load().stt_provider));
                    if let Some(h) = active.take() {
                        tracing::info!("Stopping session (hold release)");
                        h.stop();
                    }
                } else {
                    let _ = active.take();
                    app.set_status(Status::Idle);
                }
            }
        }
    }

    if let Some(h) = active.take() {
        h.stop();
    }
    hotkeys.shutdown();
    // A replacement process waits on our owned single-instance mutex. Keep the
    // runtime alive until every physical dictation has finalized and its stats
    // write is durable, then let process exit hand the mutex to the child.
    app.stats.finish_sessions_and_flush();
    sync::flush_before_exit(&app, Duration::from_secs(6));
    audio.shutdown();
    // Give in-flight pastes a moment to finish.
    std::thread::sleep(Duration::from_millis(50));
    Ok(())
}

#[cfg(test)]
mod logging_tests {
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

    #[test]
    fn local_release_stays_visible_while_batch_inference_finishes() {
        assert_eq!(status_after_release("local"), Status::Processing);
        assert_eq!(status_after_release("LOCAL"), Status::Processing);
        assert_eq!(status_after_release("elevenlabs"), Status::Idle);
    }

    #[test]
    fn local_processing_queues_toggle_and_cancellable_hold_starts() {
        let mut pending = None;
        assert!(handle_processing_hotkey(
            &mut pending,
            HotkeyEvent::TogglePressed
        ));
        assert_eq!(pending, Some(PendingStart::Toggle));

        assert!(handle_processing_hotkey(
            &mut pending,
            HotkeyEvent::HoldReleased
        ));
        assert_eq!(pending, Some(PendingStart::Toggle));

        assert!(handle_processing_hotkey(
            &mut pending,
            HotkeyEvent::HoldPressed
        ));
        assert_eq!(pending, Some(PendingStart::Hold));
        assert!(handle_processing_hotkey(
            &mut pending,
            HotkeyEvent::HoldReleased
        ));
        assert_eq!(pending, None);

        pending = Some(PendingStart::Toggle);
        assert!(!handle_processing_hotkey(
            &mut pending,
            HotkeyEvent::ToggleLongPressed
        ));
        assert_eq!(pending, None);
    }

    #[test]
    fn settings_relaunch_reopens_settings_for_configured_users() {
        assert!(should_open_settings_on_start(true, true));
        assert!(should_open_settings_on_start(true, false));
        assert!(should_open_settings_on_start(false, false));
        assert!(!should_open_settings_on_start(false, true));
    }
}
