//! Structure-aware mutation fuzzing of the parsers that eat untrusted input,
//! run as ordinary `cargo test` regression tests.
//!
//! Modelled on SageThumbs 2K's `src/fuzz.rs`, retargeted at what QuickDictate
//! actually parses. The difference is worth stating plainly, because it is the
//! reason this exists at all: SageThumbs parses untrusted FILES, while
//! QuickDictate parses untrusted **network responses**. Every string fed to the
//! targets below arrives over the wire from a speech-to-text provider, an LLM
//! cleanup endpoint, the release-check API, or the settings-sync store. A hostile
//! or merely broken server on the other end is the threat model, and "the server
//! is ours" is not a defence: the LLM cleanup endpoint is user-configurable and
//! points wherever they say.
//!
//! What it does: takes small VALID seeds, applies deterministic structure-aware
//! mutations (bit flips, byte fills, numeric blowups, an exhaustive truncation
//! sweep), and runs each through every parser entry point inside `catch_unwind`.
//! A debug build has overflow checks and bounds-check panics ON, so "no panic
//! across N mutations" is a real assertion about the whole slice/arithmetic
//! class -- not just the inputs somebody thought to write down. A panic is
//! reported with the target, the seed name, and the mutated input so it
//! reproduces.
//!
//! Why panics specifically: `windows_subsystem = "windows"` means a panicked
//! worker thread writes to a stderr that does not exist. The user sees dictation
//! stop, with no error. That is also why `clippy::unwrap_used` is on crate-wide
//! (see `clippy.toml`); this is the same concern approached from the input side.
//!
//! Determinism: a fixed-seed xorshift PRNG, no `rand`, no clock. A failure
//! always reproduces and CI never flakes.
//!
//! Deliberately NOT fuzzed here: anything that needs the network, the audio
//! device, or the native local-STT DLL. Those are not parsers and a fuzz loop
//! over them would be a slow integration test wearing a disguise. The archive
//! extractor is covered by a targeted traversal test instead (see
//! `archive_extraction_cannot_escape_its_directory`), because the question
//! there is "can a crafted entry escape", which is an assertion, not a search.

use std::panic::{catch_unwind, AssertUnwindSafe};

/// Mutations per (seed, target) pair. Chosen so the whole module stays inside a
/// couple of seconds of every `cargo test` -- a gate nobody runs is not a gate.
const MUTATIONS_PER_PAIR: usize = 600;

/// Longest input handed to a target. Real payloads here are kilobytes; the
/// point is coverage of shapes, not of size.
const MAX_INPUT: usize = 64 * 1024;

/// xorshift64*, so mutation is reproducible without pulling in `rand` and
/// without reading a clock (a fuzz failure that cannot be reproduced is a bug
/// report nobody can act on).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Any non-zero state works; xorshift is degenerate at zero.
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    fn byte(&mut self) -> u8 {
        (self.next_u64() >> 24) as u8
    }
}

/// One parser under test: a name for the failure message, and a closure that
/// runs it. Every target must be total -- returning `Err`/`None`/a default is
/// fine, panicking is the bug.
struct Target {
    name: &'static str,
    run: fn(&[u8]),
}

/// A named valid input to mutate away from.
struct Seed {
    name: &'static str,
    bytes: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Seeds -- valid inputs, in the shapes the real endpoints send.
// ---------------------------------------------------------------------------

/// The settings template baked into the exe. Real, and the exact thing
/// `Config::load_or_create` parses on a first run.
fn seed_settings() -> Vec<u8> {
    crate::config::EXAMPLE_JSON.as_bytes().to_vec()
}

/// A GitHub "latest release" payload, trimmed to the fields the updater reads.
/// This is the highest-stakes parser in the app: what it returns decides which
/// URL gets downloaded and executed.
fn seed_release_json() -> Vec<u8> {
    br#"{
      "tag_name": "v9.9.9",
      "name": "QuickDictate 9.9.9",
      "prerelease": false,
      "assets": [
        {
          "name": "quickdictate.exe",
          "browser_download_url": "https://github.com/LunarWerxs/QuickDictate/releases/download/v9.9.9/quickdictate.exe",
          "size": 4194304,
          "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
        },
        {
          "name": "quickdictate.exe.sha256",
          "browser_download_url": "https://github.com/LunarWerxs/QuickDictate/releases/download/v9.9.9/quickdictate.exe.sha256",
          "size": 64
        }
      ]
    }"#
    .to_vec()
}

