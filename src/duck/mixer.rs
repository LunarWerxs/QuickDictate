//! The Windows side of ducking: every app that can make sound on an output
//! device, and its per-app volume -- the same sliders the Volume mixer shows.
//!
//! Everything here expects COM to be initialized on the calling thread; only
//! the duck worker calls it.

use std::time::Duration;

use windows::core::{Interface, PWSTR};
use windows::Win32::Foundation::{BOOL, S_OK};
use windows::Win32::Media::Audio::{
    eRender, AudioSessionStateActive, AudioSessionStateExpired, IAudioSessionControl,
    IAudioSessionControl2, IAudioSessionEnumerator, IAudioSessionManager2, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

use super::plan::{self, Change, Level};

/// The session managers of every active output device, so a duck reaches
/// the speakers and the headphones alike.
pub(super) struct Mixer {
    managers: Vec<IAudioSessionManager2>,
}

/// One app's audio session on one output device.
pub(super) struct AppSession {
    control: IAudioSessionControl,
    volume: ISimpleAudioVolume,
    /// Unique to this session of this running process.
    pub(super) instance: String,
    /// The same across launches (see `plan::Leftover::session`).
    pub(super) session: String,
}

impl Mixer {
    /// Reach the session manager of every active output device.
    pub(super) fn open() -> windows::core::Result<Self> {
        // SAFETY: COM calls on interfaces the runtime just returned, with the
        // documented argument types; each `?` stops before a failed object is
        // used. COM is initialized on this thread by the worker.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            let mut managers = Vec::new();
            for i in 0..devices.GetCount()? {
                // One device that refuses (unplugged mid-call, a driver that
                // hosts no sessions) must not cost the others their duck.
                let Ok(device) = devices.Item(i) else {
                    continue;
                };
                if let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) {
                    managers.push(manager);
                }
            }
            Ok(Self { managers })
        }
    }

    /// Every other app's session on every output device, playing or not:
    /// never QuickDictate's own, and never the Windows system sounds.
    pub(super) fn sessions(&self) -> Vec<AppSession> {
        let own_pid = std::process::id();
        let mut out = Vec::new();
        for manager in &self.managers {
            // SAFETY: as in `open`.
            let Ok(list) = (unsafe { manager.GetSessionEnumerator() }) else {
                continue;
            };
            // SAFETY: as in `open`.
            let count = unsafe { list.GetCount() }.unwrap_or(0);
            out.extend((0..count).filter_map(|i| app_session(&list, i, own_pid)));
        }
        out
    }
}

/// The session at `index`, unless it is ours or the system sounds.
fn app_session(list: &IAudioSessionEnumerator, index: i32, own_pid: u32) -> Option<AppSession> {
    // SAFETY: as in `Mixer::open`. `GetProcessId` answers with a success code
    // for a session shared by several processes, which is still `Ok`.
    unsafe {
        let control = list.GetSession(index).ok()?;
        let control2: IAudioSessionControl2 = control.cast().ok()?;
        if control2.IsSystemSoundsSession() == S_OK {
            return None;
        }
        if control2.GetProcessId().unwrap_or(0) == own_pid {
            return None;
        }
        let instance = take_string(control2.GetSessionInstanceIdentifier().ok()?);
        let session = take_string(control2.GetSessionIdentifier().ok()?);
        let volume: ISimpleAudioVolume = control.cast().ok()?;
        Some(AppSession {
            control,
            volume,
            instance,
            session,
        })
    }
}

/// Copy out a string the audio service allocated for us, and free it.
///
/// # Safety
/// `raw` must be null or a `CoTaskMemAlloc`ed, NUL-terminated UTF-16 string
/// the caller owns, as the session identifier getters return.
unsafe fn take_string(raw: PWSTR) -> String {
    if raw.is_null() {
        return String::new();
    }
    let text = raw.to_string().unwrap_or_default();
    CoTaskMemFree(Some(raw.0 as *const std::ffi::c_void));
    text
}

