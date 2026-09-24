//! Quieting other apps while you dictate ("ducking").
//!
//! Opt-in (`Config::duck_other_audio`). When a press starts, every other app
//! that is playing sound -- music, a video, a call -- drops to
//! `duck_volume_percent` of its own volume, or is muted at 0, and the moment
//! the microphone stops listening each one goes back to where it was. It is
//! the per-app volume the Windows Volume mixer shows, so the system volume and
//! QuickDictate's own start/stop sounds are never touched. With
//! `Config::duck_fade` on (the default) both changes are quick fades rather
//! than jumps; either way they end in exactly the same state.
//!
//! Three rules keep it from ever doing more than the user asked for:
//!
//!   * **Only what it changed goes back, and only if it is still as it was
//!     left.** Move an app's slider or unmute it mid-dictation and your change
//!     wins: restoring skips that app (see [`plan::restore`]).
//!   * **Overlapping presses share one duck.** A new press can start while the
//!     previous one is still in its listening tail; the first press to start
//!     ducks and only the last one to stop listening restores, so the music
//!     never pops back up in the middle of a dictation.
//!   * **A hard exit cannot leave your music quiet for good.** Windows
//!     remembers each app's volume across launches, so every duck is written to
//!     `quickdictate-ducked-apps.json` before a press goes on; the next launch
//!     (and the next press) puts back whatever a crash or a killed process left
//!     down. The same list catches an app that closed while it was ducked, and
//!     reopens later at the lowered volume Windows stored for it.
//!
//! None of it runs on the hotkey thread: [`begin`] only counts the press and
//! posts a command, and one worker thread makes every call into the audio
//! service, so a press never waits on it.
//!
//! Layout of this module:
//!   * `mod.rs` -- the public API ([`begin`] and its [`DuckGuard`],
//!     [`recover_after_crash`], [`shutdown`]) and the press counter.
//!   * `plan.rs` -- the pure decisions (what to set, whether to put back) and
//!     the leftovers file format.
//!   * `mixer.rs` -- the Windows audio-session calls.
//!   * `worker.rs` -- the thread that makes them.

mod mixer;
mod plan;
mod worker;

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::Sender;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;

use crate::config::Config;
pub(crate) use plan::LEFTOVERS_FILE;
use worker::Cmd;

/// Presses currently holding a [`DuckGuard`].
static PRESSES: Mutex<Presses> = Mutex::new(Presses { live: 0 });
/// The worker's command queue, started by the first command that needs it.
static WORKER: OnceCell<Sender<Cmd>> = OnceCell::new();
/// Set by [`shutdown`]: nothing may duck after the final restore.
static SHUT_DOWN: AtomicBool = AtomicBool::new(false);

/// The press counter behind [`begin`] and [`DuckGuard`]'s drop, so that only
/// the first of several overlapping presses ducks and only the last one to
/// stop listening restores.
#[derive(Debug, Default)]
struct Presses {
    live: usize,
}

impl Presses {
    /// A press started. `true` when it is the one that should duck.
    fn start(&mut self) -> bool {
        self.live += 1;
        self.live == 1
    }

    /// A press stopped listening. `true` when it was the last one, so the
    /// other apps should come back up.
    fn finish(&mut self) -> bool {
        if self.live == 0 {
            return false;
        }
        self.live -= 1;
        self.live == 0
    }
}

/// Post a command to the worker, starting it on first use.
fn send(cmd: Cmd) {
    let tx = WORKER.get_or_init(worker::spawn);
    // Fails only once the worker has exited (after `shutdown`) or never
    // started (already logged by `spawn`); either way there is nothing to do.
    let _ = tx.send(cmd);
}

/// Held for as long as one press is listening. Dropping it lets the other
/// apps back up once no other press still holds one.
#[must_use = "other apps come back up the moment this is dropped"]
pub(crate) struct DuckGuard {
    _private: (),
}

impl Drop for DuckGuard {
    fn drop(&mut self) {
        let mut presses = PRESSES.lock();
        // Sent under the lock, so the worker sees duck/restore in the same
        // order the counter changed.
        if presses.finish() && !SHUT_DOWN.load(Ordering::Acquire) {
            send(Cmd::Restore);
        }
    }
}

/// A press is starting: quiet the other apps if the user asked for that.
/// `None` when ducking is off. Never blocks on the audio service.
pub(crate) fn begin(cfg: &Config) -> Option<DuckGuard> {
    if !cfg.duck_other_audio || SHUT_DOWN.load(Ordering::Acquire) {
        return None;
    }
    let mut presses = PRESSES.lock();
    if presses.start() {
        send(Cmd::Duck {
            percent: cfg.duck_volume_percent.min(100),
            fade: cfg.duck_fade,
        });
    }
    Some(DuckGuard { _private: () })
}

/// At startup: put back any app a previous run ducked and never got to
/// restore. Starts the worker only when there is such a list on disk, and
/// runs whether or not ducking is still switched on.
pub(crate) fn recover_after_crash() {
    if crate::paths::data_file(LEFTOVERS_FILE).exists() {
        send(Cmd::Recover);
    }
}

/// On the way out: restore everything still ducked, then stop the worker.
/// Waits at most `wait`, so a hung audio service cannot hold up the exit.
pub(crate) fn shutdown(wait: Duration) {
    SHUT_DOWN.store(true, Ordering::Release);
    let Some(tx) = WORKER.get() else {
        return; // never started, so nothing was ever ducked
    };
    let (ack_tx, ack_rx) = crossbeam_channel::bounded(1);
    if tx.send(Cmd::Shutdown(ack_tx)).is_ok() && ack_rx.recv_timeout(wait).is_err() {
        tracing::warn!("duck: other apps were not confirmed restored within {wait:?} of exit");
    }
}