/// An OpenAI-shaped chat completion carrying an edit list -- what the AI
/// cleanup pass parses. The endpoint is user-configurable, so this payload is
/// as untrusted as input gets.
fn seed_polish_reply() -> Vec<u8> {
    br#"{"choices":[{"message":{"content":"{\"edits\":[{\"before\":\"teh\",\"after\":\"the\"},{\"before\":\"recieve\",\"after\":\"receive\"}]}"}}]}"#
        .to_vec()
}

/// A settings-sync document: the portable-preferences subset plus a stats
/// block, as `store_pull` returns it.
fn seed_synced_doc() -> Vec<u8> {
    br#"{
      "settings": {
        "mode": "toggle",
        "language": "en-US",
        "auto_punct": true,
        "reinsert_hold_ms": 1500,
        "text_replacements": {"Chat GPT": "ChatGPT"},
        "custom_vocabulary": ["Supabase"],
        "profiles": []
      },
      "stats": {
        "schema_version": 1,
        "total_words": 1234,
        "total_audio_ms": 567890,
        "total_dictations": 42,
        "providers": {"elevenlabs": {"w": 1234, "a": 567890, "d": 42}},
        "devices": {"abc": {"s": 1, "l": 2, "t": {"w": 1, "a": 2, "d": 3}, "h": {}}}
      }
    }"#
    .to_vec()
}

/// A transcript, as a provider returns it: multi-byte characters, punctuation
/// the processor rewrites, and a trailing command phrase.
fn seed_transcript() -> Vec<u8> {
    "So I said , hello there.i think we're done — really? ok. scratch that"
        .as_bytes()
        .to_vec()
}

/// A release tag, the input to the version comparison that decides whether to
/// download anything at all.
fn seed_version_tag() -> Vec<u8> {
    b"v1.12.3-rc4+build9".to_vec()
}

/// An asset URL. `trusted_asset_url` is the gate that keeps a release payload
/// from redirecting the download somewhere else entirely.
fn seed_asset_url() -> Vec<u8> {
    b"https://github.com/LunarWerxs/QuickDictate/releases/download/v9.9.9/quickdictate.exe".to_vec()
}

