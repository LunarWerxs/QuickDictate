//! Keyless, fully local batch transcription via the optional model packs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::heuristics::rms_i16;
use super::local_early::{combine, decode_with_retry, take_early_chunk, EarlyDecoder};
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

/// A pause this long, once `PAUSE_CLIP_MIN_SECONDS` is pending, ends a clip:
/// the audio up to the middle of the pause goes to the model for good while
/// the speaker carries on, so the release only waits for what was said since
/// the last pause. Every stretch is decoded exactly once. (Decoding the
/// pending audio at each pause and throwing it away when speech resumed was
/// tried first: a running GPU decode does not stop when cancelled, so the
/// discarded ones queued ahead of the one that mattered and made stopping
/// mid-sentence slower, 1.7 s against 1.6 s, on a 30 s recording.)
const PAUSE_CUT_QUIET_MS: usize = 400;
const PAUSE_CLIP_MIN_SECONDS: usize = 20;

/// A chunk is silence, for finding those pauses, below three times the
/// quietest chunk so far (the microphone's own noise floor), kept within
/// these bounds. Far below `SILENCE_RMS` on purpose: soft speech has to count
/// as sound here, or a clip would end mid-word. Audio after the last cut that
/// never rises above the lower bound is skipped as silence.
const QUIET_RMS_MIN: i32 = 150;
const QUIET_RMS_MAX: i32 = 1000;

fn quiet_threshold(noise_floor: i32) -> i32 {
    noise_floor
        .saturating_mul(3)
        .clamp(QUIET_RMS_MIN, QUIET_RMS_MAX)
}

/// The loudest 100 ms of `pcm`, by RMS.
fn loudest_chunk_rms(pcm: &[i16]) -> i32 {
    pcm.chunks(1_600).map(rms_i16).max().unwrap_or(0)
}

