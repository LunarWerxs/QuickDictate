//! One latency record per dictation, appended to `logs/spans.jsonl`.
//!
//! Every milestone is a monotonic millisecond offset from key-down (the moment
//! `stt::start_session` ran), so the numbers add up and compare across
//! dictations. The record carries counts that explain the offsets (STT messages
//! received, polish tokens in/out, audio bytes sent) and NEVER any dictated
//! text. Report with `python scripts/spans_report.py`.
//!
//! One trace is live at a time. A newer press flushes the previous one with
//! whatever it had (missing milestones are `null`), so overlapping dictations
//! cost accuracy, never a lost line. Every call is best-effort and silent.

use std::io::Write;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde_json::{json, Value};

/// A milestone of one dictation, measured from key-down.
#[derive(Clone, Copy)]
pub(crate) enum Mark {
    /// The provider connection is open.
    Connected,
    /// The user let go of the hotkey.
    KeyRelease,
    /// The STT send half is done (listening tail included).
    SttTail,
    /// The final transcript was handed to the output thread.
    FinalTranscript,
    /// The polish pass came back (or was skipped) for the last paste.
    PolishDone,
    /// The last paste of the dictation finished.
    PasteDone,
}

/// A count that explains the offsets.
#[derive(Clone, Copy)]
pub(crate) enum Count {
    SttMessages,
    PolishTokensIn,
    PolishTokensOut,
    AudioBytesSent,
}

#[derive(Default)]
struct Trace {
    start: Option<Instant>,
    unix_ms: u64,
    epoch: u64,
    provider: String,
    marks: [Option<u64>; 6],
    counts: [u64; 4],
}

static TRACE: Mutex<Option<Trace>> = Mutex::new(None);
static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

fn path() -> &'static std::path::PathBuf {
    PATH.get_or_init(|| crate::logging::logs_dir().join("spans.jsonl"))
}

/// Key-down: start a trace for `epoch`, flushing any unfinished previous one.
pub(crate) fn begin(epoch: u64) {
    let previous = TRACE.lock().replace(Trace {
        start: Some(Instant::now()),
        unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        epoch,
        ..Trace::default()
    });
    if let Some(previous) = previous {
        write_record(&previous);
    }
}

pub(crate) fn set_provider(id: &str) {
    if let Some(t) = TRACE.lock().as_mut() {
        t.provider = id.to_string();
    }
}

/// Record `mark` at its first occurrence, except `PolishDone` and `PasteDone`,
/// which keep the LAST (a hybrid paste splits one press into several).
pub(crate) fn mark(mark: Mark) {
    let flush = {
        let mut guard = TRACE.lock();
        let Some(t) = guard.as_mut() else { return };
        let Some(start) = t.start else { return };
        let ms = start.elapsed().as_millis() as u64;
        let slot = &mut t.marks[mark as usize];
        if slot.is_none() || matches!(mark, Mark::PolishDone | Mark::PasteDone) {
            *slot = Some(ms);
        }
        // Done once the last paste has landed after the final transcript.
        let done = t.marks[Mark::FinalTranscript as usize].is_some()
            && t.marks[Mark::PasteDone as usize].is_some();
        if done {
            guard.take()
        } else {
            None
        }
    };
    if let Some(t) = flush {
        write_record(&t);
    }
}

pub(crate) fn add(count: Count, n: u64) {
    if let Some(t) = TRACE.lock().as_mut() {
        t.counts[count as usize] = t.counts[count as usize].saturating_add(n);
    }
}

/// Write out an unfinished trace (shutdown).
pub(crate) fn flush() {
    if let Some(t) = TRACE.lock().take() {
        write_record(&t);
    }
}

fn record(t: &Trace) -> Value {
    json!({
        "v": 1,
        "ts_ms": t.unix_ms,
        "epoch": t.epoch,
        "provider": t.provider,
        "connected_ms": t.marks[Mark::Connected as usize],
        "key_release_ms": t.marks[Mark::KeyRelease as usize],
        "stt_tail_ms": t.marks[Mark::SttTail as usize],
        "final_transcript_ms": t.marks[Mark::FinalTranscript as usize],
        "polish_done_ms": t.marks[Mark::PolishDone as usize],
        "paste_done_ms": t.marks[Mark::PasteDone as usize],
        "stt_messages": t.counts[Count::SttMessages as usize],
        "polish_tokens_in": t.counts[Count::PolishTokensIn as usize],
        "polish_tokens_out": t.counts[Count::PolishTokensOut as usize],
        "audio_bytes_sent": t.counts[Count::AudioBytesSent as usize],
    })
}

fn write_record(t: &Trace) {
    write_record_to(path(), t);
}

fn write_record_to(path: &std::path::Path, t: &Trace) {
    // A trace that never reached a release (a discarded or failed press) says
    // nothing about release-to-paste; keep the log to real dictations.
    if t.marks[Mark::KeyRelease as usize].is_none() {
        return;
    }
    let line = format!("{}\n", record(t));
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_line_has_all_offsets_and_counts_but_no_text_field() {
        let mut t = Trace {
            epoch: 7,
            provider: "deepgram".into(),
            ..Trace::default()
        };
        for (i, m) in [
            Mark::Connected,
            Mark::KeyRelease,
            Mark::SttTail,
            Mark::FinalTranscript,
            Mark::PolishDone,
            Mark::PasteDone,
        ]
        .into_iter()
        .enumerate()
        {
            t.marks[m as usize] = Some(100 * (i as u64 + 1));
        }
        t.counts = [3, 40, 12, 64000];

        let path = std::env::temp_dir().join(format!("qd-spans-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        write_record_to(&path, &t);
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let v: Value = serde_json::from_str(text.trim_end()).unwrap();
        for (key, want) in [
            ("connected_ms", 100),
            ("key_release_ms", 200),
            ("stt_tail_ms", 300),
            ("final_transcript_ms", 400),
            ("polish_done_ms", 500),
            ("paste_done_ms", 600),
            ("stt_messages", 3),
            ("polish_tokens_in", 40),
            ("polish_tokens_out", 12),
            ("audio_bytes_sent", 64000),
        ] {
            assert_eq!(v[key], want, "{key}");
        }
        // Only numbers, plus the provider id: nothing that could hold dictated text.
        for (key, val) in v.as_object().unwrap() {
            assert!(
                key == "provider" || val.is_number() || val.is_null(),
                "{key}"
            );
        }
    }

    #[test]
    fn a_trace_with_no_key_release_writes_nothing() {
        let path =
            std::env::temp_dir().join(format!("qd-spans-test-skip-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        write_record_to(&path, &Trace::default());
        assert!(!path.exists());
    }
}
