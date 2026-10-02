//! Optional, on-demand local speech-to-text packs.
//!
//! QuickDictate itself ships no model weights and no native inference DLLs.
//! Settings can install one of the pinned model packs below into
//! `%LOCALAPPDATA%\QuickDictate\local-stt`. Downloads use an immutable upstream
//! revision, an exact byte count, and SHA-256; partial files never become active.
//! Every model shares one pinned transcribe.cpp CPU/Vulkan runtime.

use std::fs;
use std::path::{Path, PathBuf};

mod download;
mod install;
mod native;
mod postprocess;
mod worker;

#[cfg(test)]
mod tests;

pub use install::{
    cancel_install, install_snapshot, start_install, start_remove, InstallPhase, InstallSnapshot,
};
pub use worker::{
    request_prewarm, request_unload, set_idle_unload_minutes, transcribe,
    DEFAULT_IDLE_UNLOAD_MINUTES,
};

/// Where the first quiet-boundary clip of `pcm` ends, when `pcm` is long
/// enough to need more than one clip (the same <=35 s cut the decoder makes).
/// `None` while it would still be a single clip. A long dictation uses this to
/// hand finished stretches to the model while the speaker is still talking.
pub fn first_clip_end(pcm: &[i16], sample_rate: usize) -> Option<usize> {
    let ranges = postprocess::cohere_chunk_ranges(pcm, sample_rate);
    (ranges.len() > 1).then(|| ranges[0].end)
}

/// Join the transcripts of consecutive stretches of one dictation, with the
/// same seam handling and decoder-loop guard a single decode gets.
pub fn join_transcripts(parts: Vec<String>) -> Option<String> {
    native::join_and_clean(parts)
}

const RUNTIME_VERSION: &str = "0.2.4";
const RUNTIME_SHA256: &str = "09705f54218817c065602ada8fd0f4d13b3f7fbb9d94929eeb3789c6c2b1f34a";

#[derive(Clone, Copy, Debug)]
pub struct ModelSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
    pub download_bytes: u64,
    filename: &'static str,
    url: &'static str,
    sha256: &'static str,
}

pub const MODELS: [ModelSpec; 3] = [
    ModelSpec {
        id: "cohere-q5",
        label: "Cohere Transcribe — Q5",
        detail: "Best balance · 14 languages · 1.65 GiB",
        download_bytes: 1_770_270_208,
        filename: "cohere-transcribe-03-2026-Q5_K_M.gguf",
        url: "https://huggingface.co/handy-computer/cohere-transcribe-03-2026-gguf/resolve/dfa4adebb64f3076b7b6b90b721275cc069cb421/cohere-transcribe-03-2026-Q5_K_M.gguf",
        sha256: "14d02f1ad6dd77b3a60f82639879012c3adb4fe25c50a5a47a2c4c661daf1558",
    },
    ModelSpec {
        id: "whisper-turbo-q5",
        label: "Whisper Large v3 Turbo — Q5",
        detail: "Most languages · 100 languages · 591 MiB",
        download_bytes: 619_628_128,
        filename: "whisper-large-v3-turbo-Q5_K_M.gguf",
        url: "https://huggingface.co/handy-computer/whisper-large-v3-turbo-gguf/resolve/5eaf945c7978e564bae5b28a5b1639dd93c2bfb1/whisper-large-v3-turbo-Q5_K_M.gguf",
        sha256: "977b5db4e004349dffd1ab9caa10ba5aaba3fc3edd3ba72cadb84328a3203e36",
    },
    ModelSpec {
        id: "parakeet-v3-q5",
        label: "Parakeet TDT v3 — Q5",
        detail: "Fastest · 25 European languages · 524 MiB",
        download_bytes: 548_946_272,
        filename: "parakeet-tdt-0.6b-v3-Q5_K_M.gguf",
        url: "https://huggingface.co/handy-computer/parakeet-tdt-0.6b-v3-gguf/resolve/90f082450fcbacdb54e5900c44ef697c9ea59622/parakeet-tdt-0.6b-v3-Q5_K_M.gguf",
        sha256: "cc722e76adc1a629fc0b2535de879d99b8160d07ad4c0215e2ca7d7ea0ae4b8f",
    },
];

pub fn default_model_id() -> String {
    "cohere-q5".into()
}

pub fn model(id: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|m| m.id == id)
}

fn root_dir() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|p| p.join("QuickDictate").join("local-stt"))
        .ok_or_else(|| "Windows LOCALAPPDATA is unavailable".to_string())
}

fn runtime_dir() -> Result<PathBuf, String> {
    Ok(root_dir()?.join("runtime").join(RUNTIME_VERSION))
}

fn model_dir(spec: &ModelSpec) -> Result<PathBuf, String> {
    Ok(root_dir()?.join("models").join(spec.id))
}

pub fn model_path(id: &str) -> Result<PathBuf, String> {
    let spec = model(id).ok_or_else(|| format!("unknown local model '{id}'"))?;
    Ok(model_dir(spec)?.join(spec.filename))
}

fn marker_path(spec: &ModelSpec) -> Result<PathBuf, String> {
    Ok(model_dir(spec)?.join(".verified"))
}

fn expected_marker(spec: &ModelSpec) -> String {
    format!("sha256={}\nbytes={}\n", spec.sha256, spec.download_bytes)
}

fn expected_runtime_marker() -> String {
    format!("version={RUNTIME_VERSION}\nsha256={RUNTIME_SHA256}\n")
}

/// A runtime directory only counts as trusted when its `.verified` marker
/// content matches the pinned version and hash; merely having a file named
/// transcribe.dll (and an empty or stale marker) proves nothing about what
/// is actually inside it, so this is checked instead of a bare `is_file()`.
fn runtime_verified(dir: &Path) -> bool {
    dir.join("transcribe.dll").is_file()
        && fs::read_to_string(dir.join(".verified")).ok().as_deref()
            == Some(expected_runtime_marker().as_str())
}

/// The model file itself is in place and verified, whatever the runtime's
/// state. A build that pins a newer runtime finds the model this way and
/// fetches only the runtime, never the multi-gigabyte weights again.
pub fn model_verified(id: &str) -> bool {
    let Some(spec) = model(id) else {
        return false;
    };
    let Ok(path) = model_path(id) else {
        return false;
    };
    let Ok(marker) = marker_path(spec) else {
        return false;
    };
    path.metadata().map(|m| m.len()).ok() == Some(spec.download_bytes)
        && fs::read_to_string(marker).ok().as_deref() == Some(expected_marker(spec).as_str())
}

pub fn is_installed(id: &str) -> bool {
    model_verified(id)
        && runtime_dir()
            .ok()
            .map(|p| runtime_verified(&p))
            .unwrap_or(false)
}
