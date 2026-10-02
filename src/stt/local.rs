//! Keyless, fully local batch transcription via the optional model packs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::local_early::{combine, take_early_chunk, EarlyDecoder};
use super::provider::{
    AudioFormat, ConnectError, ProviderSession, ProviderSink, ProviderStream, RecvError, SendError,
    SttEvent, SttProvider, SttSessionOpts,
};

/// A safety stop for a dictation nobody ended (four hours). It used to be six
/// minutes, because the whole recording was decoded in one pass after release
/// and Cohere's positional table tops out around 400 seconds; long dictations
/// now reach the model as <=35 s clips while they are being spoken
/// (`local_early`), so memory stays bounded by the undecoded tail.
const MAX_AUDIO_SECONDS: usize = 4 * 60 * 60;

pub struct LocalProvider {
    pub model_id: String,
}

#[async_trait]
impl SttProvider for LocalProvider {
    fn id(&self) -> &'static str {
        "local"
    }

    /// Batch: inference runs once on the finished recording, so the pip spins
    /// rather than holding a word count of "0" throughout.
    fn streams_interim_text(&self) -> bool {
        false
    }

    fn required_audio_format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: 16_000,
        }
    }

    fn requires_api_key(&self) -> bool {
        false
    }

    fn finalize_timeout(&self) -> Duration {
        // First use includes loading a multi-gigabyte model; CPU-only machines
        // may also need a while for a long utterance. Native cancellation keeps
        // this bounded if the provider half is dropped.
        Duration::from_secs(5 * 60)
    }

    async fn connect(
        &self,
        _key: &str,
        opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        if !crate::local_stt::is_installed(&self.model_id) {
            if crate::local_stt::model_verified(&self.model_id) {
                // The weights are here; only the runtime this build pins is
                // missing (an app update). Start fetching it, and say so.
                crate::local_stt::request_prewarm(&self.model_id);
                return Err(ConnectError(format!(
                    "'{}' is updating its offline runtime (a one-time ~17 MB download); \
                     dictate again in a moment",
                    self.model_id
                )));
            }
            return Err(ConnectError(format!(
                "'{}' is not installed; install it in Settings → Speech-to-text provider",
                self.model_id
            )));
        }
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Ok(ProviderSession {
            sink: Box::new(LocalSink {
                model_id: self.model_id.clone(),
                language: opts.language.clone(),
                vocabulary: opts.vocabulary_prompt(),
                sample_rate: opts.sample_rate,
                pcm: Vec::new(),
                received: 0,
                early: None,
                event_tx: Some(event_tx),
                cancel: Arc::new(AtomicBool::new(false)),
                finished: false,
                limit_hit: false,
            }),
            stream: Box::new(LocalStream { event_rx }),
        })
    }
}

struct LocalSink {
    model_id: String,
    language: String,
    /// Custom vocabulary as one prompt. The Whisper model is biased with it;
    /// Cohere has no prompt input and ignores it.
    vocabulary: String,
    sample_rate: u32,
    /// Audio not yet handed to the model: the whole recording for a short
    /// dictation, only the tail since the last early clip for a long one.
    pcm: Vec<i16>,
    /// Samples received so far, for the `MAX_AUDIO_SECONDS` safety stop.
    received: usize,
    /// Started on a long dictation's first early clip.
    early: Option<EarlyDecoder>,
    event_tx: Option<mpsc::UnboundedSender<SttEvent>>,
    cancel: Arc<AtomicBool>,
    finished: bool,
    /// Set once the recording reached `MAX_AUDIO_SECONDS`, so the warning is
    /// logged once rather than per dropped chunk.
    limit_hit: bool,
}

impl Drop for LocalSink {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

#[async_trait]
impl ProviderSink for LocalSink {
    /// Past the limit the audio is dropped but the send still succeeds. An
    /// error here reads as a dead socket to the runner, which then skips
    /// `commit`, and the minutes already buffered were never transcribed:
    /// the user got an error and no text at all. Now `commit` transcribes
    /// the first `MAX_AUDIO_SECONDS`.
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        let keep = samples_to_keep(self.received, pcm.len(), self.max_samples());
        self.received += keep;
        self.pcm.extend_from_slice(&pcm[..keep]);
        if keep < pcm.len() && !self.limit_hit {
            self.limit_hit = true;
            tracing::warn!(
                "local dictation reached the {MAX_AUDIO_SECONDS}-second safety limit; \
                 transcribing the first {MAX_AUDIO_SECONDS} s and dropping the rest"
            );
        }
        if let Some(chunk) = take_early_chunk(&mut self.pcm, self.sample_rate as usize) {
            if self.early.is_none() {
                tracing::info!("local dictation is long; transcribing it while it is spoken");
                self.early = Some(EarlyDecoder::spawn(
                    self.model_id.clone(),
                    self.language.clone(),
                    self.vocabulary.clone(),
                    Arc::clone(&self.cancel),
                ));
            }
            if let Some(early) = &self.early {
                early.push(chunk);
            }
        }
        Ok(())
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let pcm = std::mem::take(&mut self.pcm);
        let result = match self.early.take() {
            // A long dictation: its earlier clips are already decoded or in
            // flight, so only the tail is left to wait for.
            Some(early) => combine(early.finish(pcm).await),
            None => {
                crate::local_stt::transcribe(
                    self.model_id.clone(),
                    self.language.clone(),
                    self.vocabulary.clone(),
                    pcm,
                    Arc::clone(&self.cancel),
                )
                .await
            }
        };
        if let Some(tx) = self.event_tx.take() {
            match result {
                Ok(Some(text)) => {
                    let _ = tx.send(SttEvent::Committed(text));
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = tx.send(SttEvent::ProviderFailure(e));
                }
            }
        }
        Ok(())
    }

