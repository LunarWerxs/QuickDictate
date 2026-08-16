//! Optional debug control channel.
//!
//! When the env var `QUICKDICTATE_DEV_PORT` is set, the app binds a UDP
//! socket on `127.0.0.1:<port>` (use 0 for ephemeral) and forwards textual
//! commands as `HotkeyEvent`s into the same channel the real hotkey thread
//! uses. This is how the end-to-end smoke test drives the pipeline without
//! relying on synthetic keystrokes, which Windows doesn't always forward
//! to `RegisterHotKey` listeners.
//!
//! Commands (ASCII, one per datagram):
//!   `toggle`           -> HotkeyEvent::TogglePressed
//!   `toggle_long`      -> HotkeyEvent::ToggleLongPressed
//!   `hold_press`       -> HotkeyEvent::HoldPressed
//!   `hold_release`     -> HotkeyEvent::HoldReleased
//!   `fake:<text>`      -> push <text> directly into the transcript channel
//!                         (lets tests exercise the paste path without speech)
//!   `paste_last`       -> ask the output worker to replay the last saved paste
//!   `about`            -> open the About window (UI testing without the tray)
//!   `quit`             -> sets the shutdown flag on the App
//!
//! On bind, the chosen port is written to `<exe_dir>/quickdictate-dev-port.txt`
//! so a test harness can discover it without hard-coding.

use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crossbeam_channel::Sender;

use crate::hotkeys::HotkeyEvent;
use crate::state::App;

const ENV_PORT: &str = "QUICKDICTATE_DEV_PORT";

/// A parsed control-channel command, decoupled from dispatch (which needs the
/// app handles / channels this module is wired to).
#[derive(Debug, PartialEq)]
enum Command {
    Toggle,
    ToggleLong,
    HoldPress,
    HoldRelease,
    PasteLast,
    /// `paste_history:<n>` with a valid index.
    PasteHistory(usize),
    About,
    Settings,
    /// `fake:<text>` with the text after the prefix.
    Fake(String),
    Quit,
    /// `paste_history:<n>` where `<n>` didn't parse as a `usize`.
    BadPasteHistory,
    Unknown,
}

/// Parse a raw command line (already trimmed) into a [`Command`]. Pure — no
/// I/O, no channel sends — so the dispatch loop can stay a thin match over it.
fn parse_command(cmd: &str) -> Command {
    match cmd {
        "toggle" => Command::Toggle,
        "toggle_long" => Command::ToggleLong,
        "hold_press" => Command::HoldPress,
        "hold_release" => Command::HoldRelease,
        "paste_last" => Command::PasteLast,
        "about" => Command::About,
        "settings" => Command::Settings,
        "quit" => Command::Quit,
        c if c.starts_with("paste_history:") => {
            match c.trim_start_matches("paste_history:").parse::<usize>() {
                Ok(i) => Command::PasteHistory(i),
                Err(_) => Command::BadPasteHistory,
            }
        }
        c if c.starts_with("fake:") => Command::Fake(c.trim_start_matches("fake:").to_string()),
        _ => Command::Unknown,
    }
}

/// The dev-trigger port file's path given the exe's own directory.
fn port_file_path_in(dir: &Path) -> PathBuf {
    dir.join("quickdictate-dev-port.txt")
}

#[allow(
    clippy::expect_used,
    reason = "a thread that cannot be spawned is unrecoverable; the panic message is the only diagnostic there is"
)]
pub fn maybe_spawn(app: Arc<App>, tx: Sender<HotkeyEvent>) -> Option<std::thread::JoinHandle<()>> {
    let port_str = std::env::var(ENV_PORT).ok()?;
    let port: u16 = port_str.trim().parse().ok()?;
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().ok()?;
    let socket = match UdpSocket::bind(addr) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("dev_trigger: bind {addr} failed: {e}");
            return None;
        }
    };
    let local = socket.local_addr().ok();
    if let Some(addr) = local {
        tracing::info!("dev_trigger: listening on {addr}");
        if let Some(path) = port_file_path() {
            let _ = std::fs::write(&path, format!("{}\n", addr.port()));
            tracing::info!("dev_trigger: wrote port to {}", path.display());
        }
    }
    Some(
        std::thread::Builder::new()
            .name("qd-dev-trigger".into())
            .spawn(move || run(app, tx, socket))
            .expect("spawn dev_trigger"),
    )
}

fn port_file_path() -> Option<PathBuf> {
    Some(port_file_path_in(&crate::paths::data_dir()))
}

