//! The session's pure decisions, kept apart from the tasks that ask them.
//!
//! Each is a total function of its arguments, so each has a unit test that
//! needs no provider, no socket and no audio device.

/// True when a committed transcript is a hallucinated end-of-stream
/// finalization rather than something the user actually said.
///
/// ElevenLabs Scribe (`scribe_v2_realtime`, `commit_strategy=vad`) will, when an
/// utterance is finalized, occasionally emit its language-model prior "answer"
/// to the preceding question as a fresh `committed_transcript` -- ask "should we
/// do X?", stop, and it commits "Yes." -- even when we send it no trailing audio
/// at all. The tell is that **no speech-bearing audio was shipped to the
/// provider between the previous commit and this one**: `speech_now` (the running
/// speech-chunk count) still equals `speech_at_last_commit` (its value at the
/// last kept commit).
///
/// We only judge this **after release** (`released`): before release, mid-
/// utterance VAD commits are held/accumulated and must always be kept. And
/// because a genuinely-spoken trailing word ships speech first (advancing
/// `speech_now`), a real "Yes." is never mistaken for the phantom -- only a
/// commit conjured out of silence is dropped.
#[inline]
pub(super) fn is_phantom_finalization(
    released: bool,
    speech_now: u64,
    speech_at_last_commit: u64,
) -> bool {
    released && speech_now == speech_at_last_commit
}

/// Did a mid-session transport failure actually cost the user any speech?
/// Only when it did is the red "!" pip honest. Three cases:
///
/// * `words > 0` -- the transcript already landed and the user watched it get
///   typed. The reset is ElevenLabs hanging up after a job well done.
/// * `words == 0` and the send half never saw the socket die -- the press
///   produced nothing this app judged worth typing. Either the provider never
///   emitted a single word, or everything it emitted was discarded as a
///   phantom finalization. Both are EMPTY presses: start a dictation, say
///   nothing, stop. There is no transcript to lose, so the reset costs the
///   user nothing and a red "!" is complaining about a press they already
///   know was empty.
/// * `words == 0` and the socket died under the send half -- we were cut off
///   mid-press, so anything said from that moment on never even reached the
///   provider. That is a real failure and keeps the pip.
///
/// Note what is deliberately NOT consulted: our own count of speech-bearing
/// (RMS >= [`SILENCE_RMS`](super::SILENCE_RMS)) chunks. Measured over ten consecutive silent
/// presses, three of them shipped 3-17 "speech-bearing" chunks of room noise
/// and ElevenLabs returned not one partial for any of them -- so a bare RMS
/// floor is a far worse judge of "did a human say something" than the
/// provider's own verdict, and gating the pip on it just moved the false
/// alarms around. The count is still logged next to the chunk totals, because
/// it is exactly what you want when diagnosing a press after the fact.
#[inline]
pub(super) fn transport_failure_lost_speech(words: u64, socket_died: bool) -> bool {
    words == 0 && socket_died
}

pub(super) fn transcripts_equivalent(left: &str, right: &str) -> bool {
    fn normalized(text: &str) -> String {
        text.chars()
            .filter(|ch| ch.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    }

    let left = normalized(left);
    !left.is_empty() && left == normalized(right)
}

/// Upper bounds on what the phantom guard is willing to drop. The hallucinated
/// "answer" is always a tiny interjection ("Yes.", "No.", "Okay.", "Like...",
/// "Absolutely."); real dictation flushed at finalize is a fuller clause.
const PHANTOM_MAX_WORDS: usize = 4;
const PHANTOM_MAX_CHARS: usize = 24;

/// Secondary gate on the phantom drop (the primary being "no speech shipped
/// since the last commit"): is `text` short enough to *be* a phantom answer
/// rather than real dictated content? This bounds the count/commit attribution
/// race (a slow VAD commit delivering a real segment post-release could look
/// phantom by count alone) so the guard can never silently eat a real sentence
/// -- only ever a plausible answer. See the Committed arm.
#[inline]
pub(super) fn looks_like_short_answer(text: &str) -> bool {
    let t = text.trim();
    t.chars().count() <= PHANTOM_MAX_CHARS && t.split_whitespace().count() <= PHANTOM_MAX_WORDS
}

/// Root-mean-square amplitude of an i16 buffer. Cheap (one pass, integer math
/// + one sqrt). Distinguishes "still talking" from "ambient noise" in the tail.
#[inline]
pub(super) fn rms_i16(samples: &[i16]) -> i32 {
    if samples.is_empty() {
        return 0;
    }
    let mut sum: i64 = 0;
    for &s in samples {
        let v = s as i64;
        sum += v * v;
    }
    let mean = sum / samples.len() as i64;
    (mean as f64).sqrt() as i32
}
