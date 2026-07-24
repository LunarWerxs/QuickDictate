#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod about;
mod audio;
mod autostart;
mod config;
mod dev_trigger;
mod focus;
mod hotkeys;
mod icon;
mod keys;
mod local_stt;
mod onboarding;
mod output;
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
/// Falls back to `./logs` if the executable directory cannot be located.
pub(crate) fn logs_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(LOGS_DIR_NAME)
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
    let exe_dir = logs_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    prepare_logs_dir_at(&exe_dir, &logs_dir)
}

/// Install a panic hook that writes panic info to a dedicated unbuffered
/// file (and via tracing, if it still works). Without this, panics in any
/// background thread silently disappear under `windows_subsystem = "windows"`.
fn install_panic_hook() {
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
        //    if the tracing pipeline is mid-shutdown.
        use std::io::Write as _;
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
/// Single-file writer with one rotated generation. Unlike a startup-only
/// check, this keeps a long-running process bounded too. It is owned by
/// tracing-appender's one background worker, so no extra synchronization is
/// needed here.
struct SizeCappedLogWriter {
    file: Option<std::fs::File>,
    path: PathBuf,
    old_path: PathBuf,
    max_bytes: u64,
    bytes_written: u64,
}

impl SizeCappedLogWriter {
    fn open(dir: &Path, max_log_mb: u64) -> io::Result<Self> {
        Self::open_with_max_bytes(dir, max_log_mb.saturating_mul(1024 * 1024))
    }

    fn open_with_max_bytes(dir: &Path, max_bytes: u64) -> io::Result<Self> {
        let path = dir.join(MAIN_LOG_NAME);
        let old_path = dir.join(OLD_LOG_NAME);

        // Preserve the previous startup behavior as well as rotating during
        // this run. Rotation is diagnostic-only and best-effort: if an old log
        // is locked, keep appending rather than preventing QuickDictate launch.
        if max_bytes != 0
            && std::fs::metadata(&path)
                .map(|meta| meta.len() > max_bytes)
                .unwrap_or(false)
        {
            let _ = std::fs::remove_file(&old_path);
            let _ = std::fs::rename(&path, &old_path);
        }

        let file = Self::open_file(&path)?;
        let bytes_written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self {
            file: Some(file),
            path,
            old_path,
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

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            let _ = file.flush();
        }

        let rotation_result = (|| -> io::Result<()> {
            match std::fs::remove_file(&self.old_path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            std::fs::rename(&self.path, &self.old_path)
        })();

        // Always try to restore a usable writer. If rotation itself failed
        // (for example an antivirus briefly locked the file), disable further
        // attempts for this run so every subsequent log line does not repeat
        // filesystem work. The next launch gets another chance.
        let file = Self::open_file(&self.path)?;
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

fn init_logging(
    file_logging: bool,
    max_log_mb: u64,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = EnvFilter::try_from_env("QUICKDICTATE_LOG")
        // File logging is a user-facing diagnostic, so keep the default at
        // summaries. Developers can opt into verbose per-frame/per-partial
        // detail with QUICKDICTATE_LOG=info,quickdictate=debug.
        .unwrap_or_else(|_| EnvFilter::new("info"));

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

    // Prepare the diagnostics folder before either logger can open a file.
    // Migration messages are replayed once tracing is initialized below.
    let mut startup_diags = prepare_logs_dir();

    // Load (and possibly generate) settings.json before initializing tracing,
    // because `enable_logging` is read out of the config.
    let (mut cfg, config_diags) = Config::load_or_create();
    startup_diags.extend(config_diags);

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

    install_panic_hook();
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
    let hotkeys = HotkeyManager::start(toggle_combo, hold_combo, reinsert_hold_duration)?;
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
                if app.replay_tx.send(None).is_err() {
                    tracing::warn!(
                        "saved-transcription replay requested, but output worker is unavailable"
                    );
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
    audio.shutdown();
    // Give in-flight pastes a moment to finish.
    std::thread::sleep(Duration::from_millis(50));
    Ok(())
}

#[cfg(test)]
mod logging_tests {
    use super::*;

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
    fn log_writer_rotates_during_a_long_run() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "quickdictate-log-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let mut writer = SizeCappedLogWriter::open_with_max_bytes(&dir, 10).unwrap();
        writer.write_all(b"12345678").unwrap();
        writer.write_all(b"abcd").unwrap();
        writer.flush().unwrap();
        drop(writer);

        assert_eq!(
            std::fs::read(dir.join("quickdictate.log.old")).unwrap(),
            b"12345678"
        );
        assert_eq!(
            std::fs::read(dir.join("quickdictate.log")).unwrap(),
            b"abcd"
        );
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
