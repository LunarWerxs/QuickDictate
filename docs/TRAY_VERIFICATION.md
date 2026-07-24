# Verifying the tray menu on Windows 11

How to drive and read QuickDictate's tray right-click menu directly, so a
change to the tray menu (`src/ui.rs`, built on the `tray-icon` crate,
currently pinned `tray-icon = "0.19"` in `Cargo.toml`) can be proven working
end-to-end instead of just claimed from source. Verified working against
`tray-icon` 0.19.3.

## Why not just click it

On Windows 11, a newly-added tray icon lands in the hidden-icons overflow
flyout (`TopLevelWindowForOverflowXamlIsland`). That flyout auto-closes
between separate PowerShell processes, and synthetic clicks routed through
it are flaky. Don't fight the shell for this; talk to the app's own window
instead.

## Talk to the `tray-icon` crate's window directly

1. **Find the window.** `EnumWindows` for a top-level window of class
   `tray_icon_app` owned by the app's PID. It's a real window (created with
   `WS_EX_TOOLWINDOW`), just not one that's ever shown.
2. **Park the cursor first.** Call `SetCursorPos` to the tray icon's
   location before opening the menu: `show_tray_menu` in the `tray-icon`
   crate pops the menu at the *current cursor position*, not at the icon.
3. **Open the menu.** `PostMessage(hwnd, 6002, 0, 0x0204)`:
   - `6002` is `WM_USER_TRAYICON`, the crate's custom message for tray icon
     events.
   - `0x0204` is `WM_RBUTTONDOWN` and must be the **lparam**.
   - The crate opens the menu on **`WM_RBUTTONDOWN` (button *down*,
     `0x0204`)**, not `WM_RBUTTONUP` (`0x0205`). Posting the UP message
     fires the crate's click event but never opens a menu. This is the
     single easiest step to get wrong.
4. **Find the popup menu window.** `FindWindowW("#32768", null)`. This is
   the native Win32 popup-menu window class.
5. **Read the menu items via the Win32 menu API, not UIA.**
   `SendMessage(menuHwnd, 0x01E1 /* MN_GETHMENU */, 0, 0)` returns the
   `HMENU`. From there:
   - `GetMenuItemCount` for the number of items.
   - `GetMenuStringW(hmenu, index, ..., MF_BYPOSITION)` for each item's
     label.
   - `GetMenuState` for flags; `0x800` (`MF_SEPARATOR`) marks a separator
     row.
   - `GetSubMenu` to descend into a submenu.
6. **Click an item.** `GetMenuItemRect(IntPtr.Zero, hmenu, index, out r)`
   gives you the item's screen rect; send a real mouse click at its center
   (a synthetic `WM_COMMAND` to the menu is not reliable here; use an
   actual click).
7. **Dismiss any resulting dialog.** If the clicked item pops a
   `MessageBox`, find it with `FindWindowW("#32770", "<dialog title>")`
   (`#32770` is the native dialog class) and dismiss it with
   `PostMessage(dlg, WM_COMMAND, IDYES=6 | IDNO=7, 0)` as appropriate.

## Gotchas

- **UIA cannot see the menu body.** `AutomationElement.FromHandle` on the
  `#32768` popup window returns zero descendants. Windows renders native
  popup menus outside the UI Automation tree. You have to use the Win32
  menu API (`MN_GETHMENU` etc.) above; there is no UIA shortcut. UIA *does*
  work fine for the resulting `#32770` MessageBox and for locating tray
  icons themselves, just not for reading the popup menu's items.
- **`GetClassNameW` must be P/Invoked with `CharSet = CharSet.Unicode`.**
  Without it, the receiving `StringBuilder` marshals as ANSI and every
  class name truncates to its first character: `"tray_icon_app"` comes
  back as `"t"`, which looks like the window wasn't found at all.
- **A PowerShell scriptblock used as the `EnumWindows` callback silently
  stops enumeration partway through.** Write the entire enumeration loop
  inside a C# block compiled with `Add-Type`, and call that instead of
  passing a native PowerShell scriptblock as the callback.

## Quick reference

| Constant | Value | Meaning |
|---|---|---|
| `WM_USER_TRAYICON` | `6002` | Custom message the `tray-icon` crate posts to its own window for tray events |
| `WM_RBUTTONDOWN` | `0x0204` | lparam that must accompany `WM_USER_TRAYICON` to open the menu |
| `WM_RBUTTONUP` | `0x0205` | Fires the crate's click event but does **not** open the menu |
| `MN_GETHMENU` | `0x01E1` | `SendMessage` code to get the `HMENU` from a `#32768` popup window |
| `MF_SEPARATOR` | `0x800` | `GetMenuState` flag identifying a separator row |
| Popup menu window class | `#32768` | Native Win32 popup menu |
| Dialog window class | `#32770` | Native Win32 dialog (e.g. a MessageBox) |
| Tray window class | `tray_icon_app` | The `tray-icon` crate's own top-level message window |
