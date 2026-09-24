//! The user-facing flows that run on worker threads: the startup check, the
//! silent install path, and the post-relaunch cleanup.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_TOPMOST};

use crate::state::{App, Status};

use super::*;

// ---------------------------------------------------------------------------
// User-facing flows (worker threads; MessageBoxes are fine off the UI thread)
// ---------------------------------------------------------------------------

pub fn msg_box(
    title: &str,
    body: &str,
    style: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE,
) -> windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_RESULT {
    let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let body_w: Vec<u16> = body.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            HWND::default(),
            PCWSTR(body_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            style | MB_TOPMOST,
        )
    }
}

/// Silently install `tag` on the auto path: download + verify + swap, then
/// relaunch **only if idle**. If a dictation is in progress the swap is left
/// staged — the new exe already takes effect on the next launch — so a silent
/// background update never yanks the app out from under an active session.
/// Any failure is logged and swallowed (offline / no release yet is not the
/// user's problem). `IN_FLIGHT` is already held by [`spawn_startup_check`].
///
/// Returns `true` iff it actually relaunched — the caller must then leave
/// `IN_FLIGHT` set (the process is exiting), just as [`download_and_install_now`]
/// does, so a stray About-pill click during the ~100 ms shutdown window can't
/// kick off a second concurrent relaunch.
fn install_silently(app: &App, tag: &str) -> bool {
    let exe = match download_and_swap(tag) {
        Ok(exe) => exe,
        Err(e) => {
            tracing::info!("update: silent install failed (silent): {e}");
            return false;
        }
    };
    if app.status() == Status::Idle {
        // Silent background update: relaunch WITHOUT reopening About — no window
        // pops up unprompted (that would defeat "silent"). Settings does come
        // back if it is on screen: that is restoring, not popping up.
        let reopen = Reopen {
            about: false,
            settings: crate::settings_ui::is_open(),
        };
        match relaunch(&exe, tag, reopen) {
            Ok(()) => return true, // relaunched; process on its way out
            Err(e) => tracing::error!("update: {e}"),
        }
    } else {
        tracing::info!("update: staged v{tag}; applies on next restart (dictation active)");
    }
    false
}

/// Startup auto-check (settings `update_auto_check`, default on). Throttled to
/// one network hit per 24 h; silent throughout — when an update exists it is
/// installed with no prompt (see [`install_silently`]).
pub fn spawn_startup_check(app: Arc<App>) {
    if IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        let relaunching = match read_cache() {
            Some((ts, tag)) if cache_is_fresh(ts, now_secs()) => {
                tracing::debug!("update: skipping auto-check (cache fresh)");
                republish_cached(&tag);
                false
            }
            _ => run_startup_check(&app),
        };
        // Leave the lock held if we relaunched (the process is exiting) so a
        // concurrent manual install can't spawn a second child; otherwise free
        // it for the next check.
        if !relaunching {
            IN_FLIGHT.store(false, Ordering::Release);
        }
    });
}

/// A fresh cache skips the network, but still remembers the latest tag the
/// last check found. Republish it when it is newer than this build: without
/// that, any restart within 24 h of finding an update (Save & Restart, a
/// reboot) blanked the tray tooltip and the Settings banner until the next
/// day's check, and updates are notify-only by default.
fn republish_cached(tag: &str) {
    if let UpdateCheck::Available(tag) = compare_tag(tag, env!("CARGO_PKG_VERSION")) {
        tracing::info!("update: v{tag} available (cached); waiting for the user to confirm");
        set_pending_update(Some(tag));
    }
}

/// One real network check and what follows from it. Returns `true` iff the
/// silent install relaunched the app.
fn run_startup_check(app: &App) -> bool {
    match check() {
        UpdateCheck::Available(tag) => on_update_available(app, &tag),
        UpdateCheck::UpToDate => {
            write_cache(env!("CARGO_PKG_VERSION"));
            set_pending_update(None);
            tracing::info!("update: up to date");
            false
        }
        UpdateCheck::Failed => {
            // Silent: no release yet / offline is not the user's problem.
            // Stamp the cache with the short retry window: bounded
            // network chatter while offline, without one boot-time
            // blip suppressing a real update notice for a day.
            write_cache_failed();
            tracing::info!("update: auto-check failed (silent); retrying in about an hour");
            false
        }
    }
}

/// Cache and publish a newer release, then install it only if the user opted
/// in. Returns `true` iff the silent install relaunched the app.
fn on_update_available(app: &App, tag: &str) -> bool {
    write_cache(tag);
    set_pending_update(Some(tag.to_string()));
    if app.config.load().update_auto_install {
        tracing::info!("update: v{tag} available; installing silently (opted in)");
        return install_silently(app, tag);
    }
    // Default since v0.5.4. The URL and the SHA-256 both come out of the
    // release payload, so hash pinning proves the bytes match what was
    // uploaded, not that the maintainer meant to upload them. Anything able
    // to publish a release would otherwise reach every install unattended
    // within a day. The click on the About pill is the consent.
    tracing::info!(
        "update: v{tag} available; waiting for the user to confirm \
         (set update_auto_install to install silently)"
    );
    false
}

/// Startup housekeeping: delete the `.old` exe left by a previous self-update,
/// and — when relaunched with `--updated <ver> --show-about` — reopen the About
/// window so the user lands back where they were and sees the new version.
/// (Settings, when it was open, is reopened by `startup` on the `--relaunch`
/// flag the same relaunch carries; it runs before this, so About lands on top
/// of it exactly as it was.)
pub fn handle_startup_artifacts() {
    if let Ok(exe) = std::env::current_exe() {
        let old = exe.with_extension("exe.old");
        if old.exists() {
            match std::fs::remove_file(&old) {
                Ok(()) => tracing::info!("update: removed leftover {}", old.display()),
                // The old instance may still be exiting; next launch gets it.
                Err(e) => tracing::debug!("update: could not remove {} yet: {e}", old.display()),
            }
        }
    }
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--updated") {
        let ver = args.get(i + 1).cloned().unwrap_or_default();
        tracing::info!("update: relaunched after update to v{ver}");
        // Reopen the About window only after a *manual* update (the user clicked
        // the About pill → relaunch carries `--show-about`), so they land back
        // where they were and see the new version on its pill. A silent
        // background auto-update carries no `--show-about` and stays silent — no
        // modal notice, no window popping up unprompted. show_about() runs on
        // its own thread, so startup isn't blocked.
        if args.iter().any(|a| a == "--show-about") {
            crate::about::show_about();
        }
    }
}
