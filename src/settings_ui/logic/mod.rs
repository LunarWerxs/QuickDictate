//! Settings-window state: construction, validation, saving, the
//! Connections sync handshake, hotkey capture, and the key-test run.
//! Nothing here draws; the drawing lives in the sibling card modules.
//!
//! ## Layout of this module
//! Every file here adds methods to the one [`SettingsApp`] type, grouped by
//! the job they do rather than by when they run, so a change to (say) saving
//! does not mean scrolling past the sync handshake to find it:
//!
//! - [`draft`]: building the app, reseeding it on reopen, the vocabulary
//!   scratch buffers, the dirty check, and validation.
//! - [`sync_ops`]: the Connections handshake — spawning a worker, draining
//!   its events, and applying a successful sign-in.
//! - [`hotkey`]: capturing the next keypress or mouse button into a hotkey
//!   field while it is recording.
//! - [`save`]: plain save, save-and-sync, reset-to-defaults, and the deferred
//!   save-and-restart.
//! - [`keytest`]: the parallel provider key probe and its verdicts.
//! - [`screenshot`]: the headless `QUICKDICTATE_UI_SHOT` capture hook.
//!
//! These are inherent `impl` blocks, so nothing needs re-exporting: the
//! methods are visible wherever `SettingsApp` is.

mod draft;
mod hotkey;
mod keytest;
mod save;
mod screenshot;
mod sync_ops;
