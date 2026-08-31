//! The one `AudioSource` every session subscribes to, and the per-session
//! flush handle it hands back.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use cpal::traits::DeviceTrait;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::*;

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

pub(super) struct SessionEntry {
    pub(super) id: u64,
    /// Sends 16 kHz mono i16 chunks to the session's send task.
    pub(super) tx: mpsc::Sender<Vec<i16>>,
    /// Per-session resampler + pending buffer. Locked briefly by the cpal
    /// callback; never held across a channel send.
    pub(super) inner: Mutex<SessionResampler>,
    /// Prevent a backed-up consumer from producing one warning per audio frame.
    pub(super) queue_full_reported: AtomicBool,
}

pub(super) struct SessionResampler {
    pub(super) resampler: LinearResampler,
    pub(super) pending: Vec<i16>,
    /// Output rate this session was subscribed at. Fixed for the session's
    /// lifetime; needed to recompute the resampler's step if it has to be
    /// rebuilt for a new device rate.
    pub(super) target_rate: u32,
    /// Device sample rate and channel count `resampler` was last built for.
    /// Compared against the shared `device_rate`/`channels` atomics on every
    /// callback (see `feed_sessions`) so a device reopen after an unplug, mic
    /// swap, or sleep resume is caught in-flight and the resampler rebuilt
    /// for the new format, instead of silently mis-pacing or mis-mixing new
    /// PCM according to the old one.
    pub(super) built_rate: u32,
    pub(super) built_channels: usize,
    /// Small free list of chunk buffers recovered from sends that hit a full
    /// queue, reused for the next completed chunk instead of allocating
    /// fresh audio storage. Empty in the common case; a fresh buffer is
    /// allocated whenever the pool has none to give.
    pub(super) spare_chunks: Vec<Vec<i16>>,
}

impl AudioSource {
    /// Open the default input device and start streaming on a background
    /// thread. Audio is captured continuously; sessions subscribe to tap in.
    pub fn new() -> Result<Self> {
        let (device, supported) = resolve_input()?;
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

        #[allow(
            clippy::expect_used,
            reason = "a capture thread that cannot be spawned is unrecoverable; the panic message is the only diagnostic there is"
        )]
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
// Per-session flush control
// ---------------------------------------------------------------------------

/// Handle that lets the send task flush the session's resampler tail and
/// unregister from the global source when done. Cheap to clone (Arc internally).
#[derive(Clone)]
pub struct SessionFlusher {
    pub(super) sessions: Arc<parking_lot::RwLock<Vec<SessionEntry>>>,
    pub(super) session_id: u64,
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
