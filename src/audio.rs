//! Global microphone capture. The WASAPI stream is opened **once** at startup
//! on a dedicated thread (cpal `Stream` is not `Send`) and stays alive for the
//! app's lifetime. Sessions subscribe to get a dedicated resampler feed,
//! eliminating per-session mic initialization latency.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use tokio::sync::mpsc;

/// Default streaming target rate; OpenAI Realtime overrides to 24 kHz via
/// `subscribe(target_rate)`.
#[allow(dead_code)]
pub const TARGET_RATE: u32 = 16_000;
/// Frame size sent to ElevenLabs (100 ms at 16 kHz).
pub const CHUNK_SAMPLES: usize = 1600;

// ---------------------------------------------------------------------------
// Global audio source — one WASAPI stream, many session resamplers
// ---------------------------------------------------------------------------

/// Created once at app startup. The cpal stream lives on a dedicated thread
/// so that `AudioSource` itself stays `Send + Sync` (required for `Arc<App>`).
pub struct AudioSource {
    /// Shared list of live sessions. The cpal callback holds a read lock;
    /// session start/stop briefly takes the write lock.
    sessions: Arc<parking_lot::RwLock<Vec<SessionEntry>>>,
    /// Set when the app is shutting down; tells the capture thread to exit.
    #[allow(dead_code)]
    stop: Arc<AtomicBool>,
    /// Device sample rate (Hz), stored so sessions can init their resamplers.
    /// Atomic because a device-reopen after a stream error may land on a
    /// different default device with a different rate.
    device_rate: Arc<AtomicU32>,
    /// Device channel count (atomic for the same reopen reason).
    channels: Arc<AtomicUsize>,
    /// `true` while the capture stream is believed to be running. Flipped to
    /// `false` if the cpal error callback fires (e.g. the device is unplugged
    /// mid-session) or the capture thread exits, and back to `true` once the
    /// capture thread manages to reopen a device. Lets the rest of the app
    /// detect that audio has silently stopped instead of every later session
    /// receiving zero samples while the UI still looks alive. See `is_healthy()`.
    healthy: Arc<AtomicBool>,
    /// The capture thread. Joined on `shutdown()`.
    _capture_thread: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>,
}

struct SessionEntry {
    /// Sends 16 kHz mono i16 chunks to the session's send task.
    tx: mpsc::UnboundedSender<Vec<i16>>,
    /// Per-session resampler + pending buffer. Locked briefly by the cpal
    /// callback; never held across a channel send.
    inner: Mutex<SessionResampler>,
}

struct SessionResampler {
    resampler: LinearResampler,
    pending: Vec<i16>,
}

impl AudioSource {
    /// Open the default input device and start streaming on a background
    /// thread. Audio is captured continuously; sessions subscribe to tap in.
    pub fn new() -> Result<Self> {
        let (device, supported) = open_default_input()?;
        let sample_format = supported.sample_format();

        tracing::info!(
            "AudioSource: '{}' @ {} Hz, {} ch, fmt {:?}",
            device.name().unwrap_or_default(),
            supported.sample_rate().0,
            supported.channels(),
            sample_format,
        );

        let sessions: Arc<parking_lot::RwLock<Vec<SessionEntry>>> =
            Arc::new(parking_lot::RwLock::new(Vec::new()));
        let sessions_cb = Arc::clone(&sessions);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_cb = Arc::clone(&stop);
        let healthy = Arc::new(AtomicBool::new(true));
        let healthy_thread = Arc::clone(&healthy);
        let healthy_cb = Arc::clone(&healthy);
        let device_rate = Arc::new(AtomicU32::new(supported.sample_rate().0));
        let device_rate_cb = Arc::clone(&device_rate);
        let channels = Arc::new(AtomicUsize::new(supported.channels() as usize));
        let channels_cb = Arc::clone(&channels);

        let join = std::thread::Builder::new()
            .name("qd-audio".into())
            .spawn(move || {
                run_global_capture(
                    sessions_cb,
                    stop_cb,
                    healthy_cb,
                    device_rate_cb,
                    channels_cb,
                    device,
                    supported,
                );
                // The capture loop is no longer running (clean shutdown, or an
                // unrecoverable device loss at exit). Audio is no longer
                // flowing, so mark unhealthy — callers use is_healthy() to
                // notice.
                healthy_thread.store(false, Ordering::Release);
            })
            .expect("spawn audio thread");

        Ok(Self {
            sessions,
            stop,
            device_rate,
            channels,
            healthy,
            _capture_thread: parking_lot::Mutex::new(Some(join)),
        })
    }