fn seeds() -> Vec<Seed> {
    vec![
        Seed {
            name: "settings.json",
            bytes: seed_settings(),
        },
        Seed {
            name: "github-release.json",
            bytes: seed_release_json(),
        },
        Seed {
            name: "polish-reply.json",
            bytes: seed_polish_reply(),
        },
        Seed {
            name: "synced-doc.json",
            bytes: seed_synced_doc(),
        },
        Seed {
            name: "transcript",
            bytes: seed_transcript(),
        },
        Seed {
            name: "version-tag",
            bytes: seed_version_tag(),
        },
        Seed {
            name: "asset-url",
            bytes: seed_asset_url(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

/// Every target takes raw bytes so the same mutated buffer can be fed to all of
/// them: a JSON mutation is also a perfectly good version-string mutation, and
/// cross-feeding is free coverage.
fn targets() -> Vec<Target> {
    vec![
        Target {
            name: "config::from_str",
            run: |b| {
                if let Ok(text) = std::str::from_utf8(b) {
                    let _ = serde_json::from_str::<crate::config::Config>(text);
                }
            },
        },
        Target {
            name: "update::parse_ver",
            run: |b| {
                if let Ok(text) = std::str::from_utf8(b) {
                    let _ = crate::update::parse_ver(text);
                }
            },
        },
        Target {
            name: "update::trusted_asset_url",
            run: |b| {
                if let Ok(text) = std::str::from_utf8(b) {
                    let _ = crate::update::trusted_asset_url(text);
                }
            },
        },
        Target {
            name: "update::exe_asset_from_json",
            run: |b| {
                if let Ok(text) = std::str::from_utf8(b) {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                        let _ = crate::update::exe_asset_from_json(&value);
                    }
                }
            },
        },
        Target {
            name: "polish::parse_reply",
            run: |b| {
                if let Ok(text) = std::str::from_utf8(b) {
                    let _ = crate::polish::parse_reply(text);
                }
            },
        },
        Target {
            name: "sync::apply_synced_to_config",
            run: |b| {
                if let Ok(text) = std::str::from_utf8(b) {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                        let mut cfg = crate::config::Config::default();
                        let _ = crate::sync::apply_synced_to_config(&mut cfg, &value);
                    }
                }
            },
        },
        Target {
            name: "stats::merge_synced_value",
            run: |b| {
                if let Ok(text) = std::str::from_utf8(b) {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
                        let mut stats = crate::stats::UsageStats::default();
                        let _ = stats.merge_synced_value(&value);
                    }
                }
            },
        },
        Target {
            name: "text::process_chunk",
            run: |b| {
                if let Ok(text) = std::str::from_utf8(b) {
                    let processor = crate::text::TextProcessor::new(
                        &std::collections::BTreeMap::new(),
                        true,
                        true,
                        true,
                    );
                    let _ = processor.process_chunk(text, false);
                    let _ = processor.process_chunk(text, true);
                }
            },
        },
        Target {
            name: "text::ends_mid_sentence",
            run: |b| {
                if let Ok(text) = std::str::from_utf8(b) {
                    let _ = crate::text::ends_mid_sentence(text);
                }
            },
        },
        Target {
            name: "voice_commands::detect",
            run: |b| {
                if let Ok(text) = std::str::from_utf8(b) {
                    let _ = crate::voice_commands::detect(text, true);
                }
            },
        },
        Target {
            name: "paths::expand",
            run: |b| {
                if let Ok(text) = std::str::from_utf8(b) {
                    let _ = crate::paths::expand(text);
                }
            },
        },
    ]
}

// ---------------------------------------------------------------------------
// Mutators
// ---------------------------------------------------------------------------

/// One structure-aware mutation. These are chosen for the bug classes that
/// actually bite a text/JSON parser: a byte that breaks UTF-8 mid-character, a
/// length or index field blown past the buffer, a truncation that ends the input
/// inside a token, and a run of the same byte that turns a bounded field
/// unbounded.
fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        return out;
    }
    match rng.below(7) {
        // Flip one bit. The classic, and the one that produces invalid UTF-8
        // continuation bytes in the middle of a multi-byte character.
        0 => {
            let i = rng.below(out.len());
            out[i] ^= 1 << rng.below(8);
        }
        // Replace a byte outright.
        1 => {
            let i = rng.below(out.len());
            out[i] = rng.byte();
        }
        // Fill a region with one byte: turns a short field into a long one.
        2 => {
            let start = rng.below(out.len());
            let len = rng.below(out.len() - start).max(1);
            let fill = rng.byte();
            for byte in &mut out[start..start + len] {
                *byte = fill;
            }
        }
        // Splice in a numeric extreme. A JSON number that overflows u64/i64, or
        // a length that no allocation can satisfy, is exactly what a hostile
        // server sends.
        3 => {
            let extreme: &[u8] = match rng.below(6) {
                0 => b"18446744073709551615",
                1 => b"-9223372036854775808",
                2 => b"999999999999999999999999999999",
                3 => b"1e400",
                4 => b"0",
                _ => b"-1",
            };
            let at = rng.below(out.len());
            out.splice(at..at, extreme.iter().copied());
        }
        // Duplicate a slice: nesting depth, repeated keys, runaway arrays.
        4 => {
            let start = rng.below(out.len());
            let len = rng.below(out.len() - start).clamp(1, 1024);
            let chunk: Vec<u8> = out[start..start + len].to_vec();
            out.splice(start..start, chunk);
        }
        // Delete a slice.
        5 => {
            let start = rng.below(out.len());
            let len = rng.below(out.len() - start).max(1);
            out.drain(start..start + len);
        }
        // Truncate at a random point.
        _ => {
            let at = rng.below(out.len());
            out.truncate(at);
        }
    }
    out.truncate(MAX_INPUT);
    out
}

