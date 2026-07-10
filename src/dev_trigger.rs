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
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crossbeam_channel::Sender;

use crate::hotkeys::HotkeyEvent;
use crate::state::App;

const ENV_PORT: &str = "QUICKDICTATE_DEV_PORT";

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
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .map(|d| d.join("quickdictate-dev-port.txt"))
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
        match cmd {
            "toggle" => {
                let _ = tx.send(HotkeyEvent::TogglePressed);
            }
            "toggle_long" => {
                let _ = tx.send(HotkeyEvent::ToggleLongPressed);
            }
            "hold_press" => {
                let _ = tx.send(HotkeyEvent::HoldPressed);
            }
            "hold_release" => {
                let _ = tx.send(HotkeyEvent::HoldReleased);
            }
            "paste_last" => {
                let _ = app.replay_tx.send(None);
            }
            c if c.starts_with("paste_history:") => {
                // Test hook for the "Recent transcriptions" tray submenu:
                // replay history entry N (0 = most recent) without clicking.
                match c.trim_start_matches("paste_history:").parse::<usize>() {
                    Ok(i) => {
                        let _ = app.replay_tx.send(Some(i));
                    }
                    Err(_) => tracing::warn!("dev_trigger: bad paste_history index in '{c}'"),
                }
            }
            "about" => {
                // Test hook: open the About window without clicking the tray.
                crate::about::show_about();
            }
            "settings" => {
                // Test hook: open the Settings window without clicking the tray.
                crate::settings_ui::show_settings(Arc::clone(&app));
            }
            c if c.starts_with("fake:") => {
                let text = c.trim_start_matches("fake:").to_string();
                tracing::info!(
                    "dev_trigger: injecting fake transcript ({} chars)",
                    text.chars().count()
                );
                let _ = app.transcript_tx.send(text);
            }
            "quit" => {
                app.shutdown.store(true, Ordering::Release);
                break;
            }
            other => tracing::warn!("dev_trigger: unknown command '{other}'"),
        }
    }
    if let Some(path) = port_file_path() {
        let _ = std::fs::remove_file(path);
    }
}
