//! Foreground-window process detection, for Per-App Profiles.
//!
//! Resolved **once, at transcription-commit time** (just before
//! `TextProcessor::process` runs on a final transcript) rather than
//! continuously polled — dictation sessions can run long, and the user may
//! well switch windows mid-session; we want the profile that matches wherever
//! focus actually is *when the text is about to be typed*, not wherever it
//! was when the hotkey was pressed.
//!
//! Windows-only, matching the rest of the app.

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, MAX_PATH};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
};

/// The foreground window and the ids of the thread and process that own it,
/// or `None` if there is no foreground window or its owner cannot be read.
fn foreground_owner() -> Option<(HWND, u32, u32)> {
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid: u32 = 0;
        let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if tid == 0 || pid == 0 {
            return None;
        }
        Some((hwnd, tid, pid))
    }
}

/// The lowercased exe basename of the process owning the current foreground
/// window (e.g. `"code.exe"`), or `None` if it can't be determined (no
/// foreground window, access denied, etc.). Every failure is non-fatal —
/// callers should fall back to "no match" / global settings.
pub fn foreground_exe_name() -> Option<String> {
    let (_, _, pid) = foreground_owner()?;
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; MAX_PATH as usize];
        let mut len: u32 = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        result.ok()?;

        let path = String::from_utf16_lossy(&buf[..len as usize]);
        basename_lower(&path)
    }
}

/// Where keyboard input is going right now, as `(foreground window, focused
/// control)`, used to check that focus has not moved between a paste and a
/// later "scratch that" undo. The raw HWND values are enough: if a window was
/// destroyed and a new one reused the handle, the exe-name check alongside it
/// still has to match.
///
/// The focused control tells two native fields of one window apart, which the
/// top-level HWND alone cannot. It is `0` when the owning thread reports none
/// or the query fails, and `0` only ever matches another `0`. Apps that draw
/// their own fields (browsers, Electron) keep one focus HWND for all of them,
/// so there it adds nothing.
pub fn foreground_focus_ids() -> Option<(isize, isize)> {
    let (hwnd, tid, _) = foreground_owner()?;
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let focus = match unsafe { GetGUIThreadInfo(tid, &mut info) } {
        Ok(()) => info.hwndFocus.0 as isize,
        Err(_) => 0,
    };
    Some((hwnd.0 as isize, focus))
}

/// Whether the foreground window belongs to a process at a HIGHER integrity
/// level than ours. Windows' UIPI silently discards injected input in that
/// case, and `SendInput` still reports every event as sent, so this is the
/// only way to tell the user why nothing was typed.
///
/// `None` means "could not determine" (the common case for a protected
/// process we cannot even open), which callers treat as "not blocked" so a
/// probe failure never suppresses a paste that would have worked.
pub fn foreground_is_elevated() -> Option<bool> {
    let (_, _, pid) = foreground_owner()?;
    unsafe {
        let target = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let theirs = integrity_of(target);
        let _ = CloseHandle(target);
        let ours = integrity_of(GetCurrentProcess());
        match (ours, theirs) {
            (Some(ours), Some(theirs)) => Some(theirs > ours),
            _ => None,
        }
    }
}

/// The mandatory integrity level of `process` (the last sub-authority of its
/// token's integrity SID), or `None` if the token cannot be read.
unsafe fn integrity_of(process: HANDLE) -> Option<u32> {
    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
    let mut needed: u32 = 0;
    let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut needed);
    if needed == 0 {
        let _ = CloseHandle(token);
        return None;
    }
    let mut buf = vec![0u8; needed as usize];
    let ok = GetTokenInformation(
        token,
        TokenIntegrityLevel,
        Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
        needed,
        &mut needed,
    );
    let _ = CloseHandle(token);
    ok.ok()?;
    let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
    let count = GetSidSubAuthorityCount(label.Label.Sid);
    if count.is_null() || *count == 0 {
        return None;
    }
    Some(*GetSidSubAuthority(label.Label.Sid, (*count - 1) as u32))
}

/// Extract and lowercase the file-name component of a Windows path
/// (`"C:\\Foo\\Code.exe"` -> `"code.exe"`). Accepts either `\` or `/` as the
/// separator (the API returns `\`, but this keeps it robust for tests).
fn basename_lower(path: &str) -> Option<String> {
    let name = path.rsplit(['\\', '/']).next()?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_lower_strips_dir_and_lowercases() {
        assert_eq!(
            basename_lower(r"C:\Users\me\AppData\Local\Programs\Microsoft VS Code\Code.exe"),
            Some("code.exe".to_string())
        );
        assert_eq!(
            basename_lower(r"C:\Windows\System32\WindowsTerminal.exe"),
            Some("windowsterminal.exe".to_string())
        );
    }

    #[test]
    fn basename_lower_handles_forward_slashes_and_bare_names() {
        assert_eq!(basename_lower("/usr/bin/foo"), Some("foo".to_string()));
        assert_eq!(
            basename_lower("notepad.exe"),
            Some("notepad.exe".to_string())
        );
    }

    #[test]
    fn basename_lower_rejects_empty_or_trailing_separator() {
        assert_eq!(basename_lower(""), None);
        assert_eq!(basename_lower(r"C:\Foo\"), None);
    }

    /// Smoke test: whatever `foreground_exe_name` returns must be sane: a
    /// non-empty, already-lowercased basename. We deliberately do NOT assert a
    /// ".exe" suffix or a specific value, because the foreground process varies
    /// by environment (headless CI, for instance, reports a bare
    /// "hosted-compute-agent" with no extension). `None` is fine too (no
    /// foreground window, access denied, etc.).
    #[test]
    fn foreground_exe_name_smoke() {
        if let Some(n) = foreground_exe_name() {
            assert!(!n.is_empty(), "foreground name should be non-empty");
            assert_eq!(
                n,
                n.to_ascii_lowercase(),
                "foreground name should be lowercased"
            );
        }
    }
}
