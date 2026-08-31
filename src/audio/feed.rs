//! Handing one captured buffer to every live session without allocating in
//! the audio callback.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use tokio::sync::mpsc;

use super::*;

// ---------------------------------------------------------------------------
// Feed helpers
// ---------------------------------------------------------------------------

/// Cap on how many recovered chunk buffers a session's free list holds, so a
/// long stretch of a full queue cannot grow it without bound. Also the number
/// of buffers a session preallocates up front in `subscribe()`.
pub(super) const SPARE_CHUNK_POOL_CAP: usize = 4;

/// Called from the cpal callback. Feeds every active session's resampler.
/// Dead senders (session dropped without cleanup) are pruned lazily.
/// `device_rate`/`channels` are the shared atomics for the device's current
/// format; each session's resampler is compared against them below and
/// rebuilt in place if a device reopen (unplug, mic swap, sleep resume)
/// changed the format out from under it.
pub(super) fn feed_sessions(
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
