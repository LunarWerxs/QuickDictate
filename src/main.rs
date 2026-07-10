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

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_ICONWARNING, MB_OK};

use crate::audio::AudioSource;
use crate::config::Config;
use crate::hotkeys::{HotkeyEvent, HotkeyManager};
use crate::keys::KeyPool;
use crate::state::{App, Status};
use crate::stt::SttHandle;

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
/// Rotate `quickdictate.log` aside if it already exceeds `max_log_mb`, keeping
/// one previous generation as `quickdictate.log.old`. `max_log_mb == 0`
/// disables the cap. Runs before the appender opens the file (in append mode),
/// so after a rotation logging starts fresh. Best-effort: any filesystem error
/// just leaves the existing file in place.
fn rotate_log_if_needed(dir: &std::path::Path, max_log_mb: u64) {
    if max_log_mb == 0 {
        return;
    }
    let path = dir.join("quickdictate.log");
    let max_bytes = max_log_mb.saturating_mul(1024 * 1024);
    match std::fs::metadata(&path) {
        Ok(meta) if meta.len() > max_bytes => {
            let old = dir.join("quickdictate.log.old");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&path, &old);
        }
        _ => {}
    }
}

fn init_logging(
    file_logging: bool,
    max_log_mb: u64,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = EnvFilter::try_from_env("QUICKDICTATE_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,quickdictate=debug"));

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_thread_names(true);

    if file_logging {
        let dir = log_dir();
        rotate_log_if_needed(&dir, max_log_mb);
        let file_appender = tracing_appender::rolling::never(&dir, "quickdictate.log");
        let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
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
            "File logging enabled at {}\\quickdictate.log",
            dir.display()
        );
        Some(guard)
    } else {
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(stdout_layer)
            .try_init();
        None
    }
}

fn main() -> Result<()> {
    // Load (and possibly generate) settings.json before initializing tracing,
    // because `enable_logging` is read out of the config.
    let (mut cfg, diags) = Config::load_or_create();

    // `--provider <id>` overrides settings.json's stt_provider for this run —
    // the per-provider launcher .bat files at the repo root use this so one
    // settings.json (with all the keys) can serve every provider.
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
    let keys = KeyPool::new(&app.config.load());

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
