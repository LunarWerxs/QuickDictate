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
mod onboarding;
mod output;
mod settings_ui;
mod sound;
mod state;
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
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
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
/// Restart" (`--relaunch`) -- a held mutex means the old version is mid-shutdown
/// to hand off to us, so we take over (return `true`) instead of bailing --
/// otherwise the hand-off would leave zero instances running.
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
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) };
    if let Err(e) = handle {
        // Couldn't even ask the question -- fail open rather than block the
        // user from launching QuickDictate at all.
        tracing::warn!("single-instance: CreateMutexW failed: {e}; continuing anyway");
        return true;
    }
    let already_running =
        unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_ALREADY_EXISTS;
    if !already_running {
        // We own the mutex now; see doc comment above re: no cleanup needed.
        return true;
    }

    // A deliberate self-respawn — the self-updater's relaunch
    // (`update::relaunch` → `<exe> --updated <tag>`) or Settings' "Save &
    // Restart" (`<exe> --relaunch`) — is a hand-off: the "other instance" is the
    // OLD process, already latched to shut down to make way for us. If we bailed
    // here (reveal-and-quit) we'd leave ZERO instances the moment the old one
    // finishes exiting — the respawn would just kill the app. So take over
    // instead: we already hold a fresh handle to the named object (`CreateMutexW`
    // above), which keeps the single-instance guard alive for later launches
    // once the old process releases its handle on exit. The overlap is safe —
    // the hotkey layer already retries registration across exactly this hand-off
    // (see `hotkeys::register_initial`).
    if std::env::args().any(|a| a == "--updated" || a == "--relaunch") {
        tracing::info!(
            "single-instance: deliberate respawn (--updated/--relaunch); taking over from the exiting old instance"
        );
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

/// Returns the directory the log file lives in (also returned so callers can
/// surface it to the user). Falls back to cwd if exe dir cannot be located.
fn log_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Install a panic hook that writes panic info to a dedicated unbuffered
/// file (and via tracing, if it still works). Without this, panics in any
/// background thread silently disappear under `windows_subsystem = "windows"`.
fn install_panic_hook() {
    let panic_path = log_dir().join("quickdictate-panic.log");
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
        let path = dir.join("quickdictate.log");
        let old_path = dir.join("quickdictate.log.old");

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
        let dir = log_dir();
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
                    "File logging enabled at {}\\quickdictate.log ({} MiB cap, {} queued lines max)",
                    dir.display(),
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
                    "File logging requested but {}\\quickdictate.log could not be opened: {e}",
                    dir.display()
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

    // Load (and possibly generate) settings.json before initializing tracing,
    // because `enable_logging` is read out of the config.
    let (mut cfg, diags) = Config::load_or_create();

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
    for line in diags {
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

    // First-run / empty-key onboarding (§6): if no provider has a usable key,
    // open the Settings window straight away so the user lands directly on the
    // fix (the window shows an "add a key to get started" banner). We also log
    // a warning line for the log file. Only fires when genuinely unconfigured,
    // so a configured user never sees it.
    if !keys.has_usable_key() {
        onboarding::notify_no_key();
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

    loop {
        if app.shutdown.load(Ordering::Acquire) {
            break;
        }
        let evt = match hotkeys.events.recv_timeout(Duration::from_millis(50)) {
            Ok(e) => e,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        tracing::info!("hotkey event: {evt:?} (status={:?})", app.status());
        // Main owns the visible status. Sessions are allowed to keep running
        // in the background while a new one starts -- they each have their
        // own audio thread, WS connection, and finalize independently. The
        // user perceives an instant "ready for next dictation."
        //
        // `active` tracks the *most recent* session. A handle whose `done`
        // flag is set means the session terminated on its own (clean or
        // errored); we treat it as "no live session" for hotkey purposes.
        let has_live = active.as_ref().map(|h| !h.is_done()).unwrap_or(false);
        match evt {
            HotkeyEvent::TogglePressed => {
                if has_live {
                    if let Some(h) = active.take() {
                        tracing::info!("Stopping session (toggle off)");
                        h.stop();
                    }
                    app.set_status(Status::Idle);
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
                if let Some(h) = active.take() {
                    tracing::info!("Stopping session (hold release)");
                    h.stop();
                }
                app.set_status(Status::Idle);
            }
        }
    }

    if let Some(h) = active.take() {
        h.stop();
    }
    hotkeys.shutdown();
    audio.shutdown();
    // Give in-flight pastes a moment to finish.
    std::thread::sleep(Duration::from_millis(50));
    Ok(())
}

#[cfg(test)]
mod logging_tests {
    use super::*;

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
}