    async fn close(&mut self) -> Result<(), SendError> {
        // `commit` performs and drains the one batch. Close only ensures the
        // event stream terminates for the generic receive task.
        self.event_tx.take();
        Ok(())
    }
}

impl LocalSink {
    fn max_samples(&self) -> usize {
        (self.sample_rate as usize).saturating_mul(MAX_AUDIO_SECONDS)
    }
}

/// How much of an incoming buffer fits under the session's safety stop.
fn samples_to_keep(received: usize, incoming: usize, max_samples: usize) -> usize {
    incoming.min(max_samples.saturating_sub(received))
}

struct LocalStream {
    event_rx: mpsc::UnboundedReceiver<SttEvent>,
}

#[async_trait]
impl ProviderStream for LocalStream {
    async fn recv_event(&mut self) -> Result<Option<SttEvent>, RecvError> {
        Ok(self.event_rx.recv().await)
    }
}

#[cfg(test)]
mod tests {
    use super::super::local_early::EARLY_DECODE_TRIGGER_SECONDS;
    use super::*;

    // Over the limit used to return Err: the runner read that as a dead
    // socket, skipped commit, and the whole buffered recording was lost.
    // Past the stop, the first part is kept and the rest dropped.
    #[test]
    fn going_over_the_limit_keeps_the_first_part() {
        let max = 100;
        assert_eq!(samples_to_keep(0, 40, max), 40);
        assert_eq!(samples_to_keep(98, 5, max), 2);
        assert_eq!(samples_to_keep(100, 5, max), 0);
    }

    /// The old six-minute stop silently dropped everything after minute six
    /// of a long dictation (three times in the owner's logs). Two hours must
    /// now be accepted in full.
    #[test]
    fn a_two_hour_dictation_is_not_cut_at_six_minutes() {
        let sample_rate = 16_000usize;
        let max = sample_rate * MAX_AUDIO_SECONDS;
        let two_hours = sample_rate * 2 * 60 * 60;
        assert_eq!(samples_to_keep(0, two_hours, max), two_hours);
    }

    /// 16 kHz speech-like noise with a silent quarter second every 10 s, so
    /// the quiet-boundary search has real pauses to land on.
    fn speech_with_pauses(seconds: usize) -> Vec<i16> {
        let sr = 16_000;
        (0..seconds * sr)
            .map(|i| {
                if (i / sr) % 10 == 9 && i % sr < sr / 4 {
                    0
                } else {
                    ((i * 7919) % 4001) as i16 - 2000
                }
            })
            .collect()
    }

    #[test]
    fn early_clips_stay_within_30_to_35_seconds_and_lose_no_audio() {
        let sr = 16_000usize;
        let mut short = speech_with_pauses(EARLY_DECODE_TRIGGER_SECONDS - 1);
        assert!(
            take_early_chunk(&mut short, sr).is_none(),
            "too short to cut yet"
        );

        let mut pending = speech_with_pauses(130);
        let total = pending.len();
        let mut clips = Vec::new();
        while let Some(clip) = take_early_chunk(&mut pending, sr) {
            assert!(clip.len() <= 35 * sr, "clip of {} s", clip.len() / sr);
            assert!(clip.len() >= 30 * sr, "clip of {} s", clip.len() / sr);
            clips.push(clip);
        }
        assert!(clips.len() >= 2);
        assert!(pending.len() < EARLY_DECODE_TRIGGER_SECONDS * sr);
        let kept: usize = clips.iter().map(Vec::len).sum::<usize>() + pending.len();
        assert_eq!(
            kept, total,
            "every sample is in exactly one clip or the tail"
        );
    }

    #[test]
    fn clip_results_join_in_order_and_skip_a_failed_clip() {
        let joined = combine(vec![
            Ok(Some("First part.".into())),
            Err("local transcription was cancelled".into()),
            Ok(Some("Third part.".into())),
            Ok(None),
        ])
        .unwrap()
        .unwrap();
        assert!(joined.starts_with("First part."));
        assert!(joined.ends_with("Third part."));
        assert!(combine(vec![Err("boom".into())]).is_err());
        assert_eq!(combine(vec![Ok(None)]).unwrap(), None);
    }
}