/// Run one target on one input, turning a panic into a readable failure.
fn probe(target: &Target, seed_name: &str, input: &[u8], note: &str) {
    let run = target.run;
    let result = catch_unwind(AssertUnwindSafe(|| run(input)));
    assert!(
        result.is_ok(),
        "PANIC in {} on a mutation of '{seed_name}' ({note}).\n  len = {}\n  head = {:?}",
        target.name,
        input.len(),
        String::from_utf8_lossy(&input[..input.len().min(200)]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The always-on gate. Every seed x every target x `MUTATIONS_PER_PAIR`
    /// deterministic mutations, plus an EXHAUSTIVE truncation sweep.
    ///
    /// The truncation sweep is not an optimisation to trade away for speed: a
    /// prefix walk is deterministic rather than sampled, and "the response was
    /// cut off mid-token" is the single most likely thing to actually happen to
    /// a network parser -- a dropped connection produces it for free, no
    /// attacker required.
    #[test]
    fn parsers_survive_mutation_of_untrusted_seeds() {
        let seeds = seeds();
        let targets = targets();
        for (seed_index, seed) in seeds.iter().enumerate() {
            for (target_index, target) in targets.iter().enumerate() {
                // The valid seed itself must pass first: a target that panics
                // on well-formed input is not a fuzz finding, it is broken.
                probe(target, seed.name, &seed.bytes, "unmutated seed");

                let mut rng = Rng::new(
                    0x5EED_0000_0000_0001
                        ^ ((seed_index as u64) << 32)
                        ^ ((target_index as u64) << 8),
                );
                for _ in 0..MUTATIONS_PER_PAIR {
                    let input = mutate(&seed.bytes, &mut rng);
                    probe(target, seed.name, &input, "random mutation");
                }
            }
        }
    }

    #[test]
    fn parsers_survive_every_truncation_of_every_seed() {
        let targets = targets();
        for seed in seeds() {
            // Step so a multi-kilobyte seed stays affordable while still
            // landing inside every token: 1 byte for small seeds, and a stride
            // that keeps the walk under ~2000 points for large ones.
            let step = (seed.bytes.len() / 2000).max(1);
            for cut in (0..=seed.bytes.len()).step_by(step) {
                for target in &targets {
                    probe(target, seed.name, &seed.bytes[..cut], "truncation");
                }
            }
        }
    }

    /// THE TEST THAT KEEPS THE REST HONEST.
    ///
    /// Every target begins `if let Ok(text) = std::str::from_utf8(b)`, so a
    /// mutation that breaks UTF-8 returns before touching the parser. A purely
    /// byte-level mutator produces mostly-invalid UTF-8, which would leave this
    /// module running tens of thousands of iterations that all bail on line one
    /// -- green, fast, and testing nothing. (SageThumbs learned the same lesson
    /// the other way round: a seed its own parser rejects is worse than no seed,
    /// because the suite stays green while covering zero code.)
    ///
    /// So: assert that a real share of mutations still reach the parser, and
    /// that the ones which do get past the JSON parse too.
    #[test]
    fn mutations_actually_reach_the_parsers() {
        for seed in seeds() {
            let mut rng = Rng::new(0xC0FF_EE00 ^ seed.bytes.len() as u64);
            let mut utf8_ok = 0usize;
            let mut json_ok = 0usize;
            const N: usize = 2000;
            for _ in 0..N {
                let input = mutate(&seed.bytes, &mut rng);
                if let Ok(text) = std::str::from_utf8(&input) {
                    utf8_ok += 1;
                    if serde_json::from_str::<serde_json::Value>(text).is_ok() {
                        json_ok += 1;
                    }
                }
            }
            // Measured on the current mutator: 1604-1704 of 2000 stay valid
            // UTF-8, and 372-717 of the JSON seeds still parse. The bars are
            // set well under those so a fair tweak to the mutator does not
            // trip them, and well over zero so a mutator that stops producing
            // usable input cannot pass.
            assert!(
                utf8_ok * 100 / N >= 60,
                "only {utf8_ok}/{N} mutations of '{}' stayed valid UTF-8; the parsers are \
                 barely being reached",
                seed.name
            );
            // JSON seeds must also survive as parseable JSON often enough that
            // the semantic layers (asset selection, config merge, stats merge)
            // are genuinely exercised rather than bounced at the syntax gate.
            if seed.name.ends_with(".json") {
                assert!(
                    json_ok >= 150,
                    "only {json_ok}/{N} mutations of '{}' still parsed as JSON; everything \
                     past the syntax check is barely tested",
                    seed.name
                );
            }
        }
    }

    /// A mutation run is only meaningful if the mutator actually changes things
    /// and does so REPRODUCIBLY. Both halves have failed silently in fuzzers
    /// before: a no-op mutator turns the suite into "we parsed the same valid
    /// input 4000 times", and a nondeterministic one turns a real failure into
    /// a bug report nobody can reproduce.
    #[test]
    fn the_mutator_is_effective_and_deterministic() {
        let seed = seed_release_json();

        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        let run_a: Vec<Vec<u8>> = (0..64).map(|_| mutate(&seed, &mut a)).collect();
        let run_b: Vec<Vec<u8>> = (0..64).map(|_| mutate(&seed, &mut b)).collect();
        assert_eq!(
            run_a, run_b,
            "the same seed must produce the same mutations"
        );

        let changed = run_a.iter().filter(|m| **m != seed).count();
        assert!(
            changed >= 60,
            "the mutator barely changed anything ({changed}/64); the fuzz run would be vacuous"
        );
        let distinct: std::collections::BTreeSet<&Vec<u8>> = run_a.iter().collect();
        assert!(
            distinct.len() >= 50,
            "only {} distinct mutations out of 64",
            distinct.len()
        );
    }

    /// Every target must be listed once, and every one must survive the empty
    /// input -- the degenerate case a dropped connection produces.
    #[test]
    fn every_target_is_named_once_and_handles_empty_input() {
        let targets = targets();
        let mut names: Vec<&str> = targets.iter().map(|t| t.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "a target is listed twice");

        for target in &targets {
            probe(target, "<empty>", b"", "empty input");
        }
    }
}
