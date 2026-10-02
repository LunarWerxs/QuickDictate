//! Early decoding for long local dictations.
//!
//! A local model transcribes in one batch after release, so a long recording
//! used to keep everything in memory and make the user wait for all of it at
//! the end (and was cut at six minutes to bound both). Instead, once
//! [`EARLY_DECODE_TRIGGER_SECONDS`] of undecoded audio has built up, its first
//! quiet-boundary clip is handed to the model in the background while the
//! speaker keeps talking. Release then only waits for the short tail.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Undecoded audio that triggers an early clip. Above the 35 s clip limit so
/// the quiet-boundary search always has its full five-second window and the
/// cut leaves at least five seconds behind.
pub(super) const EARLY_DECODE_TRIGGER_SECONDS: usize = 40;

/// How often, and how long apart, a clip is retried when the shared worker is
/// momentarily busy (another dictation's tail, a prewarm, or a decode being
/// cancelled holds its one queue slot): up to 10 s in all. Losing a stretch
/// of a long dictation is worse than waiting, and short steps keep the wait
/// after release from rounding up to a quarter second.
const BUSY_RETRIES: usize = 200;
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Split the first finished clip off `pending` once enough audio has built
/// up, leaving the rest in place. `None` while there is not enough yet.
pub(super) fn take_early_chunk(pending: &mut Vec<i16>, sample_rate: usize) -> Option<Vec<i16>> {
    if sample_rate == 0 || pending.len() < sample_rate.saturating_mul(EARLY_DECODE_TRIGGER_SECONDS)
    {
        return None;
    }
    let cut = crate::local_stt::first_clip_end(pending, sample_rate)?;
    let rest = pending.split_off(cut);
    Some(std::mem::replace(pending, rest))
}

/// One background task per dictation that decodes its clips strictly in
/// order, one at a time, so a dictation never holds more than one job on the
/// shared worker and never trips its "busy" refusal against itself.
pub(super) struct EarlyDecoder {
    chunks: mpsc::UnboundedSender<Vec<i16>>,
    done: JoinHandle<Vec<Result<Option<String>, String>>>,
}

impl EarlyDecoder {
    pub(super) fn spawn(
        model_id: String,
        language: String,
        vocabulary: String,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        let (chunks, mut rx) = mpsc::unbounded_channel::<Vec<i16>>();
        let done = tokio::spawn(async move {
            let mut results = Vec::new();
            while let Some(pcm) = rx.recv().await {
                results
                    .push(decode_with_retry(&model_id, &language, &vocabulary, pcm, &cancel).await);
            }
            results
        });
        Self { chunks, done }
    }

    /// Queue a clip behind the ones already sent.
    pub(super) fn push(&self, pcm: Vec<i16>) {
        // The task only ends after `finish` drops the sender, so a send can
        // only fail if it panicked; `finish` reports that.
        let _ = self.chunks.send(pcm);
    }

    /// Queue the final tail, then wait for every clip's result, in order.
    pub(super) async fn finish(self, tail: Vec<i16>) -> Vec<Result<Option<String>, String>> {
        if !tail.is_empty() {
            let _ = self.chunks.send(tail);
        }
        drop(self.chunks);
        self.done
            .await
            .unwrap_or_else(|e| vec![Err(format!("local early-decode task failed: {e}"))])
    }
}

/// Decode `pcm`, waiting out a momentarily busy worker. Stops retrying once
/// `cancel` is set, so an abandoned decode never queues behind live ones.
pub(super) async fn decode_with_retry(
    model_id: &str,
    language: &str,
    vocabulary: &str,
    pcm: Vec<i16>,
    cancel: &Arc<AtomicBool>,
) -> Result<Option<String>, String> {
    let mut attempt = 0;
    loop {
        let result = crate::local_stt::transcribe(
            model_id.to_string(),
            language.to_string(),
            vocabulary.to_string(),
            pcm.clone(),
            Arc::clone(cancel),
        )
        .await;
        match result {
            Err(e)
                if e.contains("busy")
                    && attempt < BUSY_RETRIES
                    && !cancel.load(std::sync::atomic::Ordering::Acquire) =>
            {
                attempt += 1;
                tokio::time::sleep(BUSY_RETRY_DELAY).await;
            }
            other => return other,
        }
    }
}

/// Fold the clips' results into one transcript. A failed clip is logged and
/// skipped so the rest of a long dictation still arrives; only when nothing
/// decoded at all is the first error returned.
pub(super) fn combine(
    results: Vec<Result<Option<String>, String>>,
) -> Result<Option<String>, String> {
    let mut parts = Vec::new();
    let mut first_error = None;
    for (index, result) in results.into_iter().enumerate() {
        match result {
            Ok(Some(text)) => parts.push(text),
            Ok(None) => {}
            Err(e) => {
                tracing::error!(
                    "local dictation clip {} failed and was skipped: {e}",
                    index + 1
                );
                first_error.get_or_insert(e);
            }
        }
    }
    match (crate::local_stt::join_transcripts(parts), first_error) {
        (Some(text), _) => Ok(Some(text)),
        (None, Some(e)) => Err(e),
        (None, None) => Ok(None),
    }
}
