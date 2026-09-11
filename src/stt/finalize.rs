//! Winding one session down.
//!
//! The fast-fail abort for a key the provider already rejected, and the normal
//! path: enter the release phase, join both tasks under their deadlines,
//! promote a trailing partial if it earns it, then settle the outcome.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::keys::{FailKind, KeyPool};
use crate::state::App;

use super::heuristics::{transcripts_equivalent, transport_failure_lost_speech};
use super::provider::AudioFormat;
use super::recv_task::SessionAccumulators;
use super::{
    audio_duration_ms, deliver_transcript, SentAudio, SessionAbort, SessionUsage, TAIL_MIN,
};

/// Everything the finalize phase (the fast-fail abort, or the normal
/// join/promote/bookkeeping path) needs about the session, independent of
/// which outcome it turns out to be. Bundled for the same reason
/// [`ConnectedSession`](super::connect::ConnectedSession) and [`RecvTaskState`](super::recv_task::RecvTaskState) are: passing this much state
/// as individual function parameters trips clippy's `too_many_arguments`.
pub(super) struct SessionFinalizeCtx {
    pub(super) epoch: u64,
    pub(super) provider_id: &'static str,
    pub(super) keys: Arc<KeyPool>,
    pub(super) key: String,
    pub(super) key_suffix: String,
    pub(super) requires_api_key: bool,
    pub(super) delay_until_release: bool,
    pub(super) log_transcripts: bool,
    pub(super) enable_sound: bool,
    pub(super) fmt: AudioFormat,
    pub(super) sent_progress: Arc<parking_lot::Mutex<SentAudio>>,
    pub(super) acc: SessionAccumulators,
    pub(super) session_usage: Arc<parking_lot::Mutex<SessionUsage>>,
}

/// Fast-fail: the provider already told us (via `SttEvent::KeyFailure`)
/// that the key is dead before the session ever reached release. Skip the
/// entire finalize and hand back to the retry shell to rotate keys. What
/// this attempt had transcribed but not yet pasted goes into `carry` for
/// the next attempt (or the press's end) to deliver.
pub(super) async fn abort_for_early_key_failure(
    ctx: &SessionFinalizeCtx,
    kind: FailKind,
    send_task: tokio::task::JoinHandle<SentAudio>,
    recv_task: tokio::task::JoinHandle<()>,
    carry: &mut Vec<String>,
) -> Result<()> {
    let epoch = ctx.epoch;
    tracing::warn!(
        "session[{epoch}] aborting finalize early -- key ...{} failed ({kind:?})",
        ctx.key_suffix
    );
    // Do not detach either half while the retry shell rotates keys. Besides
    // retaining an obsolete audio subscription, a late receiver could paste
    // into the replacement attempt. Preserve any words that were already
    // live-pasted (delay=false) and the audio known to have been shipped.
    send_task.abort();
    recv_task.abort();
    let _ = send_task.await;
    let _ = recv_task.await;
    let stashed = stash_unpasted(&ctx.acc, carry);
    if stashed > 0 {
        tracing::info!(
            "session[{epoch}] carrying {stashed} unpasted chunk(s) into the next attempt"
        );
    }
    if !ctx.delay_until_release {
        let words = ctx.acc.transcribed_words.load(Ordering::Acquire);
        if words > 0 {
            let sent = *ctx.sent_progress.lock();
            ctx.session_usage.lock().add_fragment(
                ctx.provider_id,
                words,
                audio_duration_ms(sent.samples, ctx.fmt.sample_rate),
            );
        }
    }
    ctx.keys.mark_failed(&ctx.key, kind);
    Err(SessionAbort::KeyRejected.into())
}

/// Move everything an attempt transcribed but never pasted -- its held
/// commits and a trailing partial that is not just the last commit again --
/// onto `carry`. Returns how many chunks moved. Pure over the accumulators,
/// so it is unit-tested without a session.
pub(super) fn stash_unpasted(acc: &SessionAccumulators, carry: &mut Vec<String>) -> usize {
    let mut held = std::mem::take(&mut *acc.chunks_buf.lock());
    let partial = std::mem::take(&mut *acc.last_partial_buf.lock());
    if !partial.is_empty() {
        let last = acc.last_commit_text.lock();
        if !transcripts_equivalent(&last, &partial) {
            held.push(partial);
        }
    }
    let moved = held.len();
    carry.append(&mut held);
    moved
}

