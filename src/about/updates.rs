//! The live update check behind the status pill: the worker thread, the click
//! that starts an in-app install, and the messages they post back.

use core::ffi::c_void;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::update;

use super::*;

// ---- Update check (worker thread → WM_ABOUT_CHECKED) --------------------

/// Kick off a fresh GitHub update check on a worker thread; it posts the
/// outcome back to `hwnd` via [`WM_ABOUT_CHECKED`]. Also starts the spinner
/// timer so the "Checking…" arc animates until the result lands.
pub(super) unsafe fn start_check(hwnd: HWND) {
    // Animate the "Checking…" arc while the network call is in flight.
    let st = about_state(hwnd);
    if !st.is_null() {
        (*st).spinner_angle = 0;
    }
    let _ = SetTimer(hwnd, SPINNER_TIMER, SPINNER_INTERVAL_MS, None);

    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let outcome = update::check();
        // Hold the spinner up for at least SPINNER_MIN_MS so the check is
        // visibly "working" even when GitHub answers instantly.
        let min = std::time::Duration::from_millis(SPINNER_MIN_MS);
        let elapsed = start.elapsed();
        if elapsed < min {
            std::thread::sleep(min - elapsed);
        }
        let (code, lp) = match outcome {
            update::UpdateCheck::UpToDate => (0usize, 0isize),
            update::UpdateCheck::Available(tag) => (1usize, Box::into_raw(Box::new(tag)) as isize),
            update::UpdateCheck::Failed => (2usize, 0isize),
        };
        unsafe {
            // If the window was torn down before the post landed, reclaim the
            // boxed tag (only the Available case carries one — lp != 0).
            if PostMessageW(
                HWND(raw as *mut c_void),
                WM_ABOUT_CHECKED,
                WPARAM(code),
                LPARAM(lp),
            )
            .is_err()
                && lp != 0
            {
                drop(Box::from_raw(lp as *mut String));
            }
        }
    });
}

pub(super) unsafe fn invalidate_status(hwnd: HWND) {
    let st = about_state(hwnd);
    if !st.is_null() && (*st).status_pill != 0 {
        let _ = InvalidateRect(HWND((*st).status_pill as *mut c_void), None, true);
    }
}

/// Status-pill click: when an update is waiting, install it in-app (download →
/// verify → swap → relaunch) — the click is the consent, so no browser and no
/// extra prompt. Otherwise re-run the check (unless one is already in flight).
pub(super) unsafe fn on_status_click(hwnd: HWND) {
    let st = about_state(hwnd);
    if st.is_null() {
        return;
    }
    if let Status::Available(tag) = &(*st).status {
        let tag = tag.clone();
        // Show the spinner as "Updating…" and block further clicks while the
        // install runs; on failure the worker posts WM_ABOUT_UPDATE_FAILED,
        // which restores the "Update to <tag>" pill.
        (*st).checking = true;
        (*st).status = Status::Updating;
        (*st).spinner_angle = 0;
        let _ = SetTimer(hwnd, SPINNER_TIMER, SPINNER_INTERVAL_MS, None);
        invalidate_status(hwnd);
        start_install(hwnd, tag);
        return;
    }
    if (*st).checking {
        return;
    }
    (*st).checking = true;
    (*st).status = Status::Checking;
    invalidate_status(hwnd);
    start_check(hwnd);
}

/// Run the in-app update on a worker thread. On success the process relaunches
/// into the new exe (this window goes away with it). On failure, show the error
/// (with the manual-download link) and post [`WM_ABOUT_UPDATE_FAILED`] so the
/// pill drops back to "Update to <tag>" for a retry.
unsafe fn start_install(hwnd: HWND, tag: String) {
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        if let Err(e) = update::download_and_install_now(&tag) {
            tracing::error!("update: {e}");
            update::msg_box(
                "QuickDictate update failed",
                &format!(
                    "{e}\n\nYou can download the update manually from:\n{}",
                    update::RELEASES_URL
                ),
                MB_OK | MB_ICONERROR,
            );
            unsafe {
                // Reclaim the box if the post fails — PostMessageW to an
                // already-destroyed window fails synchronously, so this is
                // race-free: the handler consumes the tag on success, we free it
                // here on failure (About closed mid-download).
                let boxed = Box::into_raw(Box::new(tag));
                if PostMessageW(
                    HWND(raw as *mut c_void),
                    WM_ABOUT_UPDATE_FAILED,
                    WPARAM(0),
                    LPARAM(boxed as isize),
                )
                .is_err()
                {
                    drop(Box::from_raw(boxed));
                }
            }
        }
    });
}
