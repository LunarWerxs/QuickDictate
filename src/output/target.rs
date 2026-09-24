//! Where each paste landed, so "scratch that" only ever backspaces over text
//! QuickDictate itself typed into the field that still has focus.

use parking_lot::Mutex;

use crate::focus;

/// How many pastes an undo can reach back through. Matches the transcript
/// history's cap: a paste whose entry has aged out of history can never be the
/// most recent one again, so its target could never be used.
const MAX_UNDO_TARGETS: usize = 50;

/// Where one paste landed. Two captures are equal only when the foreground
/// window, the control focused inside it and the owning exe all match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PasteTarget {
    pub(super) window: isize,
    pub(super) focus: isize,
    pub(super) exe: Option<String>,
}

impl PasteTarget {
    /// Where keyboard input is going right now, or `None` when there is no
    /// foreground window to type into.
    pub(super) fn current() -> Option<Self> {
        let (window, focus) = focus::foreground_focus_ids()?;
        Some(Self {
            window,
            focus,
            exe: focus::foreground_exe_name(),
        })
    }
}

/// The pastes "scratch that" may still undo, oldest first, each keyed by the
/// id of the history entry that paste recorded.
///
/// A stack keyed by entry, rather than one "last target" slot, because the
/// undo backspaces the length of whichever history entry is most recent NOW.
/// The single slot went on describing a paste that had already been undone
/// (or a replay that recorded no entry at all), so a second "scratch that"
/// passed the focus check for that window and then deleted the length of an
/// older entry that was typed somewhere else, or never typed at all.
pub(super) struct UndoTargets(Vec<(u64, PasteTarget)>);

impl UndoTargets {
    pub(super) const fn new() -> Self {
        Self(Vec::new())
    }

    /// Make `entry_id`'s paste the one the next "scratch that" undoes.
    pub(super) fn push(&mut self, entry_id: u64, target: PasteTarget) {
        if self.0.len() >= MAX_UNDO_TARGETS {
            self.0.remove(0);
        }
        self.0.push((entry_id, target));
    }

    /// Where `entry_id` was pasted, but only while it is still the newest
    /// paste: anything typed after it would make its length the wrong number
    /// of backspaces.
    pub(super) fn target_for(&self, entry_id: u64) -> Option<&PasteTarget> {
        self.0
            .last()
            .filter(|(id, _)| *id == entry_id)
            .map(|(_, target)| target)
    }

    /// Drop the newest paste once it has been undone, so the one before it
    /// becomes undoable in turn.
    pub(super) fn pop(&mut self) {
        self.0.pop();
    }

    /// Forget every paste: text of unknown length now sits after all of them.
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }
}

/// Read by [`handle_scratch_that`](super::worker::handle_scratch_that), kept
/// up to date by [`paste_processed`](super::worker::paste_processed).
pub(super) static UNDO_TARGETS: Mutex<UndoTargets> = Mutex::new(UndoTargets::new());