/// Seed a fresh attempt with what earlier attempts of the same press carried
/// over: in held mode the text joins the attempt's held commits, so it is
/// pasted in order at release; in live mode it is pasted right away, exactly
/// as the commits would have been had their attempt survived. Returns the
/// number of words seeded, already added to the attempt's word count.
pub(super) fn seed_from_carry(
    app: &Arc<App>,
    ctx: &SessionFinalizeCtx,
    carry: &mut Vec<String>,
) -> u64 {
    if carry.is_empty() {
        return 0;
    }
    let chunks = std::mem::take(carry);
    let words: u64 = chunks
        .iter()
        .map(|c| c.split_whitespace().count() as u64)
        .sum();
    ctx.acc.transcribed_words.fetch_add(words, Ordering::AcqRel);
    ctx.acc.committed_flag.store(true, Ordering::Release);
    tracing::info!(
        "session[{}] seeded with {} chunk(s) ({words} words) carried from an earlier attempt",
        ctx.epoch,
        chunks.len()
    );
    if ctx.delay_until_release {
        ctx.acc.chunks_buf.lock().extend(chunks);
    } else if !app.session_discarded(ctx.epoch) {
        deliver_transcript(&app.transcript_tx, chunks.join(" "));
    }
    words
}

/// Flip the release flag so the send/recv tasks switch into their
/// post-release behavior (dynamic tail; live paste), then flush whatever
/// commits were held pending release so far, so release feels snappy.
pub(super) fn enter_release_phase(
    app: &Arc<App>,
    ctx: &SessionFinalizeCtx,
    tail_quiet: Duration,
    tail_max: Duration,
    release_pending: &Arc<AtomicBool>,
) {
    let epoch = ctx.epoch;
    tracing::info!(
        "session[{epoch}] release pending; entering dynamic tail (min={:?}, quiet={:?}, max={:?})",
        TAIL_MIN,
        tail_quiet,
        tail_max
    );
    // Flip the release flag FIRST so recv switches to live-paste mode for any
    // chunks the server sends from this point on.
    release_pending.store(true, Ordering::Release);

    // Then flush anything held during the session so release feels snappy.
    let release_flush: Vec<String> = std::mem::take(&mut *ctx.acc.chunks_buf.lock());
    if !release_flush.is_empty() {
        let joined = release_flush.join(" ");
        if !app.session_discarded(epoch) {
            tracing::info!(
                "session[{epoch}] release flush: {} chunk(s), {} chars",
                release_flush.len(),
                joined.chars().count()
            );
            deliver_transcript(&app.transcript_tx, joined);
        } else {
            tracing::info!(
                "session[{epoch}] skipping release flush because the user discarded this dictation"
            );
        }
    }
}

/// Join the send task, bounded so a stuck provider can't hang the whole
/// press. On timeout the task is cancelled and we fall back to whatever
/// progress it had already published to `sent_progress`.
pub(super) async fn join_send_task(
    mut send_task: tokio::task::JoinHandle<SentAudio>,
    ctx: &SessionFinalizeCtx,
    send_deadline: Duration,
) -> SentAudio {
    let epoch = ctx.epoch;
    match tokio::time::timeout(send_deadline, &mut send_task).await {
        Ok(Ok(sent)) => sent,
        Ok(Err(e)) => {
            tracing::warn!("session[{epoch}] send_task join error: {e}");
            *ctx.sent_progress.lock()
        }
        Err(_) => {
            tracing::warn!(
                "session[{epoch}] send_task did not finish in {send_deadline:?}; cancelling it"
            );
            send_task.abort();
            let _ = send_task.await;
            *ctx.sent_progress.lock()
        }
    }
}

/// Wait for recv to drain, bounded by the provider's own final-transcript
/// grace period. If it doesn't finish in time, cancel it before the caller
/// inspects the shared accumulators, so it can't emit a second, late final
/// after the last partial has already been promoted.
pub(super) async fn join_recv_task(
    mut recv_task: tokio::task::JoinHandle<()>,
    ctx: &SessionFinalizeCtx,
    final_transcript_timeout: Duration,
) {
    let epoch = ctx.epoch;
    let recv_finished = tokio::time::timeout(final_transcript_timeout, &mut recv_task)
        .await
        .is_ok();
    if !recv_finished {
        tracing::warn!(
            "session[{epoch}] recv_task did not finish within {:?}; cancelling it before promoting any partial",
            final_transcript_timeout
        );
        recv_task.abort();
        let _ = recv_task.await;
    }
}