fn run(app: Arc<App>, tx: Sender<HotkeyEvent>, socket: UdpSocket) {
    socket
        .set_read_timeout(Some(std::time::Duration::from_millis(250)))
        .ok();
    let mut buf = [0u8; 256];
    while !app.shutdown.load(Ordering::Acquire) {
        let n = match socket.recv(&mut buf) {
            Ok(n) => n,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                tracing::warn!("dev_trigger: recv error: {e}");
                continue;
            }
        };
        let cmd = std::str::from_utf8(&buf[..n]).unwrap_or("").trim();
        // `fake:<text>` embeds dictated-looking text straight in the command;
        // only echo it verbatim when the user has opted into full-text
        // transcript logging, same as every other transcript log site.
        if cmd.starts_with("fake:") && !app.config.load().log_transcripts {
            tracing::info!(
                "dev_trigger: received 'fake:' command ({} char(s))",
                cmd.len() - "fake:".len()
            );
        } else {
            tracing::info!("dev_trigger: received '{cmd}'");
        }
        match parse_command(cmd) {
            Command::Toggle => {
                let _ = tx.send(HotkeyEvent::TogglePressed);
            }
            Command::ToggleLong => {
                let _ = tx.send(HotkeyEvent::ToggleLongPressed);
            }
            Command::HoldPress => {
                let _ = tx.send(HotkeyEvent::HoldPressed);
            }
            Command::HoldRelease => {
                let _ = tx.send(HotkeyEvent::HoldReleased);
            }
            Command::PasteLast => {
                let _ = app.replay_tx.send(None);
            }
            Command::PasteHistory(i) => {
                // Test hook for the "Recent transcriptions" tray submenu:
                // replay history entry N (0 = most recent) without clicking.
                let _ = app.replay_tx.send(Some(i));
            }
            Command::BadPasteHistory => {
                tracing::warn!("dev_trigger: bad paste_history index in '{cmd}'");
            }
            Command::About => {
                // Test hook: open the About window without clicking the tray.
                crate::about::show_about();
            }
            Command::Settings => {
                // Test hook: open the Settings window without clicking the tray.
                crate::settings_ui::show_settings(Arc::clone(&app));
            }
            Command::Fake(text) => {
                tracing::info!(
                    "dev_trigger: injecting fake transcript ({} chars)",
                    text.chars().count()
                );
                let _ = app.transcript_tx.send(text);
            }
            Command::Quit => {
                app.shutdown.store(true, Ordering::Release);
                break;
            }
            Command::Unknown => tracing::warn!("dev_trigger: unknown command '{cmd}'"),
        }
    }
    if let Some(path) = port_file_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_recognizes_the_fixed_commands() {
        assert_eq!(parse_command("toggle"), Command::Toggle);
        assert_eq!(parse_command("toggle_long"), Command::ToggleLong);
        assert_eq!(parse_command("hold_press"), Command::HoldPress);
        assert_eq!(parse_command("hold_release"), Command::HoldRelease);
        assert_eq!(parse_command("paste_last"), Command::PasteLast);
        assert_eq!(parse_command("about"), Command::About);
        assert_eq!(parse_command("settings"), Command::Settings);
        assert_eq!(parse_command("quit"), Command::Quit);
    }

    #[test]
    fn parse_command_parses_a_valid_paste_history_index() {
        assert_eq!(parse_command("paste_history:3"), Command::PasteHistory(3));
        assert_eq!(parse_command("paste_history:0"), Command::PasteHistory(0));
    }

    #[test]
    fn parse_command_rejects_a_malformed_paste_history_index() {
        assert_eq!(parse_command("paste_history:abc"), Command::BadPasteHistory);
        assert_eq!(parse_command("paste_history:"), Command::BadPasteHistory);
        assert_eq!(parse_command("paste_history:-1"), Command::BadPasteHistory);
    }

    #[test]
    fn parse_command_extracts_the_fake_transcript_text() {
        assert_eq!(
            parse_command("fake:hello world"),
            Command::Fake("hello world".to_string())
        );
    }

    #[test]
    fn parse_command_of_bare_fake_prefix_is_empty_text() {
        assert_eq!(parse_command("fake:"), Command::Fake(String::new()));
    }

    #[test]
    fn parse_command_of_unrecognized_text_is_unknown() {
        assert_eq!(parse_command("banana"), Command::Unknown);
        assert_eq!(parse_command(""), Command::Unknown);
    }

    #[test]
    fn port_file_path_in_joins_the_dev_port_filename() {
        let dir = Path::new("C:\\some\\dir");
        assert_eq!(
            port_file_path_in(dir),
            PathBuf::from("C:\\some\\dir\\quickdictate-dev-port.txt")
        );
    }
}
