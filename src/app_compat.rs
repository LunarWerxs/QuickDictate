//! Versioned app-compatibility list: how to deliver text to the windows the
//! default hybrid paste does not reach.
//!
//! Some windows ignore Ctrl+V (legacy consoles, mintty, PuTTY), some see a
//! different clipboard (a remote-desktop session without clipboard
//! redirection) and some drop injected keystrokes outright (games with an
//! anti-cheat driver). `SendInput` reports success in every case, so without
//! this the user just sees nothing typed. A data list keyed on the focused
//! window's exe, window class and title picks the delivery that works there
//! and says why.
//!
//! The list ships inside the exe (`assets/app-compat.json`) and can be
//! extended or overridden without a new release by a
//! `quickdictate-app-compat.json` in the data folder, in the same shape. That
//! file is re-read whenever its modification time changes, and its entries are
//! checked before the bundled ones, so a user entry wins on any window both
//! match. A broken local file falls back to the bundled list, never to none.
//!
//! The idea (a versioned, schema-checked list of per-app compatibility
//! entries) comes from OBS Studio's win-capture compatibility list; no code
//! was taken from it.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use parking_lot::Mutex;
use serde::Deserialize;

use crate::focus;

/// File name inside the data folder. Listed in `paths::RELOCATABLE` so a
/// data-folder move carries it along.
pub const APP_COMPAT_FILE: &str = "quickdictate-app-compat.json";

/// Bumped only if the entry shape changes incompatibly. A list of any other
/// version is ignored as a whole rather than half-understood.
const FORMAT_VERSION: u32 = 1;

/// The list compiled into this build.
const BUNDLED_JSON: &str = include_str!("../assets/app-compat.json");

/// How text reaches a window an entry matches.
#[derive(Copy, Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// The normal hybrid: keystrokes for short text, clipboard paste for long.
    #[default]
    Auto,
    /// Always type Unicode keystrokes, for windows that ignore Ctrl+V.
    Keystrokes,
    /// Always paste through the clipboard, for windows that drop or mangle
    /// synthetic keystrokes but accept Ctrl+V.
    Clipboard,
    /// Inject nothing: put the text on the clipboard for the user to paste,
    /// for windows that drop every kind of injected input.
    Manual,
}

/// One list entry. Every key that is set must match; at least one must be.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct CompatEntry {
    /// Shown in logs, so a user can tell which entry decided a paste.
    pub name: String,
    /// Exe basename, case-insensitive (e.g. `"mstsc.exe"`).
    #[serde(default)]
    pub exe: Option<String>,
    /// Top-level window class, case-insensitive (e.g. `"ConsoleWindowClass"`).
    #[serde(default)]
    pub window_class: Option<String>,
    /// Case-insensitive substring of the window title.
    #[serde(default)]
    pub window_title: Option<String>,
    #[serde(default)]
    pub delivery: Delivery,
    /// Why this window needs the entry, logged once per run when it matches.
    #[serde(default)]
    pub message: Option<String>,
    /// Where to read more, logged with the message.
    #[serde(default)]
    pub url: Option<String>,
}

impl CompatEntry {
    fn is_keyed(&self) -> bool {
        self.exe.is_some() || self.window_class.is_some() || self.window_title.is_some()
    }

    fn matches(&self, window: &WindowFacts) -> bool {
        let exe_ok = self.exe.as_deref().is_none_or(|want| {
            window
                .exe
                .as_deref()
                .is_some_and(|have| have.eq_ignore_ascii_case(want))
        });
        let class_ok = self.window_class.as_deref().is_none_or(|want| {
            window
                .class
                .as_deref()
                .is_some_and(|have| have.eq_ignore_ascii_case(want))
        });
        let title_ok = self.window_title.as_deref().is_none_or(|want| {
            window
                .title
                .as_deref()
                .is_some_and(|have| have.to_lowercase().contains(&want.to_lowercase()))
        });
        self.is_keyed() && exe_ok && class_ok && title_ok
    }
}

#[derive(Deserialize)]
struct CompatFile {
    version: u32,
    /// The list's own revision, bumped on every content change and logged so
    /// a report says which list a paste was decided by.
    #[serde(default)]
    revision: u32,
    entries: Vec<CompatEntry>,
}

/// A parsed list, keyless entries already dropped.
#[derive(Debug, Default)]
pub struct CompatList {
    pub revision: u32,
    entries: Vec<CompatEntry>,
}

impl CompatList {
    pub fn parse(json: &str) -> Result<Self, String> {
        let file: CompatFile = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if file.version != FORMAT_VERSION {
            return Err(format!(
                "format version {} is not the supported {FORMAT_VERSION}",
                file.version
            ));
        }
        // An entry with no exe, class or title would match every window and
        // quietly change how all text is delivered: drop it, loudly.
        let entries = file
            .entries
            .into_iter()
            .filter(|entry| {
                if !entry.is_keyed() {
                    tracing::warn!(
                        "app-compat: entry {:?} names no exe, class or title; ignored",
                        entry.name
                    );
                }
                entry.is_keyed()
            })
            .collect();
        Ok(Self {
            revision: file.revision,
            entries,
        })
    }

    fn find(&self, window: &WindowFacts) -> Option<&CompatEntry> {
        self.entries.iter().find(|entry| entry.matches(window))
    }
}

/// What the focused window looks like to the matcher.
#[derive(Clone, Debug, Default)]
pub struct WindowFacts {
    pub exe: Option<String>,
    pub class: Option<String>,
    pub title: Option<String>,
}

