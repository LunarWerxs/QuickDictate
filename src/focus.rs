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

use windows::Win32::Foundation::{CloseHandle, HWND, MAX_PATH};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// The lowercased exe basename of the process owning the current foreground
/// window (e.g. `"code.exe"`), or `None` if it can't be determined (no
/// foreground window, access denied, etc.). Every failure is non-fatal —
/// callers should fall back to "no match" / global settings.
pub fn foreground_exe_name() -> Option<String> {
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
