//! Global microphone capture. The WASAPI stream is opened **once** at startup
//! on a dedicated thread (cpal `Stream` is not `Send`) and stays alive for the
//! app's lifetime. Sessions subscribe to get a dedicated resampler feed,
//! eliminating per-session mic initialization latency.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use tokio::sync::mpsc;

/// Frame size sent to ElevenLabs (100 ms at 16 kHz).
pub const CHUNK_SAMPLES: usize = 1600;
/// Cap queued microphone audio per session so a stalled network connection
/// cannot grow memory without bound. This is about 4–6 seconds, depending on
/// whether the provider consumes 24 kHz or 16 kHz audio.
const AUDIO_QUEUE_CAPACITY: usize = 64;
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

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
    id: u64,
    /// Sends 16 kHz mono i16 chunks to the session's send task.
    tx: mpsc::Sender<Vec<i16>>,
    /// Per-session resampler + pending buffer. Locked briefly by the cpal
    /// callback; never held across a channel send.
    inner: Mutex<SessionResampler>,
    /// Prevent a backed-up consumer from producing one warning per audio frame.
    queue_full_reported: AtomicBool,
}

struct SessionResampler {
    resampler: LinearResampler,
    pending: Vec<i16>,
    /// Output rate this session was subscribed at. Fixed for the session's
    /// lifetime; needed to recompute the resampler's step if it has to be
    /// rebuilt for a new device rate.
    target_rate: u32,
    /// Device sample rate and channel count `resampler` was last built for.
    /// Compared against the shared `device_rate`/`channels` atomics on every
    /// callback (see `feed_sessions`) so a device reopen after an unplug, mic
    /// swap, or sleep resume is caught in-flight and the resampler rebuilt
    /// for the new format, instead of silently mis-pacing or mis-mixing new
    /// PCM according to the old one.
    built_rate: u32,
    built_channels: usize,
    /// Small free list of chunk buffers recovered from sends that hit a full
    /// queue, reused for the next completed chunk instead of allocating
    /// fresh audio storage. Empty in the common case; a fresh buffer is
    /// allocated whenever the pool has none to give.
    spare_chunks: Vec<Vec<i16>>,
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
    ) -> (mpsc::Receiver<Vec<i16>>, SessionFlusher) {
        let device_rate = self.device_rate.load(Ordering::Acquire);
        let channels = self.channels.load(Ordering::Acquire);
        let step = device_rate as f64 / target_rate as f64;
        let (tx, rx) = mpsc::channel(AUDIO_QUEUE_CAPACITY);
        let session_id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let entry = SessionEntry {
            id: session_id,
            tx,
            inner: Mutex::new(SessionResampler {
                resampler: LinearResampler::new(step, channels),
                pending: Vec::with_capacity(CHUNK_SAMPLES * 2),
                target_rate,
                built_rate: device_rate,
                built_channels: channels,
                // Seed a couple of buffers up front so the first chunks a
                // fresh session emits can reuse them too, not just chunks
                // recovered later from a full queue. Built with a loop, not
                // `vec![Vec::with_capacity(n); k]`, because cloning an empty
                // Vec does not carry its capacity along, only the original
                // would actually be preallocated.
                spare_chunks: (0..SPARE_CHUNK_POOL_CAP)
                    .map(|_| Vec::with_capacity(CHUNK_SAMPLES))
                    .collect(),
            }),
            queue_full_reported: AtomicBool::new(false),
        };
        self.sessions.write().push(entry);
        (
            rx,
            SessionFlusher {
                sessions: Arc::clone(&self.sessions),
                session_id,
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
        match stream_until_failure(
            &sessions,
            &stop,
            &healthy,
            &device_rate,
            &channels,
            &device,
            &supported,
        ) {
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
    device_rate: &Arc<AtomicU32>,
    channels: &Arc<AtomicUsize>,
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
            let device_rate = Arc::clone(device_rate);
            let channels = Arc::clone(channels);
            let mut scratch: Vec<i16> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[f32], _| {
                    scratch.clear();
                    scratch.reserve(data.len());
                    for s in data {
                        scratch.push(f32_to_i16(*s));
                    }
                    feed_sessions(&sessions, &device_rate, &channels, &scratch);
                },
                make_err_fn(),
                None,
            )?
        }
        cpal::SampleFormat::I16 => {
            let sessions = Arc::clone(sessions);
            let device_rate = Arc::clone(device_rate);
            let channels = Arc::clone(channels);
            device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    // WASAPI already gave us the exact representation the
                    // resamplers consume, so avoid copying every callback into
                    // an otherwise-identical scratch buffer.
                    feed_sessions(&sessions, &device_rate, &channels, data);
                },
                make_err_fn(),
                None,
            )?
        }
        cpal::SampleFormat::U16 => {
            let sessions = Arc::clone(sessions);
            let device_rate = Arc::clone(device_rate);
            let channels = Arc::clone(channels);
            let mut scratch: Vec<i16> = Vec::new();
            device.build_input_stream(
                &config,
                move |data: &[u16], _| {
                    scratch.clear();
                    scratch.reserve(data.len());
                    for s in data {
                        scratch.push((*s as i32 - 32768) as i16);
                    }
                    feed_sessions(&sessions, &device_rate, &channels, &scratch);
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
    session_id: u64,
}

impl SessionFlusher {
    /// Atomically unregister this session from future capture callbacks, then
    /// enqueue the last pending resampler fragment while its receiver is still
    /// alive. Consuming the handle makes end-of-session ordering explicit; any
    /// other clones become harmless no-ops when dropped.
    pub fn finish(self) {
        self.flush_and_unregister();
    }

    fn flush_and_unregister(&self) {
        let entry = {
            let mut sessions = self.sessions.write();
            sessions
                .iter()
                .position(|entry| entry.id == self.session_id)
                .map(|index| sessions.remove(index))
        };
        if let Some(entry) = entry {
            let pending = std::mem::take(&mut entry.inner.lock().pending);
            if !pending.is_empty() {
                tracing::debug!("audio: flushing {} final tail samples", pending.len());
                if entry.tx.try_send(pending).is_err() {
                    tracing::warn!("audio: could not enqueue final resampler tail");
                }
            }
        }
    }
}

impl Drop for SessionFlusher {
    fn drop(&mut self) {
        // Best-effort cleanup when a caller exits without `finish()`.
        self.flush_and_unregister();
    }
}

// ---------------------------------------------------------------------------
// Feed helpers
// ---------------------------------------------------------------------------

/// Cap on how many recovered chunk buffers a session's free list holds, so a
/// long stretch of a full queue cannot grow it without bound. Also the number
/// of buffers a session preallocates up front in `subscribe()`.
const SPARE_CHUNK_POOL_CAP: usize = 4;

/// Called from the cpal callback. Feeds every active session's resampler.
/// Dead senders (session dropped without cleanup) are pruned lazily.
/// `device_rate`/`channels` are the shared atomics for the device's current
/// format; each session's resampler is compared against them below and
/// rebuilt in place if a device reopen (unplug, mic swap, sleep resume)
/// changed the format out from under it.
fn feed_sessions(
    sessions: &parking_lot::RwLock<Vec<SessionEntry>>,
    device_rate: &AtomicU32,
    channels: &AtomicUsize,
    data: &[i16],
) {
    // Normal path takes only a shared lock. A write lock is needed solely when
    // a session disappeared without its normal flusher cleanup.
    let mut list = sessions.read();
    if list.is_empty() {
        return;
    }

    if list.iter().any(|entry| entry.tx.is_closed()) {
        drop(list);
        {
            let mut writable = sessions.write();
            writable.retain(|entry| !entry.tx.is_closed());
        }
        list = sessions.read();
        if list.is_empty() {
            return;
        }
    }

    // Two cheap atomic loads per callback, not per session and not per
    // sample. Every session below compares its own built-for values against
    // this single snapshot of the device's current format.
    let current_rate = device_rate.load(Ordering::Acquire);
    let current_channels = channels.load(Ordering::Acquire);

    for entry in list.iter() {
        // Feed this callback buffer exactly once, drain every complete chunk,
        // then unlock before touching the channel. Re-feeding `data` inside the
        // drain loop duplicates microphone audio whenever a callback crosses a
        // chunk boundary.
        let chunks = {
            let mut inner = entry.inner.lock();
            let SessionResampler {
                resampler,
                pending,
                target_rate,
                built_rate,
                built_channels,
                spare_chunks,
            } = &mut *inner;

            if *built_rate != current_rate || *built_channels != current_channels {
                tracing::warn!(
                    "audio: device format changed ({} Hz, {} ch -> {} Hz, {} ch); \
                     rebuilding session resampler",
                    built_rate,
                    built_channels,
                    current_rate,
                    current_channels
                );
                let step = current_rate as f64 / *target_rate as f64;
                *resampler = LinearResampler::new(step, current_channels);
                *built_rate = current_rate;
                *built_channels = current_channels;
                // `pending` is left alone: it holds already-resampled output
                // waiting to be chunked, which is still correct: only the
                // resampler's own interpolation state (fed samples, half-frame
                // carry) was tied to the old format, and `LinearResampler::new`
                // resets exactly that.
            }

            resampler.feed_and_emit(data, pending);

            let mut chunks = Vec::new();
            while pending.len() >= CHUNK_SAMPLES {
                // Reuse a buffer from the session's free list when one is
                // available instead of allocating fresh audio storage on
                // every chunk boundary; a new buffer is allocated only when
                // the pool is empty.
                let mut chunk = spare_chunks
                    .pop()
                    .unwrap_or_else(|| Vec::with_capacity(CHUNK_SAMPLES));
                chunk.clear();
                chunk.extend_from_slice(&pending[..CHUNK_SAMPLES]);
                // drain() shifts the remainder down in place, reusing
                // `pending`'s existing allocation. split_off() would allocate
                // a brand new Vec for the tail on every chunk boundary, which
                // is an allocation inside the real-time capture callback.
                pending.drain(..CHUNK_SAMPLES);
                chunks.push(chunk);
            }
            chunks
        };
        for chunk in chunks {
            match entry.tx.try_send(chunk) {
                Ok(()) => {
                    if entry.queue_full_reported.swap(false, Ordering::Relaxed) {
                        tracing::info!("audio: session queue recovered");
                    }
                }
                Err(mpsc::error::TrySendError::Full(returned)) => {
                    if !entry.queue_full_reported.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            "audio: session queue full; dropping audio to keep latency bounded"
                        );
                    }
                    return_spare_chunk(entry, returned);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }
    }
}

/// A `try_send` that hit a full queue hands the chunk buffer back instead of
/// dropping it silently. Stash it on the session's free list so the next
/// completed chunk can reuse its allocation.
fn return_spare_chunk(entry: &SessionEntry, mut buf: Vec<i16>) {
    let mut inner = entry.inner.lock();
    if inner.spare_chunks.len() < SPARE_CHUNK_POOL_CAP {
        buf.clear();
        inner.spare_chunks.push(buf);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entry(id: u64, tx: mpsc::Sender<Vec<i16>>) -> SessionEntry {
        SessionEntry {
            id,
            tx,
            inner: Mutex::new(SessionResampler {
                resampler: LinearResampler::new(1.0, 1),
                pending: Vec::new(),
                target_rate: 16_000,
                built_rate: 16_000,
                built_channels: 1,
                spare_chunks: Vec::new(),
            }),
            queue_full_reported: AtomicBool::new(false),
        }
    }

    /// Device-format atomics matching what `test_entry` was built for, so a
    /// plain `feed_sessions` call in a test does not itself trigger a rebuild.
    fn native_format() -> (AtomicU32, AtomicUsize) {
        (AtomicU32::new(16_000), AtomicUsize::new(1))
    }

    #[test]
    fn feed_sessions_processes_each_input_buffer_once() {
        let (tx, mut rx) = mpsc::channel(4);
        let sessions = parking_lot::RwLock::new(vec![test_entry(1, tx)]);
        let (device_rate, channels) = native_format();
        let input = vec![123_i16; CHUNK_SAMPLES + 1];

        feed_sessions(&sessions, &device_rate, &channels, &input);

        let chunk = rx.try_recv().expect("one complete audio chunk");
        assert_eq!(chunk.len(), CHUNK_SAMPLES);
        assert!(rx.try_recv().is_err(), "input buffer was duplicated");
        assert_eq!(sessions.read()[0].inner.lock().pending.len(), 1);
    }

    #[test]
    fn feed_sessions_rebuilds_resampler_on_device_format_change() {
        // Session subscribed at 16 kHz target; device starts at 16 kHz mono,
        // so step 1.0 is a passthrough. Simulate a reopen at double the rate
        // mid-stream, as `run_global_capture` does after an unplug, mic swap,
        // or sleep resume, and confirm the session's resampler rebuilds for
        // the new step instead of continuing to pace input as if the device
        // were still at the old rate.
        let (tx, mut rx) = mpsc::channel(4);
        let sessions = parking_lot::RwLock::new(vec![test_entry(1, tx)]);
        let (device_rate, channels) = native_format();

        let warm = vec![1_i16; CHUNK_SAMPLES];
        feed_sessions(&sessions, &device_rate, &channels, &warm);
        let first = rx.try_recv().expect("chunk at the original rate");
        assert_eq!(first.len(), CHUNK_SAMPLES);

        // Reopen at double the rate; channel count is unchanged.
        device_rate.store(32_000, Ordering::Release);

        // At the new 32 kHz -> 16 kHz step (2.0), the same input length now
        // yields half as many output samples, so it alone cannot complete a
        // chunk unless the resampler picked up the new step.
        let after = vec![2_i16; CHUNK_SAMPLES];
        feed_sessions(&sessions, &device_rate, &channels, &after);
        assert!(
            rx.try_recv().is_err(),
            "resampler kept the stale step instead of rebuilding for the new rate"
        );
        assert_eq!(
            sessions.read()[0].inner.lock().pending.len(),
            CHUNK_SAMPLES / 2,
            "rebuilt resampler should downsample 2:1 at the new device rate"
        );
    }

    #[test]
    fn feed_sessions_drain_chunking_preserves_exact_sample_order() {
        // Regression guard for the split_off -> drain change: feed input
        // across several callback-sized slices that do not line up with
        // CHUNK_SAMPLES boundaries, and confirm every emitted chunk plus the
        // final pending tail reconstructs the exact original sample
        // sequence, with no gaps, duplicates, or off-by-one shifts from
        // reusing pooled buffers.
        let (tx, mut rx) = mpsc::channel(32);
        let sessions = parking_lot::RwLock::new(vec![test_entry(1, tx)]);
        let (device_rate, channels) = native_format();

        let total = CHUNK_SAMPLES * 3 + 250;
        let all: Vec<i16> = (0..total as i32).map(|v| v as i16).collect();

        let mut received = Vec::new();
        // Odd-sized slices so chunk boundaries land mid-callback.
        for slice in all.chunks(777) {
            feed_sessions(&sessions, &device_rate, &channels, slice);
            while let Ok(chunk) = rx.try_recv() {
                received.extend(chunk);
            }
        }
        received.extend(sessions.read()[0].inner.lock().pending.clone());

        assert_eq!(received, all);
    }

    #[test]
    fn session_flusher_targets_and_unregisters_by_stable_id() {
        let (tx1, mut rx1) = mpsc::channel(4);
        let (tx2, mut rx2) = mpsc::channel(4);
        let mut first = test_entry(11, tx1);
        first.inner.get_mut().pending.push(1);
        let mut second = test_entry(22, tx2);
        second.inner.get_mut().pending.push(2);
        let sessions = Arc::new(parking_lot::RwLock::new(vec![first, second]));
        let flusher = SessionFlusher {
            sessions: Arc::clone(&sessions),
            session_id: 22,
        };

        flusher.finish();
        assert!(rx1.try_recv().is_err());
        assert_eq!(rx2.try_recv().expect("target session tail"), vec![2]);

        let remaining: Vec<u64> = sessions.read().iter().map(|entry| entry.id).collect();
        assert_eq!(remaining, vec![11]);
    }

    #[test]
    fn finish_flushes_before_unregistering_and_closing_the_receiver() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut entry = test_entry(33, tx);
        entry.inner.get_mut().pending.extend([7, 8, 9]);
        let sessions = Arc::new(parking_lot::RwLock::new(vec![entry]));
        let flusher = SessionFlusher {
            sessions: Arc::clone(&sessions),
            session_id: 33,
        };

        flusher.finish();

        assert!(sessions.read().is_empty());
        assert_eq!(
            rx.try_recv().expect("final pending fragment"),
            vec![7, 8, 9]
        );
    }
}
