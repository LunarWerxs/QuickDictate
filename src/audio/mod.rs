//! Global microphone capture. The WASAPI stream is opened **once** at startup
//! on a dedicated thread (cpal `Stream` is not `Send`) and stays alive for the
//! app's lifetime. Sessions subscribe to get a dedicated resampler feed,
//! eliminating per-session mic initialization latency.

mod capture;
mod feed;
mod resample;
mod source;

#[cfg(test)]
mod tests;

use std::sync::atomic::AtomicU64;

pub use capture::set_preferred_input;
pub use source::{AudioSource, SessionFlusher};

use capture::*;
use feed::*;
use resample::*;
use source::*;

/// Frame size sent to ElevenLabs (100 ms at 16 kHz).
pub const CHUNK_SAMPLES: usize = 1600;
/// Cap queued microphone audio per session so a stalled network connection
/// cannot grow memory without bound. This is about 4–6 seconds, depending on
/// whether the provider consumes 24 kHz or 16 kHz audio.
const AUDIO_QUEUE_CAPACITY: usize = 64;
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