    /// Whether the global capture stream is still believed to be running. Returns
    /// `false` while the device is errored out (e.g. unplugged) and the capture
    /// thread is retrying to reopen it, so the app can surface a visible
    /// "microphone stopped" state instead of silently producing empty dictations.
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    /// Create a new session feed resampled to `target_rate` Hz. Returns the
    /// audio receiver and a flusher that drains the session's resampler tail on
    /// demand. Drop the flusher to unregister from the global source. Each
    /// provider picks its rate via `required_audio_format()` (16 kHz for the
    /// streaming set, 24 kHz for OpenAI Realtime).
    pub fn subscribe(
        self: &Arc<Self>,
        target_rate: u32,
    ) -> (mpsc::UnboundedReceiver<Vec<i16>>, SessionFlusher) {
        let step = self.device_rate.load(Ordering::Acquire) as f64 / target_rate as f64;
        let (tx, rx) = mpsc::unbounded_channel();
        let entry = SessionEntry {
            tx: tx.clone(),
            inner: Mutex::new(SessionResampler {
                resampler: LinearResampler::new(step, self.channels.load(Ordering::Acquire)),
                pending: Vec::with_capacity(CHUNK_SAMPLES * 2),
            }),
        };
        let addr = tx_addr(&tx);
        self.sessions.write().push(entry);
        (
            rx,
            SessionFlusher {
                sessions: Arc::clone(&self.sessions),
                sender_addr: addr,
            },
        )
    }

    /// Signal the capture thread to stop and join it.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self._capture_thread.lock().take() {
            let _ = j.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Capture thread (runs on a dedicated std thread; owns the cpal Stream)
// ---------------------------------------------------------------------------

/// How often the capture thread polls the stop/health flags while streaming.
const HEALTH_POLL: std::time::Duration = std::time::Duration::from_millis(100);
/// Delay between attempts to reopen the input device after a stream failure.
const REOPEN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// Open the current default input device and its default config.
fn open_default_input() -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device"))?;
    let supported = device
        .default_input_config()
        .map_err(|e| anyhow!("default_input_config: {e}"))?;
    Ok((device, supported))
}

/// Outer capture loop: stream from the device until shutdown, and on a stream
/// failure (device unplugged/disabled, `stream.play()` race) keep retrying to
/// reopen the (possibly different) default device instead of dying silently.
/// The `healthy` flag is `false` for the whole degraded window, so sessions
/// surface a visible error instead of recording nothing; it flips back to
/// `true` the moment a reopen succeeds.
fn run_global_capture(
    sessions: Arc<parking_lot::RwLock<Vec<SessionEntry>>>,
    stop: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    device_rate: Arc<AtomicU32>,
    channels: Arc<AtomicUsize>,
    device: cpal::Device,
    supported: cpal::SupportedStreamConfig,
) {
    let mut device = device;
    let mut supported = supported;
    loop {
        match stream_until_failure(&sessions, &stop, &healthy, &device, &supported) {
            Ok(()) => return, // clean shutdown
            Err(e) => {
                healthy.store(false, Ordering::Release);
                if stop.load(Ordering::Acquire) {
                    return;
                }
                tracing::error!("AudioSource capture failed: {e:#}; will retry the device");
            }
        }
        // Reopen retry: poll for a working default input device (the user may
        // replug the mic or Windows may promote another device) until one
        // opens or the app shuts down.
        loop {
            // Wait out the retry delay in HEALTH_POLL steps so shutdown()
            // never has to sit through a full delay to join this thread.
            let mut waited = std::time::Duration::ZERO;
            while waited < REOPEN_RETRY_DELAY {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(HEALTH_POLL);
                waited += HEALTH_POLL;
            }
            match open_default_input() {
                Ok((d, s)) => {
                    device_rate.store(s.sample_rate().0, Ordering::Release);
                    channels.store(s.channels() as usize, Ordering::Release);
                    tracing::info!(
                        "AudioSource: reopened '{}' @ {} Hz, {} ch",
                        d.name().unwrap_or_default(),
                        s.sample_rate().0,
                        s.channels(),
                    );
                    device = d;
                    supported = s;
                    break;
                }
                Err(e) => tracing::debug!("AudioSource reopen attempt failed: {e:#}"),
            }
        }
    }
}

