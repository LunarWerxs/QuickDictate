//! Keyless, fully local batch transcription via the optional model packs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::provider::{
    AudioFormat, ConnectError, ProviderSession, ProviderSink, ProviderStream, RecvError, SendError,
    SttEvent, SttProvider, SttSessionOpts,
};

/// Cohere's positional table tops out around 400 seconds. Staying below that
/// also bounds session PCM to 5.76 million i16 samples (~11 MiB).
const MAX_AUDIO_SECONDS: usize = 360;

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
                sample_rate: opts.sample_rate,
                pcm: Vec::new(),
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
    sample_rate: u32,
    pcm: Vec<i16>,
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
        let max_samples = self.sample_rate as usize * MAX_AUDIO_SECONDS;
        let room = max_samples.saturating_sub(self.pcm.len());
        self.pcm.extend_from_slice(&pcm[..pcm.len().min(room)]);
        if pcm.len() > room && !self.limit_hit {
            self.limit_hit = true;
            tracing::warn!(
                "local dictation reached the {MAX_AUDIO_SECONDS}-second safety limit; \
                 transcribing the first {MAX_AUDIO_SECONDS} s and dropping the rest"
            );
        }
        Ok(())
    }

    async fn commit(&mut self) -> Result<(), SendError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let pcm = std::mem::take(&mut self.pcm);
        let result = crate::local_stt::transcribe(
            self.model_id.clone(),
            self.language.clone(),
            pcm,
            Arc::clone(&self.cancel),
        )
        .await;
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
    use super::*;

    fn sink(sample_rate: u32) -> LocalSink {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        LocalSink {
            model_id: "test".into(),
            language: "en".into(),
            sample_rate,
            pcm: Vec::new(),
            event_tx: Some(event_tx),
            cancel: Arc::new(AtomicBool::new(false)),
            finished: false,
            limit_hit: false,
        }
    }

    // Over the limit used to return Err: the runner read that as a dead
    // socket, skipped commit, and the whole buffered recording was lost.
    #[tokio::test]
    async fn going_over_the_limit_keeps_the_first_part_and_the_session() {
        // One sample per second makes the limit MAX_AUDIO_SECONDS samples.
        let mut sink = sink(1);
        sink.send_audio(&vec![1; MAX_AUDIO_SECONDS - 2])
            .await
            .unwrap();
        sink.send_audio(&[2; 5]).await.unwrap();
        sink.send_audio(&[3; 5]).await.unwrap();
        assert_eq!(sink.pcm.len(), MAX_AUDIO_SECONDS);
        assert_eq!(&sink.pcm[MAX_AUDIO_SECONDS - 3..], &[1, 2, 2]);
        assert!(sink.limit_hit);
    }
}
