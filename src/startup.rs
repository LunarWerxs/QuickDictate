//! Everything that happens before the hotkey loop can run.
//!
//! The single-instance hand-off, the `--version` canary, settings + logging,
//! the audio pipeline, and `bring_up_app`, which constructs the `App` and
//! starts every background worker the running app depends on.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, LPARAM, WAIT_ABANDONED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject, INFINITE};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, PostMessageW, RegisterWindowMessageW, MB_ICONERROR, MB_ICONWARNING, MB_OK,
};

use crate::audio::{self, AudioSource};
use crate::config::Config;
use crate::hotkeys::HotkeyManager;
use crate::keys::KeyPool;
use crate::logging::{init_logging, install_panic_hook, prepare_logs_dir};
use crate::state::App;
use crate::{
    autostart, dev_trigger, feedback_survey, local_stt, nudge, onboarding, output, paths,
    settings_ui, stats, stt, ui, update,
};

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
pub(crate) fn single_instance_guard() -> bool {
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

fn should_open_settings_on_start(is_settings_relaunch: bool, has_usable_key: bool) -> bool {
    is_settings_relaunch || !has_usable_key
}

/// A side-effect-free canary for release CI and the self-updater, handled
/// before the single-instance mutex, settings, microphone, hotkeys, tray, or
/// logging are initialized. Returns whether it consumed the invocation.
pub(crate) fn handle_version_flag() -> bool {
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--version" || arg == "version")
    {
        println!("{}", env!("CARGO_PKG_VERSION"));
        true
    } else {
        false
    }
}

/// Load settings.json, resolve the data folder, and bring up logging --
/// everything from `Config::load_or_create` through the panic hook and
/// `RUST_BACKTRACE`. Nothing before this point may touch disk; everything
/// after it reads through the returned `Config`.
pub(crate) fn init_settings_and_logging(
) -> (Config, Option<tracing_appender::non_blocking::WorkerGuard>) {
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
    let log_guard = init_logging(file_logging, cfg.max_log_mb);
    if explicit_provider {
        tracing::info!("provider override from command line: {}", cfg.stt_provider);
    }
    if let Some(p) = &auto_provider {
        tracing::info!(
            "configured provider had no keys; auto-selected '{p}' (the only provider with keys)"
        );
    }

    replay_startup_diagnostics(startup_diags);

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

    (cfg, log_guard)
}

/// Replay the config-loading diagnostics through tracing now that it's up.
/// "ALERT: " lines (a corrupt settings.json that was backed up and replaced
/// with defaults) also get a message box — with windows_subsystem="windows"
/// a log line alone is invisible, and the user must learn their keys/prefs
/// were sidelined. Shown from a worker thread so startup isn't blocked.
fn replay_startup_diagnostics(startup_diags: Vec<String>) {
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
}

/// Pre-warm the audio pipeline. The WASAPI stream stays open for the app's
/// lifetime so sessions never pay mic-initialization latency. With
/// windows_subsystem="windows" a bare `?` here would exit with no visible
/// trace of why, so a missing/broken microphone gets a message box before we
/// bail. Publishes the microphone preference before the source opens, so the
/// very first stream already lands on the right device.
pub(crate) fn init_audio_pipeline(cfg: &Config) -> Result<Arc<AudioSource>> {
    audio::set_preferred_input(&cfg.input_device);
    match AudioSource::new() {
        Ok(a) => Ok(Arc::new(a)),
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
            Err(e)
        }
    }
}

/// Everything `main` gets back from [`bring_up_app`]: the handles the event
/// loop needs, plus the background-worker join handles that must simply stay
/// alive for the app's lifetime (never read again, so each is `_`-prefixed).
pub(crate) struct Started {
    pub(crate) app: Arc<App>,
    pub(crate) keys: Arc<KeyPool>,
    pub(crate) hotkeys: HotkeyManager,
    _output_join: std::thread::JoinHandle<()>,
    _ui_join: std::thread::JoinHandle<()>,
    _dev_trigger: Option<std::thread::JoinHandle<()>>,
}

/// Construct the `App`, open Settings if this is a first run or a Save &
/// Restart hand-off, and start every background worker: install-id
/// resolution, update housekeeping, autostart sync, key prewarm, output,
/// tray/UI, and hotkeys.
pub(crate) fn bring_up_app(
    cfg: Config,
    rt_handle: tokio::runtime::Handle,
    audio: Arc<AudioSource>,
) -> Result<Started> {
    let app = App::new(cfg, rt_handle, Arc::clone(&audio));
    let keys = KeyPool::new(&app.config.load());

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

    // Count this launch for the "you could be signed in" prompt — but NOT a Save & Restart
    // hand-off, which is one sitting the user never left. Counting it would inflate the session
    // total, and worse: the engine treats a session that begins with an unanswered ask as a
    // decline, so restarting from the Settings window with the banner up would silently spend one
    // of the user's two declines on an action that was not a refusal at all.
    if !is_settings_relaunch {
        nudge::start_session();
        feedback_survey::start_session();
    }

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

    // Anonymous usage rollup (opt-in, off by default, see
    // `Config::share_usage_stats`): once a day, send LunarWerx an
    // aggregated, PII-free snapshot of this install's usage totals. A no-op
    // (returns immediately) unless the setting is on.
    stats::spawn_daily_report(Arc::clone(&app));

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

    Ok(Started {
        app,
        keys,
        hotkeys,
        _output_join,
        _ui_join,
        _dev_trigger,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_relaunch_reopens_settings_for_configured_users() {
        assert!(should_open_settings_on_start(true, true));
        assert!(should_open_settings_on_start(true, false));
        assert!(should_open_settings_on_start(false, false));
        assert!(!should_open_settings_on_start(false, true));
    }
}
