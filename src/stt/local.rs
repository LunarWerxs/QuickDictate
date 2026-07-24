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
}

impl Drop for LocalSink {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

#[async_trait]
impl ProviderSink for LocalSink {
    async fn send_audio(&mut self, pcm: &[i16]) -> Result<(), SendError> {
        let max_samples = self.sample_rate as usize * MAX_AUDIO_SECONDS;
        if self.pcm.len().saturating_add(pcm.len()) > max_samples {
            let message =
                format!("local dictation exceeded the {MAX_AUDIO_SECONDS}-second safety limit");
            if let Some(tx) = &self.event_tx {
                let _ = tx.send(SttEvent::ProviderFailure(message.clone()));
            }
            return Err(SendError(message));
        }
        self.pcm.extend_from_slice(pcm);
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

    #[test]
    fn local_pcm_bound_stays_small() {
        assert_eq!(16_000 * MAX_AUDIO_SECONDS * size_of::<i16>(), 11_520_000);
    }
}
