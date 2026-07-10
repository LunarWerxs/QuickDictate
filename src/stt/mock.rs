//! In-process mock provider for deterministic, network-free testing of the
//! provider contract (`connect` → sink → `recv_event`). Scripts a fixed
//! sequence of [`SttEvent`]s so the event-handling plumbing can be exercised
//! without a real API, mic, or socket.
//!
//! Test-only. A `MockProvider` is the seam for future full-`run_session` tests
//! once the audio source is made injectable (today `run_session` opens WASAPI
//! directly, so the network-free coverage stops at the provider boundary; the
//! real end-to-end pipeline is covered by the live tests and `smoke_test.ps1`).

use std::collections::VecDeque;

use async_trait::async_trait;

use super::provider::{
    AudioFormat, ConnectError, Encoding, ProviderSession, ProviderSink, ProviderStream, RecvError,
    SendError, SttEvent, SttProvider, SttSessionOpts,
};

/// A provider that replays a scripted event sequence. `send_audio`/`commit`/
/// `close` are no-ops (recorded), so tests control exactly what the stream
/// half yields.
pub struct MockProvider {
    pub script: Vec<SttEvent>,
}

#[async_trait]
impl SttProvider for MockProvider {
    fn id(&self) -> &'static str {
        "mock"
    }

    fn required_audio_format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: 16_000,
            encoding: Encoding::Pcm16Le,
        }
    }

    async fn connect(
        &self,
        _key: &str,
        _opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        Ok(ProviderSession {
            sink: Box::new(MockSink { audio_chunks: 0 }),
            stream: Box::new(MockStream {
                events: self.script.clone().into(),
            }),
        })
    }
}

struct MockSink {
    audio_chunks: usize,
}

#[async_trait]
impl ProviderSink for MockSink {
    async fn send_audio(&mut self, _pcm: &[i16]) -> Result<(), SendError> {
        self.audio_chunks += 1;
        Ok(())
    }
    async fn commit(&mut self) -> Result<(), SendError> {
        Ok(())
    }
    async fn close(&mut self) -> Result<(), SendError> {
        Ok(())
    }
}

struct MockStream {
    events: VecDeque<SttEvent>,
}

#[async_trait]
impl ProviderStream for MockStream {
    async fn recv_event(&mut self) -> Result<Option<SttEvent>, RecvError> {
        Ok(self.events.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::FailKind;

    /// Collect committed/partial the way the runner's recv loop does.
    async fn drive(provider: &dyn SttProvider) -> (String, String, Option<FailKind>) {
        let opts = SttSessionOpts {
            language: "en".into(),
            sample_rate: 16_000,
            model: None,
        };
        let ProviderSession {
            mut sink,
            mut stream,
        } = provider.connect("k", &opts).await.unwrap();
        sink.send_audio(&[0i16; 1600]).await.unwrap();
        sink.commit().await.unwrap();
        sink.close().await.unwrap();
        let (mut committed, mut last_partial, mut fail) = (String::new(), String::new(), None);
        while let Some(ev) = stream.recv_event().await.unwrap() {
            match ev {
                SttEvent::Committed(t) => {
                    if !committed.is_empty() {
                        committed.push(' ');
                    }
                    committed.push_str(&t);
                }
                SttEvent::Partial(t) => last_partial = t,
                SttEvent::KeyFailure(k) => fail = Some(k),
                SttEvent::Closed(_) => break,
                SttEvent::SessionStarted => {}
            }
        }
        (committed, last_partial, fail)
    }

    #[tokio::test]
    async fn scripts_partials_and_commit() {
        let provider = MockProvider {
            script: vec![
                SttEvent::SessionStarted,
                SttEvent::Partial("the quick".into()),
                SttEvent::Partial("the quick brown fox".into()),
                SttEvent::Committed("the quick brown fox".into()),
                SttEvent::Closed(None),
            ],
        };
        let (committed, last_partial, fail) = drive(&provider).await;
        assert_eq!(committed, "the quick brown fox");
        assert_eq!(last_partial, "the quick brown fox");
        assert!(fail.is_none());
    }

    #[tokio::test]
    async fn scripts_multiple_commits_and_failure() {
        let provider = MockProvider {
            script: vec![
                SttEvent::Committed("first".into()),
                SttEvent::Committed("second".into()),
                SttEvent::KeyFailure(FailKind::Exhausted),
                SttEvent::Closed(None),
            ],
        };
        let (committed, _partial, fail) = drive(&provider).await;
        assert_eq!(committed, "first second");
        assert_eq!(fail, Some(FailKind::Exhausted));
    }
}