/// A decode of `pcm[..covers]` started at the release, while the tail listens.
/// Speech in the tail cancels it.
struct Speculation {
    covers: usize,
    cancel: Arc<AtomicBool>,
    result: tokio::task::JoinHandle<Result<Option<String>, String>>,
}

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
                spec: None,
                noise_floor: i32::MAX,
                trailing_quiet: 0,
                sound_since_cut: false,
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
    /// A decode of the pending audio started ahead of `commit`.
    spec: Option<Speculation>,
    /// The quietest chunk so far (RMS): the microphone's noise floor.
    noise_floor: i32,
    /// Silent samples at the end of `pcm`, by [`quiet_threshold`].
    trailing_quiet: usize,
    /// Sound has arrived since the last pause cut.
    sound_since_cut: bool,
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
        self.abandon_spec();
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
        self.note_loudness(&pcm[..keep]);
        let clip = match take_early_chunk(&mut self.pcm, self.sample_rate as usize) {
            Some(clip) => Some(clip),
            None => self.take_pause_clip(),
        };
        if let Some(clip) = clip {
            // A decode started at the release no longer matches the pending audio.
            self.abandon_spec();
            self.trailing_quiet = self.trailing_quiet.min(self.pcm.len());
            self.push_clip(clip);
        }
        Ok(())
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let pcm = std::mem::take(&mut self.pcm);
        let tail = self.decode_pending(pcm).await;
        let result = match self.early.take() {
            // A long dictation: its earlier clips are already decoded or in
            // flight, so only the tail was left to wait for.
            Some(early) => {
                let mut results = early.finish(Vec::new()).await;
                results.push(tail);
                combine(results)
            }
            None => tail,
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

    /// Start decoding what is left at the release, so the tail listens while
    /// the model works. Nothing is left when only silence followed the last
    /// pause cut.
    async fn released(&mut self) {
        if self.spec.is_none() && !self.only_silence_left(&self.pcm) {
            self.speculate();
        }
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

    /// Track the noise floor and the trailing silence. Sound cancels a decode
    /// started at the release: it is now short of the words.
    fn note_loudness(&mut self, chunk: &[i16]) {
        if chunk.is_empty() {
            return;
        }
        let rms = rms_i16(chunk);
        self.noise_floor = self.noise_floor.min(rms);
        if rms < quiet_threshold(self.noise_floor) {
            self.trailing_quiet += chunk.len();
        } else {
            self.trailing_quiet = 0;
            self.sound_since_cut = true;
            if let Some(spec) = self.spec.take() {
                spec.cancel.store(true, Ordering::Release);
            }
        }
    }

    /// The pending audio up to the middle of a pause that just reached
    /// `PAUSE_CUT_QUIET_MS`, once enough is pending to be worth a clip.
    fn take_pause_clip(&mut self) -> Option<Vec<i16>> {
        let sr = self.sample_rate as usize;
        if !self.sound_since_cut
            || self.trailing_quiet < sr * PAUSE_CUT_QUIET_MS / 1000
            || self.pcm.len() < sr * PAUSE_CLIP_MIN_SECONDS
        {
            return None;
        }
        let rest = self.pcm.split_off(self.pcm.len() - self.trailing_quiet / 2);
        self.sound_since_cut = false;
        Some(std::mem::replace(&mut self.pcm, rest))
    }

    /// Queue a finished stretch behind the ones already sent.
    fn push_clip(&mut self, clip: Vec<i16>) {
        if self.early.is_none() {
            tracing::info!("local dictation: transcribing finished stretches while it is spoken");
            self.early = Some(EarlyDecoder::spawn(
                self.model_id.clone(),
                self.language.clone(),
                self.vocabulary.clone(),
                Arc::clone(&self.cancel),
            ));
        }
        if let Some(early) = &self.early {
            early.push(clip);
        }
    }

    /// Clips already went to the model and nothing audible came after the
    /// last one. Only then is the rest skipped: a whole dictation is always
    /// decoded, however quiet the microphone.
    fn only_silence_left(&self, pcm: &[i16]) -> bool {
        self.early.is_some() && loudest_chunk_rms(pcm) < QUIET_RMS_MIN
    }

    /// Decode the pending audio in the background, ahead of `commit`.
    fn speculate(&mut self) {
        if self.pcm.is_empty() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (model_id, language, vocabulary) = (
            self.model_id.clone(),
            self.language.clone(),
            self.vocabulary.clone(),
        );
        let (pcm, job_cancel) = (self.pcm.clone(), Arc::clone(&cancel));
        let result = tokio::spawn(async move {
            decode_with_retry(&model_id, &language, &vocabulary, pcm, &job_cancel).await
        });
        self.spec = Some(Speculation {
            covers: self.pcm.len(),
            cancel,
            result,
        });
    }

    fn abandon_spec(&mut self) {
        if let Some(spec) = self.spec.take() {
            spec.cancel.store(true, Ordering::Release);
        }
    }

    /// Transcribe the audio not yet handed to the model, reusing the decode
    /// that started at the release. Speech after it cancels it, so at most
    /// faint sound can follow it; anything above the quiet floor is decoded
    /// on its own and joined. A decode that failed or was cancelled is redone
    /// whole.
    async fn decode_pending(&mut self, pcm: Vec<i16>) -> Result<Option<String>, String> {
        if let Some(spec) = self.spec.take() {
            let covers = spec.covers;
            if covers <= pcm.len() {
                if let Ok(Ok(head)) = spec.result.await {
                    let rest = &pcm[covers..];
                    if loudest_chunk_rms(rest) < QUIET_RMS_MIN {
                        return Ok(head);
                    }
                    let rest = decode_with_retry(
                        &self.model_id,
                        &self.language,
                        &self.vocabulary,
                        rest.to_vec(),
                        &self.cancel,
                    )
                    .await?;
                    return Ok(crate::local_stt::join_transcripts(
                        head.into_iter().chain(rest).collect(),
                    ));
                }
            } else {
                spec.cancel.store(true, Ordering::Release);
            }
        }
        if self.only_silence_left(&pcm) {
            return Ok(None);
        }
        decode_with_retry(
            &self.model_id,
            &self.language,
            &self.vocabulary,
            pcm,
            &self.cancel,
        )
        .await
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

    /// A sink whose model does not exist, so a started decode fails at once
    /// without loading anything.
    fn test_sink() -> LocalSink {
        LocalSink {
            model_id: "no-such-model".into(),
            language: "en".into(),
            vocabulary: String::new(),
            sample_rate: 16_000,
            pcm: Vec::new(),
            received: 0,
            early: None,
            spec: None,
            noise_floor: i32::MAX,
            trailing_quiet: 0,
            sound_since_cut: false,
            event_tx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            finished: false,
            limit_hit: false,
        }
    }

    /// `n` chunks of 100 ms at roughly `level` RMS.
    async fn feed(sink: &mut LocalSink, n: usize, level: i16) {
        for _ in 0..n {
            let chunk: Vec<i16> = (0..1_600)
                .map(|i| if i % 2 == 0 { level } else { -level })
                .collect();
            sink.send_audio(&chunk).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_pause_once_enough_is_pending_sends_the_stretch_to_the_model() {
        let mut sink = test_sink();
        feed(&mut sink, 3, 20).await; // room noise sets the floor
        feed(&mut sink, PAUSE_CLIP_MIN_SECONDS * 10, 3_000).await; // speech
        feed(&mut sink, 3, 20).await;
        assert!(sink.early.is_none(), "300 ms is still mid-sentence");
        feed(&mut sink, 1, 20).await;
        assert!(sink.early.is_some(), "a 400 ms pause ends the clip");
        assert_eq!(sink.pcm.len(), 3_200, "cut in the middle of the pause");
        // Soft speech, far below the tail's 1500 SILENCE_RMS, still counts.
        feed(&mut sink, 1, 400).await;
        assert_eq!(sink.trailing_quiet, 0);
        assert!(sink.sound_since_cut);

        let mut short = test_sink();
        feed(&mut short, 3, 20).await;
        feed(&mut short, PAUSE_CLIP_MIN_SECONDS * 10 - 20, 3_000).await;
        feed(&mut short, 6, 20).await; // a real pause, 1.1 s short of the minimum
        assert!(
            short.early.is_none(),
            "too little pending to be worth a clip"
        );
    }

    #[tokio::test]
    async fn release_decodes_what_is_left_and_tail_speech_cancels_it() {
        let mut sink = test_sink();
        feed(&mut sink, 3, 20).await;
        feed(&mut sink, 50, 3_000).await;
        sink.released().await;
        let spec = sink.spec.as_ref().expect("the release starts the decode");
        assert_eq!(spec.covers, sink.pcm.len());
        let cancel = Arc::clone(&spec.cancel);
        feed(&mut sink, 1, 3_000).await; // a word in the tail
        assert!(sink.spec.is_none());
        assert!(cancel.load(Ordering::Acquire));

        // Everything was cut at a pause and only silence followed it.
        let mut cut = test_sink();
        feed(&mut cut, 3, 20).await;
        feed(&mut cut, PAUSE_CLIP_MIN_SECONDS * 10, 3_000).await;
        feed(&mut cut, 6, 20).await;
        assert!(cut.only_silence_left(&cut.pcm));
        cut.released().await;
        assert!(cut.spec.is_none(), "nothing left to decode");

        // A whole dictation is decoded however quiet the microphone is.
        let mut faint = test_sink();
        feed(&mut faint, 30, 100).await;
        assert!(!faint.only_silence_left(&faint.pcm));
    }

    #[test]
    fn the_quiet_floor_follows_the_microphone_within_bounds() {
        assert_eq!(quiet_threshold(20), QUIET_RMS_MIN);
        assert_eq!(quiet_threshold(200), 600);
        assert_eq!(quiet_threshold(1_100), QUIET_RMS_MAX);
        assert_eq!(loudest_chunk_rms(&[0; 3_200]), 0);
        let mut pcm = vec![0i16; 3_200];
        pcm[1_600..].fill(500);
        assert_eq!(loudest_chunk_rms(&pcm), 500);
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