impl WindowFacts {
    pub fn current() -> Self {
        Self {
            exe: focus::foreground_exe_name(),
            class: focus::foreground_window_class(),
            title: focus::foreground_window_title(),
        }
    }
}

/// The local list first, then the bundled one.
fn find_in<'a>(
    local: Option<&'a CompatList>,
    bundled: &'a CompatList,
    window: &WindowFacts,
) -> Option<&'a CompatEntry> {
    local
        .and_then(|list| list.find(window))
        .or_else(|| bundled.find(window))
}

struct LocalCache {
    checked: bool,
    stamp: Option<SystemTime>,
    list: Option<CompatList>,
}

static BUNDLED: Mutex<Option<CompatList>> = Mutex::new(None);
static LOCAL: Mutex<LocalCache> = Mutex::new(LocalCache {
    checked: false,
    stamp: None,
    list: None,
});
/// Entry names whose message has already been logged this run.
static ANNOUNCED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn read_local(path: &Path) -> Option<CompatList> {
    let json = fs::read_to_string(path).ok()?;
    match CompatList::parse(&json) {
        Ok(list) => {
            tracing::info!(
                "app-compat: loaded {} local entr(ies), revision {}",
                list.entries.len(),
                list.revision
            );
            Some(list)
        }
        Err(e) => {
            tracing::warn!("app-compat: {APP_COMPAT_FILE} ignored, using the bundled list: {e}");
            None
        }
    }
}

/// The entry that decides how text reaches `window`, if any. Costs one file
/// stat per call; the local file is only re-read when it changed.
pub fn lookup(window: &WindowFacts) -> Option<CompatEntry> {
    let path = crate::paths::data_file(APP_COMPAT_FILE);
    let stamp = fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut local = LOCAL.lock();
    if !local.checked || local.stamp != stamp {
        local.list = stamp.and_then(|_| read_local(&path));
        local.stamp = stamp;
        local.checked = true;
    }
    let mut bundled = BUNDLED.lock();
    let bundled = bundled.get_or_insert_with(|| {
        CompatList::parse(BUNDLED_JSON).unwrap_or_else(|e| {
            tracing::error!("app-compat: the bundled list does not parse: {e}");
            CompatList::default()
        })
    });
    find_in(local.list.as_ref(), bundled, window).cloned()
}

/// Log why `entry` changed a paste: its message and link once per run at
/// warn (it is news the first time), every later match at debug.
pub fn announce(entry: &CompatEntry) {
    let first = {
        let mut seen = ANNOUNCED.lock();
        let seen = seen.get_or_insert_with(HashSet::new);
        seen.insert(entry.name.clone())
    };
    let (name, delivery) = (&entry.name, entry.delivery);
    if !first {
        tracing::debug!("app-compat: {name} matched, delivery {delivery:?}");
        return;
    }
    let why = match &entry.message {
        Some(message) => message.as_str(),
        None => "listed in the app-compatibility list",
    };
    let link = match &entry.url {
        Some(url) => format!(" (more: {url})"),
        None => String::new(),
    };
    tracing::warn!("app-compat: {name} matched, delivery {delivery:?}: {why}{link}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(exe: &str, class: &str, title: &str) -> WindowFacts {
        WindowFacts {
            exe: Some(exe.to_string()),
            class: Some(class.to_string()),
            title: Some(title.to_string()),
        }
    }

    /// The shipped asset is only checked at runtime, where a typo would
    /// silently disable every entry behind one log line.
    #[test]
    fn bundled_list_parses_and_every_entry_is_keyed() {
        let list = CompatList::parse(BUNDLED_JSON).expect("bundled app-compat.json parses");
        assert!(!list.entries.is_empty());
        assert!(list.entries.iter().all(CompatEntry::is_keyed));
    }

    #[test]
    fn every_set_key_must_match_and_a_keyless_entry_is_dropped() {
        let list = CompatList::parse(
            r#"{"version":1,"entries":[
                {"name":"none","delivery":"manual"},
                {"name":"game","exe":"Game.exe","window_title":"ranked","delivery":"manual"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(list.entries.len(), 1, "the keyless entry must be dropped");
        let hit = list.find(&window("game.exe", "UnityWndClass", "Game - RANKED match"));
        assert_eq!(hit.map(|e| e.delivery), Some(Delivery::Manual));
        assert!(list
            .find(&window("game.exe", "UnityWndClass", "Game - menu"))
            .is_none());
        assert!(list
            .find(&window("other.exe", "UnityWndClass", "ranked"))
            .is_none());
    }

    #[test]
    fn a_local_entry_wins_over_the_bundled_one() {
        let bundled = CompatList::parse(
            r#"{"version":1,"entries":[
                {"name":"b","window_class":"PuTTY","delivery":"keystrokes"}
            ]}"#,
        )
        .unwrap();
        let local = CompatList::parse(
            r#"{"version":1,"entries":[
                {"name":"l","window_class":"putty","delivery":"clipboard"}
            ]}"#,
        )
        .unwrap();
        let putty = window("putty.exe", "PuTTY", "host");
        let winner = |local: Option<&CompatList>| {
            find_in(local, &bundled, &putty).map(|entry| entry.name.clone())
        };
        assert_eq!(winner(Some(&local)).as_deref(), Some("l"));
        assert_eq!(winner(None).as_deref(), Some("b"));
    }

    #[test]
    fn another_format_version_is_refused_whole() {
        assert!(CompatList::parse(r#"{"version":2,"entries":[]}"#).is_err());
    }
}
