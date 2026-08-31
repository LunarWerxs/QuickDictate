//! Tests for the provider-agnostic session runner.

use super::heuristics::{
    is_phantom_finalization, looks_like_short_answer, transcripts_equivalent,
    transport_failure_lost_speech,
};
use super::send_task::TailSilenceGate;
use super::{audio_duration_ms, SentAudio, SessionUsage};

#[test]
fn short_answer_detector_matches_observed_phantoms() {
    // Every phantom shape observed during the original Scribe investigation.
    for p in [
        "Yes.",
        "No.",
        "Yeah.",
        "Okay.",
        "Sure.",
        "Like...",
        "Absolutely.",
        "I think so.",
    ] {
        assert!(
            looks_like_short_answer(p),
            "{p:?} should read as a phantom answer"
        );
    }
}

#[test]
fn short_answer_detector_spares_real_sentences() {
    // A real trailing clause a slow VAD commit might deliver post-release
    // must never be eaten, even if the count-based check misfires.
    assert!(!looks_like_short_answer(
        "Can we make them properly sized instead of super wide?"
    ));
    assert!(!looks_like_short_answer(
        "please refactor this whole function"
    ));
}

#[test]
fn phantom_guard_drops_post_release_commit_with_no_new_speech() {
    // The bug: question committed pre-release at speech=15; release; the tail
    // ships nothing; Scribe finalizes "Yes." while the count is still 15.
    assert!(is_phantom_finalization(true, 15, 15));
}

#[test]
fn phantom_guard_keeps_a_genuinely_spoken_trailing_word() {
    // A real trailing "Yes." ships at least one speech chunk first (16 > 15),
    // so it must NOT be dropped.
    assert!(!is_phantom_finalization(true, 16, 15));
}

#[test]
fn phantom_guard_never_touches_pre_release_commits() {
    // Before release, mid-utterance VAD commits are held and always kept,
    // regardless of the speech counts.
    assert!(!is_phantom_finalization(false, 15, 15));
    assert!(!is_phantom_finalization(false, 0, 0));
}

#[test]
fn phantom_guard_keeps_words_flushed_by_a_mid_sentence_release() {
    // Released mid-sentence: the final real words shipped in the live phase
    // (speech=20) but VAD never committed them (last commit still at 0). The
    // manual commit flushes them post-release; new speech since the last
    // commit means this is real, not a phantom.
    assert!(!is_phantom_finalization(true, 20, 0));
}

#[test]
fn an_empty_dictation_never_raises_the_error_pip() {
    // The reported bug: start a dictation, say nothing, stop. ElevenLabs
    // resets the socket without a closing handshake, but the provider
    // returned no words, so there is no transcript to lose and no "!".
    assert!(!transport_failure_lost_speech(0, false));
}

#[test]
fn room_noise_on_an_empty_press_is_still_an_empty_press() {
    // Same call, stated separately because it is the case that made the
    // first fix wrong: a silent press in a room with a TV on ships plenty
    // of chunks above SILENCE_RMS, and ElevenLabs still returns nothing.
    // Zero words back means an empty press however loud the room was.
    assert!(!transport_failure_lost_speech(0, false));
}

#[test]
fn a_socket_that_died_under_us_still_raises_the_pip() {
    // Cut off mid-press: whatever was said from that point never reached
    // the provider, so the user really did lose speech and should see it.
    assert!(transport_failure_lost_speech(0, true));
}

#[test]
fn a_reset_after_the_words_landed_is_teardown_not_failure() {
    // The pre-existing guard, unchanged: the user watched their sentence
    // get typed, so how the socket closed afterwards is not their problem.
    assert!(!transport_failure_lost_speech(27, false));
    assert!(!transport_failure_lost_speech(27, true));
}

#[test]
fn dropped_phantom_matches_its_partial_fallback() {
    assert!(transcripts_equivalent(" 100%   Go. ", "100% go."));
    assert!(transcripts_equivalent("Okay…", "OKAY!"));
    assert!(!transcripts_equivalent("100% Go.", "100% Go. now"));
    assert!(!transcripts_equivalent("...", ""));
}

#[test]
fn audio_duration_uses_samples_not_assumed_chunk_length() {
    assert_eq!(audio_duration_ms(16_000, 16_000), 1_000);
    assert_eq!(audio_duration_ms(24_000, 24_000), 1_000);
    assert_eq!(audio_duration_ms(1_200, 24_000), 50);
    assert_eq!(audio_duration_ms(1_200, 0), 0);

    let mut sent = SentAudio::default();
    sent.record_prefix(&[vec![0; 1_600], vec![0; 800]], 2);
    assert_eq!(sent.chunks, 2);
    assert_eq!(sent.samples, 2_400);
    assert_eq!(audio_duration_ms(sent.samples, 16_000), 150);
}

#[test]
fn retry_fragments_form_one_physical_dictation_total() {
    let mut usage = SessionUsage::default();
    usage.add_fragment("openai", 40, 12_000);
    usage.add_fragment("openai", 15, 4_000);
    assert_eq!(
        usage,
        SessionUsage {
            provider: "openai".into(),
            words: 55,
            audio_ms: 16_000,
        }
    );
}

#[test]
fn speech_with_no_held_pause_ships_immediately_and_alone() {
    let mut g = TailSilenceGate::default();
    let out = g.offer(vec![9000; 4], true);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], 9000);
    assert_eq!(g.held(), 0);
}

#[test]
fn silence_is_held_back_not_shipped() {
    let mut g = TailSilenceGate::default();
    assert!(g.offer(vec![0; 4], false).is_empty());
    assert!(g.offer(vec![1; 4], false).is_empty());
    assert_eq!(g.held(), 2);
}

#[test]
fn resumed_speech_flushes_the_held_pause_in_order_then_the_speech() {
    // A genuine mid-utterance pause must reach the provider verbatim so the
    // words after it aren't spliced onto the words before it.
    let mut g = TailSilenceGate::default();
    g.offer(vec![10; 1], false); // pause chunk A
    g.offer(vec![20; 1], false); // pause chunk B
    let out = g.offer(vec![9000; 1], true); // speech resumes
    assert_eq!(out.len(), 3);
    assert_eq!(out[0][0], 10); // A first
    assert_eq!(out[1][0], 20); // then B
    assert_eq!(out[2][0], 9000); // then the speech chunk
    assert_eq!(g.held(), 0); // buffer drained on flush
}

#[test]
fn trailing_silence_never_followed_by_speech_is_never_emitted() {
    // This is the whole point: the run of silence after the last real word
    // stays held, so the caller discards it and the model never sees dead
    // air to finalize into a hallucinated "Yes."
    let mut g = TailSilenceGate::default();
    assert_eq!(g.offer(vec![9000; 1], true).len(), 1); // last real word ships
    assert!(g.offer(vec![0; 1], false).is_empty());
    assert!(g.offer(vec![0; 1], false).is_empty());
    assert!(g.offer(vec![0; 1], false).is_empty());
    assert_eq!(g.held(), 3); // all held; caller drops them, none sent
}

#[test]
fn alternating_speech_resets_the_held_run_each_time() {
    let mut g = TailSilenceGate::default();
    g.offer(vec![9000; 1], true); // speech -> ships, nothing held
    assert_eq!(g.held(), 0);
    g.offer(vec![0; 1], false); // 1 held
    assert_eq!(g.held(), 1);
    let out = g.offer(vec![9000; 1], true); // speech again -> flush 1 + speech
    assert_eq!(out.len(), 2);
    assert_eq!(g.held(), 0);
}
