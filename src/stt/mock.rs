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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use super::provider::{
    AudioFormat, ConnectError, ProviderSession, ProviderSink, ProviderStream, RecvError, SendError,
    SttEvent, SttProvider, SttSessionOpts,
};

/// A provider that replays a scripted event sequence. `send_audio`/`commit`/
/// `close` are no-ops (recorded), so tests control exactly what the stream
/// half yields. `connects` and `sent_chunks` count across every session the
/// provider opened, so a test can see a reconnect and what was replayed.
#[derive(Default)]
pub struct MockProvider {
    pub script: Vec<SttEvent>,
    pub connects: Arc<AtomicUsize>,
    pub sent_chunks: Arc<AtomicUsize>,
    /// When set, the FIRST session's sink refuses every send after accepting
    /// this many chunks, the way a socket the server closed mid-press does.
    /// Later sessions (replacements) accept everything.
    pub first_socket_dies_after: Option<usize>,
    /// Once the script runs out, keep the stream open and silent (a healthy
    /// server with nothing to say) instead of ending it.
    pub hold_open: bool,
}

#[async_trait]
impl SttProvider for MockProvider {
    fn id(&self) -> &'static str {
        "mock"
    }

    fn required_audio_format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: 16_000,
        }
    }

    async fn connect(
        &self,
        _key: &str,
        _opts: &SttSessionOpts,
    ) -> Result<ProviderSession, ConnectError> {
        let first = self.connects.fetch_add(1, Ordering::AcqRel) == 0;
        Ok(ProviderSession {
            sink: Box::new(MockSink {
                audio_chunks: Arc::clone(&self.sent_chunks),
                accepts: self.first_socket_dies_after.filter(|_| first),
            }),
            stream: Box::new(MockStream {
                events: self.script.clone().into(),
                hold_open: self.hold_open,
            }),
        })
    }
}

struct MockSink {
    audio_chunks: Arc<AtomicUsize>,
    /// Sends left before this socket "closes"; `None` never closes.
    accepts: Option<usize>,
}

#[async_trait]
impl ProviderSink for MockSink {
    async fn send_audio(&mut self, _pcm: &[i16]) -> Result<(), SendError> {
        if let Some(left) = self.accepts.as_mut() {
            if *left == 0 {
                return Err(SendError("mock: the server closed this socket".into()));
            }
            *left -= 1;
        }
        self.audio_chunks.fetch_add(1, Ordering::AcqRel);
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
    hold_open: bool,
}

#[async_trait]
impl ProviderStream for MockStream {
    async fn recv_event(&mut self) -> Result<Option<SttEvent>, RecvError> {
        match self.events.pop_front() {
            None if self.hold_open => std::future::pending().await,
            next => Ok(next),
        }
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
            custom_vocabulary: Vec::new(),
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
                SttEvent::ProviderFailure(_) => {}
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
            ..Default::default()
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
            ..Default::default()
        };
        let (committed, _partial, fail) = drive(&provider).await;
        assert_eq!(committed, "first second");
        assert_eq!(fail, Some(FailKind::Exhausted));
    }
}
