//! Provider abstraction for speech-to-text backends.
//!
//! The session runner in [`crate::stt`] is written **once** against these
//! traits; each STT backend (ElevenLabs, Deepgram, …) is a small adapter that
//! maps its own wire protocol onto the normalized [`SttEvent`] stream. The
//! runner never sees a provider's JSON envelope, auth header, or commit
//! handshake — only these events.
//!
//! A live session is modeled as a **split** connection: a [`ProviderSink`]
//! (audio out: `send_audio`/`commit`/`close`) and a [`ProviderStream`]
//! (events in: `recv_event`). The split lets the runner drive sending and
//! receiving on two concurrent tasks exactly as the original hardcoded
//! ElevenLabs path did, without either half borrowing the other.

use std::time::Duration;

use async_trait::async_trait;

use crate::keys::FailKind;

/// Normalized transcript/lifecycle events. Every provider maps its wire
/// protocol onto these; the session runner only ever sees these.
#[derive(Debug, Clone)]
pub enum SttEvent {
    /// The provider acknowledged the session/handshake. Informational.
    SessionStarted,
    /// Interim hypothesis (drives the live word count). Non-empty, trimmed.
    Partial(String),
    /// Durable final chunk (drives paste). Non-empty, trimmed.
    Committed(String),
    /// The provider told us the credential is bad / exhausted / rate-limited.
    KeyFailure(FailKind),
    /// Transport closed; reason string if the peer gave one.
    Closed(Option<String>),
}

/// PCM wire-encoding a provider wants. Beta1 providers are all 16-bit
/// little-endian PCM; only the sample rate varies (16 kHz default; OpenAI
/// Realtime wants 24 kHz).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Encoding {
    Pcm16Le,
}

#[derive(Copy, Clone, Debug)]
pub struct AudioFormat {
    pub sample_rate: u32,
    /// Reserved: every Beta1 provider is PCM16LE, so the runner doesn't branch
    /// on this yet. Kept in the format contract for providers that later want a
    /// different wire encoding (e.g. mu-law).
    #[allow(dead_code)]
    pub encoding: Encoding,
}

/// Per-session options negotiated by the runner and handed to `connect`.
#[derive(Clone, Debug)]
pub struct SttSessionOpts {
    /// Language identifier. **Provider-specific granularity:** most streaming
    /// providers take a bare ISO code (`en`); Google batch wants full BCP-47
    /// (`en-US`). Each adapter documents which it expects; the runner fills
    /// this from the provider's [`SttProvider::language_for`].
    pub language: String,
    /// Sample rate the audio pipeline will actually deliver, matching
    /// [`SttProvider::required_audio_format`].
    pub sample_rate: u32,
    /// Optional per-provider model override (else the provider's default).
    pub model: Option<String>,
}

/// Failure connecting / upgrading the transport. Carries the raw message so
/// the provider's [`SttProvider::classify_connect_error`] can map it to a
/// [`FailKind`] (invalid key vs quota vs rate-limit vs transient).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConnectError(pub String);

/// Failure sending audio / commit / close on an established connection.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SendError(pub String);

/// Failure receiving/parsing an inbound event.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RecvError(pub String);

/// An established, split provider connection.
pub struct ProviderSession {
    pub sink: Box<dyn ProviderSink>,
    pub stream: Box<dyn ProviderStream>,
}

/// Provider factory: stateless description + `connect`.
#[async_trait]
pub trait SttProvider: Send + Sync {
    /// Stable id (`"elevenlabs"`, `"deepgram"`, …). Used in logs/config.
    fn id(&self) -> &'static str;

    /// PCM format this provider needs on the wire. The runner subscribes the
    /// audio pipeline at this rate.
    fn required_audio_format(&self) -> AudioFormat;

    /// Map the app's configured `language` (e.g. `"en-US"`) to the exact string
    /// this provider expects. Default: strip the region (`en-US` -> `en`),
    /// which is what the streaming providers want; Google batch overrides this
    /// to keep the full BCP-47 tag.
    fn language_for(&self, configured: &str) -> String {
        configured
            .split('-')
            .next()
            .unwrap_or("en")
            .to_ascii_lowercase()
    }

    /// Classify a connect/upgrade failure into a [`FailKind`] so the key pool
    /// can cool the credential down appropriately.
    fn classify_connect_error(&self, err: &ConnectError) -> FailKind {
        classify_by_substring(&err.0)
    }

    /// How long the runner waits for the send task (final audio flush + commit
    /// + close) to complete after hotkey release. Streaming providers finish
    /// within the dynamic-tail window; **batch** providers (Google) do their
    /// single network round-trip inside `commit()` and need much longer.
    ///
    /// This is only a **lower bound**: the dynamic tail is now user-configurable
    /// (`Config::listen_tail_ms`), so `run_session` always floors the real
    /// deadline at `tail_max + 600 ms` on top of this value (see the
    /// `send_deadline` there). This streaming default therefore just needs to
    /// cover the *default* tail plus commit/close; it does not have to track a
    /// raised `listen_tail_ms`.
    fn finalize_timeout(&self) -> Duration {
        Duration::from_millis(2400)
    }

    /// Open a session for `key`. Returns a split sink+stream on success.
    async fn connect(
        &self,
        key: &str,
        opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError>;
}

/// Audio-out half of a live session.
#[async_trait]
pub trait ProviderSink: Send {
    /// Send one chunk of PCM (`required_audio_format` sample rate, mono i16).
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError>;
    /// Signal end-of-utterance so the provider flushes a final transcript.
    /// No-op for VAD-only providers; for batch providers this is where the
    /// single network round-trip happens.
    async fn commit(&mut self) -> Result<(), SendError>;
    /// Tear the transport down cleanly.
    async fn close(&mut self) -> Result<(), SendError>;
}

/// Events-in half of a live session.
#[async_trait]
pub trait ProviderStream: Send {
    /// Next normalized event. `Ok(None)` means the stream is fully drained
    /// (transport closed and no more events). Adapters internally skip frames
    /// that don't map to a meaningful [`SttEvent`] (keep-alives, empty
    /// transcripts, unknown message types).
    async fn recv_event(&mut self) -> Result<Option<SttEvent>, RecvError>;
}

/// Reinterpret an `i16` slice as little-endian bytes without copying. Safe on
/// x86_64 Windows (the only target). Shared by every raw-PCM provider
/// (ElevenLabs base64, Deepgram/DashScope/AssemblyAI binary frames).
#[inline]
pub(crate) fn i16_slice_as_bytes(samples: &[i16]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(samples.as_ptr() as *const u8, samples.len() * 2) }
}

/// Best-effort FailKind from an error/close string. Providers that expose a
/// real HTTP status should prefer that; this is the substring fallback the
/// original ElevenLabs path used and the default `classify_connect_error`.
pub(crate) fn classify_by_substring(msg: &str) -> FailKind {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("insufficient")
        || lower.contains("quota")
        || lower.contains("billing")
        || lower.contains("credit")
        || lower.contains("arrearage")     // DashScope: account out of balance
        || lower.contains("good standing") // DashScope arrearage message
        || lower.contains("balance")
        || lower.contains("payment")
    {
        FailKind::Exhausted
    } else if lower.contains("401")
        || lower.contains("403")
        || lower.contains("invalid")
        || lower.contains("unauthorized")
    {
        FailKind::Invalid
    } else if lower.contains("429") || lower.contains("rate") || lower.contains("too many") {
        FailKind::RateLimit
    } else {
        FailKind::Transient
    }
}