impl AppSession {
    /// Whether the app is playing sound right now.
    pub(super) fn is_playing(&self) -> bool {
        // SAFETY: as in `Mixer::open`.
        matches!(unsafe { self.control.GetState() }, Ok(state) if state == AudioSessionStateActive)
    }

    /// Whether the session has ended (its app closed or released the
    /// device), so changing it now would reach nothing.
    pub(super) fn is_gone(&self) -> bool {
        // SAFETY: as in `Mixer::open`.
        !matches!(unsafe { self.control.GetState() }, Ok(state) if state != AudioSessionStateExpired)
    }

    /// The app's volume and mute state right now.
    pub(super) fn level(&self) -> Option<Level> {
        // SAFETY: as in `Mixer::open`.
        unsafe {
            Some(Level {
                volume: self.volume.GetMasterVolume().ok()?,
                muted: self.volume.GetMute().ok()?.as_bool(),
            })
        }
    }

    /// Make `change` at once. `false` when the audio service refused it.
    pub(super) fn apply(&self, change: Change) -> bool {
        match change {
            Change::Mute => self.set_muted(true),
            Change::Unmute => self.set_muted(false),
            Change::Volume(volume) => self.set_volume(volume),
        }
    }

    fn set_volume(&self, volume: f32) -> bool {
        // SAFETY: as in `Mixer::open`. A null event context is documented as
        // allowed; nothing here needs to recognize its own change events.
        unsafe {
            self.volume
                .SetMasterVolume(volume.clamp(0.0, 1.0), std::ptr::null())
        }
        .is_ok()
    }

    fn set_muted(&self, muted: bool) -> bool {
        // SAFETY: as in `set_volume`.
        unsafe { self.volume.SetMute(BOOL::from(muted), std::ptr::null()) }.is_ok()
    }
}

/// One app's change, as `glide` makes it: the app, how it reads now, and the
/// change to make.
pub(super) type Move<'a> = (&'a AppSession, Level, Change);

/// Make every move as a fade over `over` instead of a jump, all apps together.
/// It ends in exactly the state `AppSession::apply` would leave, so restoring
/// and the leftovers file never need to know a fade happened: a mute still
/// keeps the slider where it was, it just gets there by way of silence, and
/// an unmute swells up from nothing to where the slider sits.
///
/// Returns, per move, whether it landed. One that fails part-way is sent to
/// its louder end (where the app started when going down, its target when
/// coming back), so a hiccup never strands an app half-quiet.
pub(super) fn glide(moves: &[Move<'_>], over: Duration) -> Vec<bool> {
    let mut ok = vec![true; moves.len()];
    let mut ramps = Vec::with_capacity(moves.len());
    for (i, (app, now, change)) in moves.iter().enumerate() {
        ramps.push(match change {
            Change::Mute => (now.volume, 0.0),
            Change::Unmute => {
                ok[i] = app.set_volume(0.0) && app.set_muted(false);
                (0.0, now.volume)
            }
            Change::Volume(target) => (now.volume, *target),
        });
    }
    let steps = plan::fade_steps(over);
    for step in 1..=steps {
        let t = step as f32 / steps as f32;
        for (i, (app, _, _)) in moves.iter().enumerate() {
            if ok[i] {
                let (from, to) = ramps[i];
                ok[i] = app.set_volume(plan::fade_level(from, to, t));
            }
        }
        if step < steps {
            std::thread::sleep(plan::FADE_STEP);
        }
    }
    for (i, (app, now, change)) in moves.iter().enumerate() {
        if ok[i] && *change == Change::Mute {
            // Silent now: mute, then put the slider back where it was.
            ok[i] = app.set_muted(true) && app.set_volume(now.volume);
        }
        if !ok[i] {
            let (from, to) = ramps[i];
            let _ = app.set_muted(false);
            let _ = app.set_volume(from.max(to));
        }
    }
    ok
}