/// Build and run one capture stream. Returns `Ok(())` on a clean shutdown
/// (stop flag) or `Err` if the stream could not be built/played or its error
/// callback fired.
fn stream_until_failure(
    sessions: &Arc<parking_lot::RwLock<Vec<SessionEntry>>>,
    stop: &Arc<AtomicBool>,
    healthy: &Arc<AtomicBool>,
    device: &cpal::Device,
    supported: &cpal::SupportedStreamConfig,
) -> Result<()> {
    let sample_format = supported.sample_format();
    let mut config: cpal::StreamConfig = supported.config();
    config.buffer_size = cpal::BufferSize::Default;
    // A stream error (e.g. the mic is unplugged mid-session) arrives out-of-band:
    // cpal keeps the Stream object alive but stops delivering data, so without
    // this the app would keep "listening" while capturing nothing. Flip the
    // shared health flag so callers can detect it (and the outer loop can
    // rebuild the stream). Built fresh per match-arm because
    // build_input_stream consumes the closure.
    let make_err_fn = || {
        let healthy = Arc::clone(healthy);
        move |e| {
            tracing::error!("audio stream error: {e}");
            healthy.store(false, Ordering::Release);
        }
    };

    let stream: cpal::Stream = match sample_format {
        cpal::SampleFormat::F32 => {
            let sessions = Arc::clone(sessions);
            let mut scratch: Vec<i16> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[f32], _| {
                    scratch.clear();
                    scratch.reserve(data.len());
                    for s in data {
                        scratch.push(f32_to_i16(*s));
                    }
                    feed_sessions(&sessions, &scratch);
                },
                make_err_fn(),
                None,
            )?
        }
        cpal::SampleFormat::I16 => {
            let sessions = Arc::clone(sessions);
            let mut scratch: Vec<i16> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    scratch.clear();
                    scratch.extend_from_slice(data);
                    feed_sessions(&sessions, &scratch);
                },
                make_err_fn(),
                None,
            )?
        }
        cpal::SampleFormat::U16 => {
            let sessions = Arc::clone(sessions);
            let mut scratch: Vec<i16> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[u16], _| {
                    scratch.clear();
                    scratch.reserve(data.len());
                    for s in data {
                        scratch.push((*s as i32 - 32768) as i16);
                    }
                    feed_sessions(&sessions, &scratch);
                },
                make_err_fn(),
                None,
            )?
        }
        other => return Err(anyhow!("unsupported sample format {other:?}")),
    };

    stream.play().map_err(|e| anyhow!("stream.play: {e}"))?;
    healthy.store(true, Ordering::Release);
    tracing::info!("AudioSource: streaming");

    // Idle until shutdown, watching for the error callback.
    while !stop.load(Ordering::Acquire) {
        if !healthy.load(Ordering::Acquire) {
            drop(stream);
            return Err(anyhow!("stream error callback fired"));
        }
        std::thread::sleep(HEALTH_POLL);
    }

    drop(stream);
    tracing::info!("AudioSource: capture stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-session flush control
// ---------------------------------------------------------------------------

/// Handle that lets the send task flush the session's resampler tail and
/// unregister from the global source when done. Cheap to clone (Arc internally).
#[derive(Clone)]
pub struct SessionFlusher {
    sessions: Arc<parking_lot::RwLock<Vec<SessionEntry>>>,
    /// Address of our sender, used to remove the correct entry on drop.
    sender_addr: usize,
}

impl SessionFlusher {
    /// Flush any partial samples sitting in this session's resampler
    /// (the "tail" of the user's last syllable). Call this when the session's
    /// dynamic tail phase ends to avoid clipping the utterance.
    pub fn flush_tail(&self) {
        let sessions = self.sessions.read();
        for entry in sessions.iter() {
            if tx_addr(&entry.tx) == self.sender_addr {
                let mut inner = entry.inner.lock();
                let pending = std::mem::take(&mut inner.pending);
                if !pending.is_empty() {
                    tracing::debug!("audio: flushing {} tail samples", pending.len());
                    let _ = entry.tx.send(pending);
                }
                break;
            }
        }
    }
}

impl Drop for SessionFlusher {
    fn drop(&mut self) {
        // Final flush, then unregister.
        self.flush_tail();
        self.sessions
            .write()
            .retain(|e| tx_addr(&e.tx) != self.sender_addr);
    }
}

fn tx_addr(tx: &mpsc::UnboundedSender<Vec<i16>>) -> usize {
    std::ptr::from_ref(tx).addr()
}

// ---------------------------------------------------------------------------
// Feed helpers
// ---------------------------------------------------------------------------

/// Called from the cpal callback. Feeds every active session's resampler.
/// Dead senders (session dropped without cleanup) are pruned lazily.
fn feed_sessions(sessions: &parking_lot::RwLock<Vec<SessionEntry>>, data: &[i16]) {
    // Fast path: no sessions → nothing to do.
    if sessions.read().is_empty() {
        return;
    }

    // Prune dead entries (receiver dropped) under write lock. This is rare
    // so the write-lock cost is acceptable.
    {
        let mut list = sessions.write();
        if list.iter().any(|e| e.tx.is_closed()) {
            list.retain(|e| !e.tx.is_closed());
        }
    }

    // Feed all remaining sessions under read lock.
    let list = sessions.read();
    for entry in list.iter() {
        // Lock, drain any full chunks, unlock, then send. Never hold the
        // resampler lock across a channel send.
        loop {
            let chunk: Option<Vec<i16>> = {
                let mut inner = entry.inner.lock();
                let SessionResampler { resampler, pending } = &mut *inner;
                resampler.feed_and_emit(data, pending);
                if pending.len() >= CHUNK_SAMPLES {
                    Some(pending.drain(..CHUNK_SAMPLES).collect())
                } else {
                    None
                }
            }; // lock released here
            match chunk {
                Some(c) => {
                    if entry.tx.send(c).is_err() {
                        return; // receiver gone
                    }
                }
                None => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Resampler (unchanged algorithm — linear interpolation, mono mix)
// ---------------------------------------------------------------------------

#[inline]
fn f32_to_i16(s: f32) -> i16 {
    let v = (s * 32767.0).clamp(-32768.0, 32767.0);
    v as i16
}

#[derive(Default)]
struct LinearResampler {
    step: f64,
    channels: usize,
    pos: f64,
    last_frame_mono: Option<i16>,
    consumed: u64,
}

impl LinearResampler {
    fn new(step: f64, channels: usize) -> Self {
        Self {
            step,
            channels: channels.max(1),
            pos: 0.0,
            last_frame_mono: None,
            consumed: 0,
        }
    }

    fn feed_and_emit(&mut self, data: &[i16], out: &mut Vec<i16>) {
        if data.is_empty() {
            return;
        }
        let ch = self.channels;
        let frames = data.len() / ch;
        if frames == 0 {
            return;
        }

        let frame_start = self.consumed;
        let frame_end = self.consumed + frames as u64;

        let mono = |i: usize| -> i16 {
            if ch == 1 {
                data[i]
            } else {
                let mut acc: i32 = 0;
                let base = i * ch;
                for c in 0..ch {
                    acc += data[base + c] as i32;
                }
                (acc / ch as i32) as i16
            }
        };

        let prev_mono = self.last_frame_mono;
        while self.pos < frame_end as f64 {
            let local = self.pos - frame_start as f64;
            if local < 0.0 {
                let p0 = prev_mono.unwrap_or(0) as f32;
                let p1 = mono(0) as f32;
                let frac = (local + 1.0).clamp(0.0, 1.0) as f32;
                let v = p0 * (1.0 - frac) + p1 * frac;
                out.push(v as i16);
            } else {
                let i = local as usize;
                let frac = (local - i as f64) as f32;
                if i + 1 < frames {
                    let a = mono(i) as f32;
                    let b = mono(i + 1) as f32;
                    out.push((a * (1.0 - frac) + b * frac) as i16);
                } else if i < frames {
                    out.push(mono(i));
                } else {
                    break;
                }
            }
            self.pos += self.step;
        }

        self.last_frame_mono = Some(mono(frames - 1));
        self.consumed = frame_end;
    }
}