/// Flush any commit chunks still held (recv pushed one between the release
/// flip and the caller taking the buffer), then decide whether the trailing
/// partial (if any) should be promoted to a real transcript -- or
/// suppressed as a dropped phantom / a repeat of the last commit / stale.
/// Returns whether there was a trailing partial at all, for the caller's
/// no-transcript-at-all diagnostic.
pub(super) fn promote_tail_transcript(app: &Arc<App>, ctx: &SessionFinalizeCtx) -> bool {
    let epoch = ctx.epoch;
    // Sweep once more in case recv pushed a chunk between us flipping
    // release_pending and taking the buffer.
    let held_chunks = std::mem::take(&mut *ctx.acc.chunks_buf.lock());
    let last_partial = std::mem::take(&mut *ctx.acc.last_partial_buf.lock());
    let dropped_phantom = ctx.acc.dropped_phantom_buf.lock().take();

    if !held_chunks.is_empty() {
        let joined = held_chunks.join(" ");
        if !app.session_discarded(epoch) {
            tracing::info!(
                "session[{epoch}] flushing {} held commit chunk(s), {} chars total",
                held_chunks.len(),
                joined.chars().count()
            );
            deliver_transcript(&app.transcript_tx, joined);
        } else {
            tracing::info!(
                "session[{epoch}] skipping held commit flush because the user discarded this dictation"
            );
        }
    }

    // The last-partial fallback is now per SEGMENT, not per session: a kept
    // commit clears the partial buffer, so anything left here is speech that
    // arrived after the last commit and never got finalized (the provider hit
    // `final_transcript_timeout`). Gating it on `!got_committed` used to throw
    // that trailing segment away for the rest of the session as soon as one
    // earlier sentence committed. `got_committed` still guards the
    // "no transcript at all" diagnostic in the caller, which is genuinely per
    // session.
    //
    // It is promoted even when the NEXT press has already started. This is
    // the common shape of a stalled provider: the user sees only the first
    // sentence land at release, presses again within a second to say the
    // rest, and the trailing partial -- the very words that went missing --
    // used to be dropped as "superseded". Only a dictation the user threw
    // away on purpose (the replay long-press) is withheld.
    let had_partial = !last_partial.is_empty();
    let partial_was_dropped_phantom = dropped_phantom
        .as_deref()
        .is_some_and(|phantom| transcripts_equivalent(phantom, &last_partial));
    // Belt and braces: if a provider re-emits the committed text as a trailing
    // partial, promoting it would paste the same words twice.
    let partial_repeats_last_commit = {
        let last = ctx.acc.last_commit_text.lock();
        !last.is_empty() && transcripts_equivalent(&last, &last_partial)
    };
    if had_partial && partial_was_dropped_phantom {
        tracing::info!(
            "session[{epoch}] suppressing last partial because it matches a dropped phantom finalization"
        );
    } else if had_partial && partial_repeats_last_commit {
        tracing::info!(
            "session[{epoch}] suppressing last partial because it repeats the last commit"
        );
    } else if had_partial && !app.session_discarded(epoch) {
        ctx.acc.transcribed_words.fetch_add(
            last_partial.split_whitespace().count() as u64,
            Ordering::AcqRel,
        );
        if ctx.log_transcripts {
            tracing::info!("session[{epoch}] promoting last partial: {last_partial}");
        } else {
            tracing::info!(
                "session[{epoch}] promoting last partial: {} char(s)",
                last_partial.chars().count()
            );
        }
        deliver_transcript(&app.transcript_tx, last_partial);
    } else if had_partial {
        tracing::info!(
            "session[{epoch}] skipping last partial because the user discarded this dictation"
        );
    }

    had_partial
}

/// Session bookkeeping once finalize is done: rotate/credit the key,
/// account any transcribed words toward usage, play the stop chime, and
/// decide the session's overall `Result` -- including the "transport died
/// but didn't cost the user anything" downgrade to `Ok(())`.
pub(super) fn finish_session_outcome(
    ctx: &SessionFinalizeCtx,
    audio_ms: u64,
    speech_chunks: u64,
    sent: &SentAudio,
) -> Result<()> {
    let epoch = ctx.epoch;
    // Happy path only reaches here (fast-fail returned above on failure).
    let key_failure = *ctx.acc.key_fail_kind.lock();
    if let Some(kind) = key_failure {
        ctx.keys.mark_failed(&ctx.key, kind);
        tracing::warn!("session[{epoch}] ended with FAILED key ({kind:?}); pool will rotate");
    } else if ctx.requires_api_key {
        ctx.keys.mark_success(&ctx.key, audio_ms);
    }
    let words = ctx.acc.transcribed_words.load(Ordering::Acquire);
    if words > 0 {
        ctx.session_usage
            .lock()
            .add_fragment(ctx.provider_id, words, audio_ms);
    }
    crate::sound::play_stop(ctx.enable_sound);
    tracing::info!("session[{epoch}] ended");
    if key_failure.is_some() {
        return Err(SessionAbort::KeyRejected.into());
    }
    if let Some(message) = ctx.acc.provider_failure.lock().take() {
        // A transport that died without costing the user anything is a
        // teardown, not a failure. ElevenLabs in particular often drops the TCP
        // connection without a closing handshake, so `recv_event` reports
        // "Connection reset without closing handshake" on sessions that lost
        // nothing at all. Raising the error pip for those is a lie. The point
        // of recording a mid-session transport error is the case where speech
        // was LOST, so gate on exactly that (see
        // `transport_failure_lost_speech`).
        if !transport_failure_lost_speech(words, sent.socket_died) {
            if words > 0 {
                tracing::info!(
                    "session[{epoch}] transport dropped during teardown after delivering \
                     {words} word(s); not surfacing an error ({message})"
                );
            } else {
                tracing::info!(
                    "session[{epoch}] transport dropped on an empty dictation -- the provider \
                     returned no words at all ({speech_chunks} chunk(s) were above our silence \
                     floor), so there is no transcript to lose; not surfacing an error \
                     ({message})"
                );
            }
            return Ok(());
        }
        return Err(anyhow!(message));
    }
    Ok(())
}
