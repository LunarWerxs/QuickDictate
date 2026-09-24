//! The one thread that talks to the Windows audio service for ducking, so a
//! press only ever posts a command and never waits on it.

use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::mixer::{AppSession, Mixer};
use super::plan::{self, Leftover, Level, LEFTOVERS_FILE};

/// While ducked, how often to look for apps that started playing after the
/// press began (a video that autoplays, a song someone hits play on), so they
/// are quieted too instead of coming in over the dictation.
const RESCAN: Duration = Duration::from_secs(1);

pub(super) enum Cmd {
    /// Quiet every app playing sound, to `percent` of its volume (0 = mute).
    Duck { percent: u8 },
    /// Put back everything the current duck changed.
    Restore,
    /// Put back whatever an earlier run left ducked.
    Recover,
    /// Restore, acknowledge, and stop.
    Shutdown(Sender<()>),
}

pub(super) fn spawn() -> Sender<Cmd> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let spawned = std::thread::Builder::new()
        .name("qd-duck".into())
        .spawn(move || run(&rx));
    if let Err(e) = spawned {
        tracing::warn!(
            "duck: could not start its worker thread ({e}); other apps will not be quieted"
        );
    }
    tx
}

fn run(rx: &Receiver<Cmd>) {
    // SAFETY: FFI call with no out-parameters, made once on this thread
    // before any other COM use. A multithreaded apartment suits the audio
    // session APIs and needs no message pump.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut worker = Worker::load();
    loop {
        let cmd = if worker.ducked.is_some() {
            match rx.recv_timeout(RESCAN) {
                Ok(cmd) => cmd,
                Err(RecvTimeoutError::Timeout) => {
                    worker.rescan();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => break,
            }
        };
        match cmd {
            Cmd::Duck { percent } => worker.duck(percent),
            Cmd::Restore => worker.restore(),
            Cmd::Recover => worker.recover(),
            Cmd::Shutdown(ack) => {
                worker.restore();
                let _ = ack.send(());
                return;
            }
        }
    }
    worker.restore();
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// "muted" or "lowered to 20%", for the log.
fn verb(percent: u8) -> String {
    if percent == 0 {
        "muted".into()
    } else {
        format!("lowered to {percent}%")
    }
}

struct Worker {
    /// Apps an earlier duck left down and could not yet put back (see
    /// `plan::Leftover`), mirrored to [`LEFTOVERS_FILE`].
    leftovers: Vec<Leftover>,
    ducked: Option<Ducked>,
    /// Whether [`LEFTOVERS_FILE`] exists, so an idle press end costs no disk
    /// access at all.
    on_disk: bool,
}

/// The duck in force for the current press(es).
struct Ducked {
    percent: u8,
    mixer: Mixer,
    apps: Vec<DuckedApp>,
    /// Instances already decided on while playing (ducked, or left alone as
    /// already muted), so a rescan only ever acts on newcomers.
    seen: HashSet<String>,
}

struct DuckedApp {
    app: AppSession,
    original: Level,
    set: Level,
    at_ms: u64,
}

impl DuckedApp {
    fn leftover(&self) -> Leftover {
        Leftover {
            session: self.app.session.clone(),
            original: self.original,
            set: self.set,
            at_ms: self.at_ms,
        }
    }
}

impl Ducked {
    /// Duck every app playing sound that this press has not decided on yet.
    /// Returns the names of the ones it quieted.
    fn sweep(&mut self) -> Vec<String> {
        let mut quieted = Vec::new();
        for app in self.mixer.sessions() {
            // Not-yet-playing apps stay undecided, so a later rescan still
            // catches them if they start mid-dictation.
            if self.seen.contains(&app.instance) || !app.is_playing() {
                continue;
            }
            self.seen.insert(app.instance.clone());
            let Some(found) = app.level() else {
                continue;
            };
            let Some(change) = plan::duck(found, self.percent) else {
                continue;
            };
            if app.apply(change) {
                quieted.push(plan::app_name(&app.session).to_string());
                self.apps.push(DuckedApp {
                    original: found,
                    set: found.after(change),
                    at_ms: now_ms(),
                    app,
                });
            }
        }
        quieted
    }
}

impl Worker {
    fn load() -> Self {
        let path = crate::paths::data_file(LEFTOVERS_FILE);
        let text = std::fs::read_to_string(&path).ok();
        let leftovers = text
            .as_deref()
            .map(|t| plan::parse_leftovers(t, now_ms()))
            .unwrap_or_default();
        Self {
            leftovers,
            ducked: None,
            on_disk: text.is_some(),
        }
    }

    fn duck(&mut self, percent: u8) {
        if self.ducked.is_some() {
            return;
        }
        // An app a crash left quiet has to be put back first, or this duck
        // would record the quiet volume as the one to return to.
        self.recover();
        let started = Instant::now();
        let mixer = match Mixer::open() {
            Ok(mixer) => mixer,
            Err(e) => {
                tracing::warn!(
                    "duck: could not reach the Windows audio service ({e}); leaving other apps alone"
                );
                return;
            }
        };
        let mut ducked = Ducked {
            percent,
            mixer,
            apps: Vec::new(),
            seen: HashSet::new(),
        };
        let quieted = ducked.sweep();
        tracing::info!(
            "duck: {} {} app(s) in {} ms{}",
            verb(percent),
            quieted.len(),
            started.elapsed().as_millis(),
            if quieted.is_empty() {
                String::new()
            } else {
                format!(" ({})", quieted.join(", "))
            }
        );
        self.ducked = Some(ducked);
        if !quieted.is_empty() {
            self.persist();
        }
    }

    fn rescan(&mut self) {
        let Some(ducked) = self.ducked.as_mut() else {
            return;
        };
        let quieted = ducked.sweep();
        if !quieted.is_empty() {
            tracing::info!(
                "duck: {} {} more app(s) that started mid-dictation ({})",
                verb(ducked.percent),
                quieted.len(),
                quieted.join(", ")
            );
            self.persist();
        }
    }

    fn restore(&mut self) {
        let Some(ducked) = self.ducked.take() else {
            return;
        };
        let (mut restored, mut kept, mut unreachable) = (0usize, 0usize, 0usize);
        for app in &ducked.apps {
            // Closed while it was ducked: Windows kept the lowered volume for
            // that app's next launch, so it becomes a leftover `recover` puts
            // right once the app is back.
            let now = if app.app.is_gone() {
                None
            } else {
                app.app.level()
            };
            let Some(now) = now else {
                plan::upsert(&mut self.leftovers, app.leftover());
                unreachable += 1;
                continue;
            };
            match plan::restore(app.original, app.set, now) {
                Some(change) if app.app.apply(change) => restored += 1,
                Some(_) => {
                    plan::upsert(&mut self.leftovers, app.leftover());
                    unreachable += 1;
                }
                None => kept += 1,
            }
        }
        if !ducked.apps.is_empty() {
            tracing::info!(
                "duck: restored {restored} app(s); {kept} changed by you mid-dictation and left as \
                 they are; {unreachable} gone, to be restored when they return"
            );
        }
        self.persist();
    }

    /// Put back any leftover whose app is running again, if it still reads
    /// exactly as the duck left it. Leftovers whose app is not running stay
    /// on the list for the next try.
    fn recover(&mut self) {
        if self.leftovers.is_empty() {
            if self.on_disk && self.ducked.is_none() {
                self.persist(); // an unreadable or all-stale file: clear it
            }
            return;
        }
        let Ok(mixer) = Mixer::open() else {
            return;
        };
        let sessions = mixer.sessions();
        let before = self.leftovers.len();
        let mut restored = Vec::new();
        self.leftovers.retain(|left| {
            let running: Vec<&AppSession> = sessions
                .iter()
                .filter(|s| s.session == left.session && !s.is_gone())
                .collect();
            if running.is_empty() {
                return true;
            }
            for app in running {
                let change = app
                    .level()
                    .and_then(|now| plan::restore(left.original, left.set, now));
                if change.is_some_and(|c| app.apply(c)) {
                    restored.push(plan::app_name(&left.session).to_string());
                }
            }
            false
        });
        if !restored.is_empty() {
            tracing::info!(
                "duck: put back {} app(s) an earlier run left quieted ({})",
                restored.len(),
                restored.join(", ")
            );
        }
        if self.leftovers.len() != before {
            self.persist();
        }
    }

    /// Mirror the leftovers, plus everything ducked right now, to disk; or
    /// delete the file when there is nothing to put back.
    fn persist(&mut self) {
        let mut all = self.leftovers.clone();
        if let Some(ducked) = &self.ducked {
            for app in &ducked.apps {
                plan::upsert(&mut all, app.leftover());
            }
        }
        let path = crate::paths::data_file(LEFTOVERS_FILE);
        if all.is_empty() {
            if self.on_disk {
                match std::fs::remove_file(&path) {
                    Ok(()) => self.on_disk = false,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.on_disk = false,
                    Err(e) => tracing::warn!("duck: could not remove {}: {e}", path.display()),
                }
            }
            return;
        }
        match std::fs::write(&path, plan::leftovers_json(&all)) {
            Ok(()) => self.on_disk = true,
            Err(e) => tracing::warn!(
                "duck: could not record the quieted apps in {} ({e}); a crash now would leave \
                 them quiet",
                path.display()
            ),
        }
    }
}
