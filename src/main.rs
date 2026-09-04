#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// A release build has no console (`windows_subsystem = "windows"` above), so a
// panic on a background thread writes to a stderr that goes nowhere: dictation
// just stops, with no error and nothing on screen. `.unwrap()`/`.expect()` are
// therefore SILENT failure here, not loud ones, and are linted crate-wide.
// Tests are exempt via clippy.toml (an unwrap there IS the assertion). A
// genuinely infallible site takes a local `#[allow(..., reason = "...")]`,
// where the reason string is the argument for why it cannot fire.
#![warn(clippy::unwrap_used, clippy::expect_used)]

mod about;
mod audio;
mod autostart;
mod config;
mod dev_trigger;
/// An occasional, cadence-gated "how's it going?" feedback prompt - its own small state machine,
/// adapted from the same pattern `nudge_engine.rs` uses for the sign-in ask. See its module doc
/// for why this is not a `Campaign` added to the vendored engine.
mod feedback_survey;
mod focus;
/// Mutation fuzzing of the untrusted-input parsers, wired in as ordinary tests
/// so it runs on every `cargo test` (and therefore in CI) without a named job.
#[cfg(test)]
mod fuzz;
mod hotkeys;
mod icon;
mod keys;
mod local_stt;
mod logging;
mod mouse_hook;
/// The "you could be signed in" prompt: app glue (persistence, identity, the decision).
mod nudge;
/// The shared LunarWerx decision engine, vendored VERBATIM. Never edit it here — see `nudge.rs`.
///
/// `dead_code` is allowed for exactly that reason: the file is a byte-for-byte copy of
/// `packages/connections-connect/ports/nudge.rs`, and it carries the whole API every LunarWerx app
/// might use (`set_cadence`, the monthly cadence, the discover campaign). QuickDictate uses a
/// subset. Trimming the unused half would make the copy stop matching upstream, which is the one
/// property that lets a `diff` prove this app has not silently drifted into asking differently.
#[allow(dead_code, reason = "verbatim vendored copy; see the module doc above")]
mod nudge_engine;
mod onboarding;
mod output;
mod paths;
mod polish;
mod secretstore;
mod session_loop;
mod settings_ui;
mod sound;
mod startup;
mod state;
mod stats;
mod stt;
mod sync;
mod text;
mod theme;
mod ui;
mod update;
mod voice_commands;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::session_loop::run_event_loop;
use crate::startup::{
    bring_up_app, handle_version_flag, init_audio_pipeline, init_settings_and_logging,
    single_instance_guard,
};

fn main() -> Result<()> {
    if handle_version_flag() {
        return Ok(());
    }

    // Single-instance guard: claims a named mutex before anything else
    // (settings.json load, logging, audio, hotkeys, tray). If another
    // QuickDictate is already running, this asks it to reveal Settings and
    // exits immediately -- no audio/hotkey/tray/logging side effects at all
    // for the second launch. This is also the guaranteed way back in when
    // `hide_tray_icon` has hidden the notification-area icon: launching the
    // exe again always reaches a running instance's Settings window.
    if !single_instance_guard() {
        std::process::exit(0);
    }

    let (cfg, _log_guard) = init_settings_and_logging();
    let cfg_arc = Arc::new(cfg);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("qd-tokio")
        .build()?;
    let rt_handle = rt.handle().clone();

    let audio = init_audio_pipeline(&cfg_arc)?;
    let mut started = bring_up_app((*cfg_arc).clone(), rt_handle, Arc::clone(&audio))?;

    let active = run_event_loop(&started.app, &mut started.keys, &started.hotkeys);

    if let Some(h) = active {
        h.stop();
    }
    started.hotkeys.shutdown();
    // A replacement process waits on our owned single-instance mutex. Keep the
    // runtime alive until every physical dictation has finalized and its stats
    // write is durable, then let process exit hand the mutex to the child.
    started.app.stats.finish_sessions_and_flush();
    sync::flush_before_exit(&started.app, Duration::from_secs(6));
    audio.shutdown();
    // Give in-flight pastes a moment to finish.
    std::thread::sleep(Duration::from_millis(50));
    Ok(())
}
