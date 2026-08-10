//! Optional, on-demand local speech-to-text packs.
//!
//! QuickDictate itself ships no model weights and no native inference DLLs.
//! Settings can install one of the pinned model packs below into
//! `%LOCALAPPDATA%\QuickDictate\local-stt`. Downloads use an immutable upstream
//! revision, an exact byte count, and SHA-256; partial files never become active.
//! Both models share one pinned transcribe.cpp CPU/Vulkan runtime.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use libloading::os::windows::{
    Library, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

const RUNTIME_VERSION: &str = "0.1.3";
const RUNTIME_URL: &str = "https://github.com/handy-computer/transcribe.cpp/releases/download/v0.1.3/transcribe-native-0.1.3-windows-x86_64-cpu-vulkan.tar.gz";
const RUNTIME_SHA256: &str = "9f536cb0fb839bd305e6d92fb214fd417c7718a416a6c7646a9911fbd56fdad5";
const RUNTIME_BYTES: u64 = 25_957_910;
const RUNTIME_ARCHIVE_ROOT: &str = "transcribe-native-windows-x86_64-cpu-vulkan";
const USER_AGENT: &str = concat!("QuickDictate/", env!("CARGO_PKG_VERSION"));
const PARALLEL_DOWNLOAD_MIN_BYTES: u64 = 32 * 1024 * 1024;
const PARALLEL_DOWNLOAD_WORKERS: usize = 8;
const DOWNLOAD_BUFFER_BYTES: usize = 1024 * 1024;
const DOWNLOAD_RANGE_ATTEMPTS: usize = 3;

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

pub const MODELS: [ModelSpec; 2] = [
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
        detail: "Smallest · 100 languages · 591 MiB",
        download_bytes: 619_628_128,
        filename: "whisper-large-v3-turbo-Q5_K_M.gguf",
        url: "https://huggingface.co/handy-computer/whisper-large-v3-turbo-gguf/resolve/5eaf945c7978e564bae5b28a5b1639dd93c2bfb1/whisper-large-v3-turbo-Q5_K_M.gguf",
        sha256: "977b5db4e004349dffd1ab9caa10ba5aaba3fc3edd3ba72cadb84328a3203e36",
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

pub fn is_installed(id: &str) -> bool {
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
        && runtime_dir()
            .ok()
            .map(|p| runtime_verified(&p))
            .unwrap_or(false)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallPhase {
    NotInstalled,
    DownloadingRuntime,
    InstallingRuntime,
    DownloadingModel,
    VerifyingDownload,
    Cancelling,
    Installed,
    Removing,
    Failed(String),
}

impl InstallPhase {
    fn busy(&self) -> bool {
        matches!(
            self,
            Self::DownloadingRuntime
                | Self::InstallingRuntime
                | Self::DownloadingModel
                | Self::VerifyingDownload
                | Self::Cancelling
                | Self::Removing
        )
    }
}

#[derive(Clone, Debug)]
pub struct InstallSnapshot {
    pub phase: InstallPhase,
    pub downloaded: u64,
    pub total: u64,
}

impl InstallSnapshot {
    pub fn busy(&self) -> bool {
        self.phase.busy()
    }
}

#[derive(Default)]
struct InstallerControl {
    states: HashMap<String, InstallSnapshot>,
    cancels: HashMap<String, Arc<AtomicBool>>,
}

fn installer_control() -> &'static Mutex<InstallerControl> {
    static CONTROL: OnceLock<Mutex<InstallerControl>> = OnceLock::new();
    CONTROL.get_or_init(|| Mutex::new(InstallerControl::default()))
}

pub fn install_snapshot(id: &str) -> InstallSnapshot {
    if let Some(state) = installer_control()
        .lock()
        .ok()
        .and_then(|s| s.states.get(id).cloned())
    {
        if state.busy() || matches!(state.phase, InstallPhase::Failed(_)) {
            return state;
        }
    }
    InstallSnapshot {
        phase: if is_installed(id) {
            InstallPhase::Installed
        } else {
            InstallPhase::NotInstalled
        },
        downloaded: 0,
        total: model(id).map(|m| m.download_bytes).unwrap_or(0),
    }
}

fn set_state(id: &str, phase: InstallPhase, downloaded: u64, total: u64) {
    if let Ok(mut control) = installer_control().lock() {
        let downloaded = if let Some(current) = control.states.get(id) {
            if matches!(current.phase, InstallPhase::Cancelling) && phase.busy() {
                return;
            }
            if current.phase == phase {
                downloaded.max(current.downloaded)
            } else {
                downloaded
            }
        } else {
            downloaded
        };
        control.states.insert(
            id.to_string(),
            InstallSnapshot {
                phase,
                downloaded,
                total,
            },
        );
    }
}

fn finish_operation(id: &str, phase: InstallPhase, downloaded: u64, total: u64) {
    if let Ok(mut control) = installer_control().lock() {
        control.cancels.remove(id);
        control.states.insert(
            id.to_string(),
            InstallSnapshot {
                phase,
                downloaded,
                total,
            },
        );
    }
}

fn claim_operation(id: &str, phase: InstallPhase, total: u64) -> Result<Arc<AtomicBool>, String> {
    let mut control = installer_control()
        .lock()
        .map_err(|_| "local model installer state is unavailable".to_string())?;
    if control.states.values().any(InstallSnapshot::busy) {
        return Err("another local model install/remove operation is already running".into());
    }
    let cancel = Arc::new(AtomicBool::new(false));
    control.cancels.insert(id.to_string(), Arc::clone(&cancel));
    control.states.insert(
        id.to_string(),
        InstallSnapshot {
            phase,
            downloaded: 0,
            total,
        },
    );
    Ok(cancel)
}

pub fn cancel_install(id: &str) -> Result<(), String> {
    let mut control = installer_control()
        .lock()
        .map_err(|_| "local model installer state is unavailable".to_string())?;
    let (downloaded, total) = match control.states.get(id) {
        Some(snapshot)
            if matches!(
                snapshot.phase,
                InstallPhase::DownloadingRuntime
                    | InstallPhase::InstallingRuntime
                    | InstallPhase::DownloadingModel
                    | InstallPhase::VerifyingDownload
                    | InstallPhase::Cancelling
            ) =>
        {
            (snapshot.downloaded, snapshot.total)
        }
        _ => return Err("that model is not currently being installed".into()),
    };
    let cancel = control
        .cancels
        .get(id)
        .cloned()
        .ok_or_else(|| "model installer cancellation is unavailable".to_string())?;
    cancel.store(true, Ordering::Release);
    control.states.insert(
        id.to_string(),
        InstallSnapshot {
            phase: InstallPhase::Cancelling,
            downloaded,
            total,
        },
    );
    Ok(())
}

pub fn start_install(id: &str) -> Result<(), String> {
    let spec = *model(id).ok_or_else(|| format!("unknown local model '{id}'"))?;
    if is_installed(id) {
        return Ok(());
    }
    let cancel = claim_operation(id, InstallPhase::DownloadingRuntime, spec.download_bytes)?;
    let spawn = std::thread::Builder::new()
        .name(format!("qd-model-install-{}", spec.id))
        .spawn(move || {
            let result = install(&spec, &cancel);
            if cancel.load(Ordering::Acquire) {
                tracing::info!("local model '{}' install cancelled", spec.id);
                finish_operation(spec.id, InstallPhase::NotInstalled, 0, spec.download_bytes);
            } else {
                match result {
                    Ok(()) => finish_operation(
                        spec.id,
                        InstallPhase::Installed,
                        spec.download_bytes,
                        spec.download_bytes,
                    ),
                    Err(e) => {
                        tracing::error!("local model '{}' install failed: {e}", spec.id);
                        finish_operation(spec.id, InstallPhase::Failed(e), 0, spec.download_bytes);
                    }
                }
            }
        });
    match spawn {
        Ok(_) => Ok(()),
        Err(e) => {
            let message = format!("could not start model installer: {e}");
            finish_operation(
                spec.id,
                InstallPhase::Failed(message.clone()),
                0,
                spec.download_bytes,
            );
            Err(message)
        }
    }
}

pub fn start_remove(id: &str) -> Result<(), String> {
    let spec = *model(id).ok_or_else(|| format!("unknown local model '{id}'"))?;
    let _cancel = claim_operation(spec.id, InstallPhase::Removing, spec.download_bytes)?;
    let spawn = std::thread::Builder::new()
        .name(format!("qd-model-remove-{}", spec.id))
        .spawn(move || {
            let result = model_dir(&spec).and_then(|dir| {
                if dir.exists() {
                    fs::remove_dir_all(&dir)
                        .map_err(|e| format!("could not remove {}: {e}", dir.display()))?;
                }
                Ok(())
            });
            match result {
                Ok(()) => {
                    finish_operation(spec.id, InstallPhase::NotInstalled, 0, spec.download_bytes)
                }
                Err(e) => {
                    finish_operation(spec.id, InstallPhase::Failed(e), 0, spec.download_bytes)
                }
            }
        });
    match spawn {
        Ok(_) => Ok(()),
        Err(e) => {
            let message = format!("could not start model removal: {e}");
            finish_operation(
                spec.id,
                InstallPhase::Failed(message.clone()),
                0,
                spec.download_bytes,
            );
            Err(message)
        }
    }
}

fn install(spec: &ModelSpec, cancel: &AtomicBool) -> Result<(), String> {
    ensure_runtime(spec, cancel)?;
    check_cancelled(cancel)?;
    set_state(
        spec.id,
        InstallPhase::DownloadingModel,
        0,
        spec.download_bytes,
    );
    let dir = model_dir(spec)?;
    fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let dest = dir.join(spec.filename);
    download_verified(
        spec.id,
        InstallPhase::DownloadingModel,
        spec.url,
        spec.download_bytes,
        spec.sha256,
        &dest,
        spec.download_bytes,
        cancel,
    )?;
    if let Err(e) = check_cancelled(cancel) {
        let _ = fs::remove_file(&dest);
        return Err(e);
    }
    let marker = marker_path(spec)?;
    write_atomic(&marker, expected_marker(spec).as_bytes())?;
    Ok(())
}

fn ensure_runtime(spec: &ModelSpec, cancel: &AtomicBool) -> Result<(), String> {
    let final_dir = runtime_dir()?;
    if runtime_verified(&final_dir) {
        return Ok(());
    }
    let root = root_dir()?;
    let runtime_parent = root.join("runtime");
    fs::create_dir_all(&runtime_parent)
        .map_err(|e| format!("could not create {}: {e}", runtime_parent.display()))?;
    let archive = runtime_parent.join(format!("runtime-{RUNTIME_VERSION}.tar.gz"));
    set_state(spec.id, InstallPhase::DownloadingRuntime, 0, RUNTIME_BYTES);
    download_verified(
        spec.id,
        InstallPhase::DownloadingRuntime,
        RUNTIME_URL,
        RUNTIME_BYTES,
        RUNTIME_SHA256,
        &archive,
        RUNTIME_BYTES,
        cancel,
    )?;
    check_cancelled(cancel)?;
    set_state(
        spec.id,
        InstallPhase::InstallingRuntime,
        RUNTIME_BYTES,
        RUNTIME_BYTES,
    );

    let staging = runtime_parent.join(format!(".installing-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .map_err(|e| format!("could not clear {}: {e}", staging.display()))?;
    }
    fs::create_dir_all(&staging)
        .map_err(|e| format!("could not create {}: {e}", staging.display()))?;
    let unpack_result = (|| {
        let file =
            File::open(&archive).map_err(|e| format!("could not open downloaded runtime: {e}"))?;
        let mut tar = tar::Archive::new(GzDecoder::new(file));
        // `unpack` routes every entry through tar's traversal-safe `unpack_in`.
        tar.unpack(&staging)
            .map_err(|e| format!("could not extract local runtime: {e}"))?;
        check_cancelled(cancel)?;
        let extracted = staging.join(RUNTIME_ARCHIVE_ROOT);
        if !extracted.join("transcribe.dll").is_file() || !extracted.join("contract.json").is_file()
        {
            return Err("downloaded runtime did not contain its required files".into());
        }
        write_atomic(
            &extracted.join(".verified"),
            expected_runtime_marker().as_bytes(),
        )?;
        if final_dir.exists() {
            fs::remove_dir_all(&final_dir)
                .map_err(|e| format!("could not replace {}: {e}", final_dir.display()))?;
        }
        fs::rename(&extracted, &final_dir)
            .map_err(|e| format!("could not activate local runtime: {e}"))?;
        check_cancelled(cancel)?;
        Ok(())
    })();
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&staging);
    unpack_result
}

#[allow(clippy::too_many_arguments)]
fn download_verified(
    id: &str,
    phase: InstallPhase,
    url: &str,
    expected_bytes: u64,
    expected_sha256: &str,
    dest: &Path,
    display_total: u64,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let parent = dest
        .parent()
        .ok_or_else(|| "download destination has no parent".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    let part = dest.with_extension("part");
    let _ = fs::remove_file(&part);
    let result = (|| {
        check_cancelled(cancel)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("could not start download runtime: {e}"))?;
        let client = download_client()?;
        let parallel = expected_bytes >= PARALLEL_DOWNLOAD_MIN_BYTES
            && runtime.block_on(server_supports_ranges(&client, url, expected_bytes, cancel))?;
        let actual = if parallel {
            tracing::info!(
                "downloading {expected_bytes} bytes with {PARALLEL_DOWNLOAD_WORKERS} parallel ranges"
            );
            runtime.block_on(download_parallel(
                &client,
                id,
                phase,
                url,
                expected_bytes,
                &part,
                display_total,
                cancel,
                PARALLEL_DOWNLOAD_WORKERS,
            ))?;
            set_state(
                id,
                InstallPhase::VerifyingDownload,
                expected_bytes,
                display_total,
            );
            hash_file(&part, cancel)?
        } else {
            tracing::info!("downloading {expected_bytes} bytes as one HTTP stream");
            runtime.block_on(download_single(
                &client,
                id,
                phase,
                url,
                expected_bytes,
                &part,
                display_total,
                cancel,
            ))?
        };
        check_cancelled(cancel)?;
        if actual != expected_sha256 {
            return Err("download failed SHA-256 verification".into());
        }
        if dest.exists() {
            fs::remove_file(dest)
                .map_err(|e| format!("could not replace {}: {e}", dest.display()))?;
        }
        fs::rename(&part, dest)
            .map_err(|e| format!("could not activate {}: {e}", dest.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

fn check_cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err("download cancelled".into())
    } else {
        Ok(())
    }
}

fn download_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(4 * 60 * 60))
        .build()
        .map_err(|e| format!("could not create download client: {e}"))
}

async fn send_with_cancel(
    request: reqwest::RequestBuilder,
    cancel: &AtomicBool,
) -> Result<reqwest::Response, String> {
    let request = request.send();
    tokio::pin!(request);
    loop {
        tokio::select! {
            result = &mut request => {
                return result.map_err(|e| format!("download request failed: {e}"));
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                check_cancelled(cancel)?;
            }
        }
    }
}

async fn next_chunk_with_cancel(
    response: &mut reqwest::Response,
    cancel: &AtomicBool,
) -> Result<Option<bytes::Bytes>, String> {
    let chunk = response.chunk();
    tokio::pin!(chunk);
    loop {
        tokio::select! {
            result = &mut chunk => {
                return result.map_err(|e| format!("download read failed: {e}"));
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                check_cancelled(cancel)?;
            }
        }
    }
}

async fn server_supports_ranges(
    client: &reqwest::Client,
    url: &str,
    expected_bytes: u64,
    cancel: &AtomicBool,
) -> Result<bool, String> {
    check_cancelled(cancel)?;
    let mut response = send_with_cancel(
        client.get(url).header(reqwest::header::RANGE, "bytes=0-0"),
        cancel,
    )
    .await
    .map_err(|e| format!("download range probe failed: {e}"))?;
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Ok(false);
    }
    let expected_range = format!("bytes 0-0/{expected_bytes}");
    let actual_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok());
    if actual_range != Some(expected_range.as_str()) || response.content_length() != Some(1) {
        return Ok(false);
    }
    let chunk = next_chunk_with_cancel(&mut response, cancel)
        .await
        .map_err(|e| format!("download range probe failed: {e}"))?;
    if chunk.as_deref().map(<[u8]>::len) != Some(1) {
        return Ok(false);
    }
    check_cancelled(cancel)?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn download_single(
    client: &reqwest::Client,
    id: &str,
    phase: InstallPhase,
    url: &str,
    expected_bytes: u64,
    part: &Path,
    display_total: u64,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let mut response = send_with_cancel(client.get(url), cancel)
        .await
        .map_err(|e| format!("download failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("download failed: HTTP {}", response.status()));
    }
    if let Some(len) = response.content_length() {
        if len != expected_bytes {
            return Err(format!(
                "download size changed upstream (expected {expected_bytes}, got {len})"
            ));
        }
    }
    let mut file =
        File::create(part).map_err(|e| format!("could not create {}: {e}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut downloaded = 0u64;
    loop {
        check_cancelled(cancel)?;
        let Some(chunk) = next_chunk_with_cancel(&mut response, cancel).await? else {
            break;
        };
        let n = chunk.len();
        downloaded = downloaded.saturating_add(n as u64);
        if downloaded > expected_bytes {
            return Err("download exceeded its pinned size".into());
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .map_err(|e| format!("download write failed: {e}"))?;
        set_state(id, phase.clone(), downloaded, display_total);
    }
    file.sync_all()
        .map_err(|e| format!("could not flush download: {e}"))?;
    if downloaded != expected_bytes {
        return Err(format!(
            "download was incomplete (expected {expected_bytes} bytes, got {downloaded})"
        ));
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn range_segments(total: u64, workers: usize) -> Vec<(u64, u64)> {
    if total == 0 || workers == 0 {
        return Vec::new();
    }
    let workers = workers.min(usize::try_from(total).unwrap_or(usize::MAX));
    let chunk = total.div_ceil(workers as u64);
    (0..workers)
        .filter_map(|index| {
            let start = index as u64 * chunk;
            (start < total).then(|| (start, (start + chunk).min(total) - 1))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn download_parallel(
    client: &reqwest::Client,
    id: &str,
    phase: InstallPhase,
    url: &str,
    expected_bytes: u64,
    part: &Path,
    display_total: u64,
    cancel: &AtomicBool,
    workers: usize,
) -> Result<(), String> {
    let file =
        File::create(part).map_err(|e| format!("could not create {}: {e}", part.display()))?;
    file.set_len(expected_bytes)
        .map_err(|e| format!("could not size {}: {e}", part.display()))?;
    drop(file);

    let progress = AtomicU64::new(0);
    let failed = AtomicBool::new(false);
    let first_error = Mutex::new(None::<String>);
    let downloads = range_segments(expected_bytes, workers)
        .into_iter()
        .map(|(start, end)| {
            let client = client.clone();
            let phase = phase.clone();
            let progress = &progress;
            let failed = &failed;
            let first_error = &first_error;
            async move {
                let result = download_range(
                    &client,
                    id,
                    phase,
                    url,
                    expected_bytes,
                    start,
                    end,
                    part,
                    display_total,
                    progress,
                    cancel,
                    failed,
                )
                .await;
                if let Err(error) = result {
                    if failed
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        if let Ok(mut first) = first_error.lock() {
                            *first = Some(error);
                        }
                    }
                }
            }
        });
    futures_util::future::join_all(downloads).await;
    check_cancelled(cancel)?;
    if let Some(error) = first_error.lock().ok().and_then(|mut e| e.take()) {
        return Err(error);
    }
    let downloaded = progress.load(Ordering::Acquire);
    if downloaded != expected_bytes {
        return Err(format!(
            "parallel download was incomplete (expected {expected_bytes} bytes, got {downloaded})"
        ));
    }
    let file = OpenOptions::new()
        .write(true)
        .open(part)
        .map_err(|e| format!("could not open {} for flushing: {e}", part.display()))?;
    file.sync_all()
        .map_err(|e| format!("could not flush download: {e}"))
}

#[allow(clippy::too_many_arguments)]
async fn download_range(
    client: &reqwest::Client,
    id: &str,
    phase: InstallPhase,
    url: &str,
    expected_bytes: u64,
    start: u64,
    end: u64,
    part: &Path,
    display_total: u64,
    progress: &AtomicU64,
    cancel: &AtomicBool,
    failed: &AtomicBool,
) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .open(part)
        .map_err(|e| format!("could not open {}: {e}", part.display()))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|e| format!("could not seek {}: {e}", part.display()))?;
    let mut next = start;
    let mut last_error = None;
    for attempt in 1..=DOWNLOAD_RANGE_ATTEMPTS {
        check_cancelled(cancel)?;
        if failed.load(Ordering::Acquire) {
            return Err("parallel download stopped after another range failed".into());
        }
        let mut response = match send_with_cancel(
            client
                .get(url)
                .header(reqwest::header::RANGE, format!("bytes={next}-{end}")),
            cancel,
        )
        .await
        {
            Ok(response) => response,
            Err(e) => {
                last_error = Some(format!("request failed: {e}"));
                if attempt < DOWNLOAD_RANGE_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(250 * attempt as u64)).await;
                }
                continue;
            }
        };
        let remaining = end - next + 1;
        let expected_range = format!("bytes {next}-{end}/{expected_bytes}");
        let actual_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok());
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
            || actual_range != Some(expected_range.as_str())
            || response.content_length() != Some(remaining)
        {
            last_error = Some(format!(
                "server returned unexpected metadata ({})",
                response.status()
            ));
            if attempt < DOWNLOAD_RANGE_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(250 * attempt as u64)).await;
            }
            continue;
        }

        last_error = Some(format!("response ended before byte {end}"));
        while next <= end {
            check_cancelled(cancel)?;
            if failed.load(Ordering::Acquire) {
                return Err("parallel download stopped after another range failed".into());
            }
            let limit = (end - next + 1).min(DOWNLOAD_BUFFER_BYTES as u64) as usize;
            let chunk = match next_chunk_with_cancel(&mut response, cancel).await {
                Ok(None) => break,
                Ok(Some(chunk)) => chunk,
                Err(e) => {
                    last_error = Some(format!("read failed at byte {next}: {e}"));
                    break;
                }
            };
            if chunk.len() > limit {
                return Err(format!(
                    "range {start}-{end} returned more data than requested"
                ));
            }
            let n = chunk.len();
            file.write_all(&chunk)
                .map_err(|e| format!("range {start}-{end} write failed: {e}"))?;
            next += n as u64;
            let downloaded = progress.fetch_add(n as u64, Ordering::AcqRel) + n as u64;
            set_state(id, phase.clone(), downloaded, display_total);
        }
        if next > end {
            return Ok(());
        }
        if attempt < DOWNLOAD_RANGE_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(250 * attempt as u64)).await;
        }
    }
    Err(format!(
        "range {start}-{end} failed after {DOWNLOAD_RANGE_ATTEMPTS} attempts: {}",
        last_error.unwrap_or_else(|| "range did not start".into())
    ))
}

fn hash_file(path: &Path, cancel: &AtomicBool) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|e| format!("could not verify {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; DOWNLOAD_BUFFER_BYTES];
    loop {
        check_cancelled(cancel)?;
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("could not verify {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// `is_installed` trusts a matching length plus a marker derived only from
/// public compile-time constants, because it is polled from the UI and must
/// stay cheap; that means a same-length swap performed after install would
/// otherwise be trusted forever. This re-hashes the installed model file
/// against its pinned SHA-256 the first time a process actually loads it,
/// caching the outcome so a second dictation in the same run does not
/// re-hash multiple gigabytes. A failed check is deliberately not cached, so
/// a reinstall in the same process is re-verified rather than staying stuck.
fn verify_model_hash_once(spec: &ModelSpec, path: &Path) -> Result<(), String> {
    static VERIFIED: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = VERIFIED.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let verified = cache
            .lock()
            .map_err(|_| "local model verification state is unavailable".to_string())?;
        if verified.get(spec.id) == Some(&true) {
            return Ok(());
        }
    }
    let actual = hash_file(path, &AtomicBool::new(false))?;
    let ok = actual == spec.sha256;
    let mut verified = cache
        .lock()
        .map_err(|_| "local model verification state is unavailable".to_string())?;
    verified.insert(spec.id.to_string(), ok);
    if ok {
        Ok(())
    } else {
        Err(format!(
            "local model '{}' failed integrity verification; its file no longer matches the installed checksum. Reinstall it in Settings",
            spec.id
        ))
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    if path.exists() {
        fs::remove_file(path).map_err(|e| format!("could not replace {}: {e}", path.display()))?;
    }
    fs::rename(&tmp, path).map_err(|e| format!("could not save {}: {e}", path.display()))
}

// Cohere's own long-form processor never sends the model more than 35 seconds
// at once. It searches the final five seconds for a quiet boundary, then starts
// a fresh decode. The native runtime accepts a much larger positional window,
// but a multi-minute greedy decode can fall into a sentence loop long before
// that hard limit (the supplied field log reproduced this at 240.9 seconds).
const COHERE_CLIP_MAX_SECONDS: usize = 35;
const COHERE_BOUNDARY_SEARCH_SECONDS: usize = 5;
const COHERE_MIN_TAIL_SECONDS: usize = 5;
const COHERE_ENERGY_WINDOW_MS: usize = 100;
const COHERE_ENERGY_STEP_MS: usize = 10;
const PATHOLOGICAL_SENTENCE_RUN: usize = 4;
const PATHOLOGICAL_SENTENCES_TO_KEEP: usize = 2;
const PATHOLOGICAL_CYCLE_RUN: usize = 4;
const PATHOLOGICAL_CYCLES_TO_KEEP: usize = 2;
const PATHOLOGICAL_MIN_REPEATED_TOKENS: usize = 8;
const PATHOLOGICAL_MAX_CYCLE_TOKENS: usize = 24;

fn cohere_chunk_ranges(pcm: &[i16], sample_rate: usize) -> Vec<Range<usize>> {
    if pcm.is_empty() || sample_rate == 0 {
        return Vec::new();
    }
    let max_clip = sample_rate.saturating_mul(COHERE_CLIP_MAX_SECONDS);
    if pcm.len() <= max_clip {
        return std::iter::once(0..pcm.len()).collect();
    }

    let search_span = sample_rate.saturating_mul(COHERE_BOUNDARY_SEARCH_SECONDS);
    let min_tail = sample_rate.saturating_mul(COHERE_MIN_TAIL_SECONDS);
    let mut ranges = Vec::new();
    let mut start = 0usize;
    while pcm.len().saturating_sub(start) > max_clip {
        let search_start = start + max_clip.saturating_sub(search_span);
        // Avoid manufacturing a tiny final fragment for recordings only a
        // little longer than 35 seconds.
        let search_end = (start + max_clip).min(pcm.len().saturating_sub(min_tail));
        let cut = quietest_cut(pcm, search_start, search_end, sample_rate)
            .unwrap_or(search_end.max(search_start));
        // Defensive progress guard; normal inputs always advance by >=30 s.
        let cut = cut.clamp(start + 1, pcm.len());
        ranges.push(start..cut);
        start = cut;
    }
    if start < pcm.len() {
        ranges.push(start..pcm.len());
    }
    ranges
}

/// Find the lowest-energy 100 ms window in `[start, end]` and return its
/// midpoint. A 10 ms step is fine-grained enough to land between spoken words
/// without doing meaningful work compared with inference.
fn quietest_cut(pcm: &[i16], start: usize, end: usize, sample_rate: usize) -> Option<usize> {
    if start >= end || end > pcm.len() || sample_rate == 0 {
        return None;
    }
    let window = (sample_rate.saturating_mul(COHERE_ENERGY_WINDOW_MS) / 1_000).max(1);
    let step = (sample_rate.saturating_mul(COHERE_ENERGY_STEP_MS) / 1_000).max(1);
    let half = window / 2;
    let first = start.saturating_add(half).min(end);
    let last = end.saturating_sub(window.saturating_sub(half));
    if first > last {
        return Some(start + (end - start) / 2);
    }

    let mut best: Option<(u64, usize)> = None;
    let mut cut = first;
    while cut <= last {
        let window_start = cut.saturating_sub(half);
        let window_end = (window_start + window).min(pcm.len());
        let energy = pcm[window_start..window_end]
            .iter()
            .map(|sample| i64::from(*sample).unsigned_abs())
            .sum::<u64>();
        if best
            .map(|(best_energy, _)| energy < best_energy)
            .unwrap_or(true)
        {
            best = Some((energy, cut));
        }
        let next = cut.saturating_add(step);
        if next <= cut {
            break;
        }
        cut = next;
    }
    best.map(|(_, cut)| cut)
}

fn sentence_units(text: &str) -> Vec<&str> {
    let mut units = Vec::new();
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        if !matches!(ch, '.' | '!' | '?') {
            continue;
        }
        let end = index + ch.len_utf8();
        let next = text[end..].chars().next();
        if next.is_none_or(char::is_whitespace) {
            let unit = text[start..end].trim();
            if !unit.is_empty() {
                units.push(unit);
            }
            start = end;
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        units.push(tail);
    }
    units
}

fn normalized_sentence(text: &str) -> String {
    text.chars()
        .filter_map(|ch| {
            if ch.is_alphanumeric() || ch == '\'' {
                Some(ch.to_ascii_lowercase())
            } else if ch.is_whitespace() {
                Some(' ')
            } else {
                None
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Conservative last line of defense for decoder degeneration. Only runs of
/// four or more identical full sentences are touched, and two copies remain so
/// deliberate emphasis is preserved.
fn collapse_pathological_sentence_runs(text: &str) -> (String, usize) {
    let units = sentence_units(text);
    let mut output = Vec::with_capacity(units.len());
    let mut dropped = 0usize;
    let mut index = 0usize;
    while index < units.len() {
        let normalized = normalized_sentence(units[index]);
        let mut end = index + 1;
        while end < units.len()
            && !normalized.is_empty()
            && normalized_sentence(units[end]) == normalized
        {
            end += 1;
        }
        let run = end - index;
        let keep = if run >= PATHOLOGICAL_SENTENCE_RUN {
            dropped = dropped.saturating_add(run - PATHOLOGICAL_SENTENCES_TO_KEEP);
            PATHOLOGICAL_SENTENCES_TO_KEEP
        } else {
            run
        };
        output.extend_from_slice(&units[index..index + keep]);
        index = end;
    }
    (output.join(" "), dropped)
}

#[derive(Debug)]
struct WordSpan {
    normalized: String,
    end: usize,
}

fn word_spans(text: &str) -> Vec<WordSpan> {
    let mut spans = Vec::new();
    let mut current: Option<String> = None;
    for (index, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            let normalized = current.get_or_insert_with(String::new);
            normalized.extend(ch.to_lowercase());
        } else if let Some(normalized) = current.take() {
            spans.push(WordSpan {
                normalized,
                end: index,
            });
        }
    }
    if let Some(normalized) = current {
        spans.push(WordSpan {
            normalized,
            end: text.len(),
        });
    }
    spans
}

/// Catch punctuation-free or alternating decoder cycles that the full-sentence
/// guard cannot see (for example, "and here, and here, and here..."). Four
/// cycles and at least eight repeated tokens are required; two cycles remain.
fn collapse_pathological_token_cycles(text: &str) -> (String, usize) {
    let tokens = word_spans(text);
    if tokens.len() < PATHOLOGICAL_MIN_REPEATED_TOKENS {
        return (text.to_string(), 0);
    }

    let mut removals = Vec::new();
    let mut index = 0usize;
    let mut dropped_tokens = 0usize;
    while index < tokens.len() {
        let max_cycle =
            PATHOLOGICAL_MAX_CYCLE_TOKENS.min((tokens.len() - index) / PATHOLOGICAL_CYCLE_RUN);
        let mut found = None;
        for cycle_len in 1..=max_cycle {
            let motif = &tokens[index..index + cycle_len];
            let mut cycles = 1usize;
            while index + (cycles + 1) * cycle_len <= tokens.len()
                && tokens[index + cycles * cycle_len..index + (cycles + 1) * cycle_len]
                    .iter()
                    .map(|token| token.normalized.as_str())
                    .eq(motif.iter().map(|token| token.normalized.as_str()))
            {
                cycles += 1;
            }
            if cycles >= PATHOLOGICAL_CYCLE_RUN
                && cycles * cycle_len >= PATHOLOGICAL_MIN_REPEATED_TOKENS
            {
                found = Some((cycle_len, cycles));
                break;
            }
        }

        if let Some((cycle_len, cycles)) = found {
            let keep_end = index + PATHOLOGICAL_CYCLES_TO_KEEP * cycle_len - 1;
            let run_end = index + cycles * cycle_len - 1;
            removals.push((tokens[keep_end].end, tokens[run_end].end));
            dropped_tokens =
                dropped_tokens.saturating_add((cycles - PATHOLOGICAL_CYCLES_TO_KEEP) * cycle_len);
            index += cycles * cycle_len;
        } else {
            index += 1;
        }
    }

    if removals.is_empty() {
        return (text.to_string(), 0);
    }
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (start, end) in removals {
        output.push_str(&text[cursor..start]);
        cursor = end;
    }
    output.push_str(&text[cursor..]);
    (output, dropped_tokens)
}

fn collapse_pathological_repetitions(text: &str) -> (String, usize) {
    let (sentences_cleaned, sentence_drops) = collapse_pathological_sentence_runs(text);
    let (tokens_cleaned, token_drops) = collapse_pathological_token_cycles(&sentences_cleaned);
    (tokens_cleaned, sentence_drops.saturating_add(token_drops))
}

fn join_transcript_parts(parts: impl IntoIterator<Item = String>) -> Option<String> {
    let joined = parts
        .into_iter()
        .filter_map(|part| {
            let part = part.trim().to_string();
            (!part.is_empty()).then_some(part)
        })
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}

// ---- Native transcribe.cpp worker -----------------------------------------

struct Job {
    model_id: String,
    language: String,
    pcm: Vec<i16>,
    cancel: Arc<AtomicBool>,
    result: oneshot::Sender<Result<Option<String>, String>>,
}

enum WorkerCommand {
    Transcribe(Job),
    Prewarm(String),
    Unload,
}

static WORKER: OnceLock<Result<mpsc::SyncSender<WorkerCommand>, String>> = OnceLock::new();
static UNLOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

/// How long the worker keeps an idle model resident before releasing it.
/// `request_unload` already handles the explicit case (Settings switches
/// away from Local); this covers a user who leaves Local selected and simply
/// stops dictating, which would otherwise pin multi-gigabyte weights in
/// memory for the rest of the tray app's uptime.
const IDLE_UNLOAD_AFTER: Duration = Duration::from_secs(10 * 60);

/// Pure decision behind the idle unload: has the worker gone at least
/// `IDLE_UNLOAD_AFTER` without a Transcribe or Prewarm command reaching it.
/// Split out from `worker_loop` so the threshold has a fast unit test
/// instead of needing a live worker thread and a real ten-minute wait.
fn idle_unload_due(idle_for: Duration) -> bool {
    idle_for >= IDLE_UNLOAD_AFTER
}

fn worker() -> Result<&'static mpsc::SyncSender<WorkerCommand>, String> {
    WORKER
        .get_or_init(|| {
            // One queued utterance plus the one actively running. More would retain
            // an unbounded stack of raw PCM when a user toggles rapidly on a slow
            // CPU; reject excess work with a clear busy error instead.
            let (tx, rx) = mpsc::sync_channel::<WorkerCommand>(1);
            std::thread::Builder::new()
                .name("qd-local-stt".into())
                .spawn(move || worker_loop(rx))
                .map_err(|e| format!("could not start local STT worker: {e}"))?;
            Ok(tx)
        })
        .as_ref()
        .map_err(Clone::clone)
}

pub async fn transcribe(
    model_id: String,
    language: String,
    pcm: Vec<i16>,
    cancel: Arc<AtomicBool>,
) -> Result<Option<String>, String> {
    let (result_tx, result_rx) = oneshot::channel();
    let job = Job {
        model_id,
        language,
        pcm,
        cancel,
        result: result_tx,
    };
    worker()?
        .try_send(WorkerCommand::Transcribe(job))
        .map_err(|e| match e {
            mpsc::TrySendError::Full(_) => {
                "local transcription engine is busy; wait for the previous dictation".to_string()
            }
            mpsc::TrySendError::Disconnected(_) => "local STT worker stopped".to_string(),
        })?;
    result_rx
        .await
        .map_err(|_| "local STT worker stopped".to_string())?
}

/// Load the selected model and execute one short silent inference in the
/// background. Model loading alone does not compile every Vulkan pipeline;
/// the silent run pays that one-time driver cost before the user's first
/// dictation instead of making the first result appear to hang.
pub fn request_prewarm(model_id: &str) {
    if !is_installed(model_id) {
        return;
    }
    let command = WorkerCommand::Prewarm(model_id.to_string());
    match worker() {
        Ok(worker) => match worker.try_send(command) {
            Ok(()) => tracing::info!("local STT prewarm queued for '{model_id}'"),
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::debug!("local STT prewarm skipped because the worker is busy")
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                tracing::warn!("local STT prewarm skipped because the worker stopped")
            }
        },
        Err(e) => tracing::warn!("local STT prewarm could not start: {e}"),
    }
}

/// Drop a cached multi-gigabyte model when Settings switches away from Local
/// (or changes local models). While Local remains selected, keeping the model
/// resident avoids repeatedly paying model-load and Vulkan-pipeline warmup.
pub fn request_unload() {
    if let Some(Ok(worker)) = WORKER.get() {
        UNLOAD_REQUESTED.store(true, Ordering::Release);
        let _ = worker.try_send(WorkerCommand::Unload);
    }
}

fn worker_loop(rx: mpsc::Receiver<WorkerCommand>) {
    let mut engine: Option<NativeEngine> = None;
    let mut idle_since = Instant::now();
    loop {
        // The wait only ever runs between commands (Transcribe and Prewarm
        // are handled synchronously below before the loop comes back here),
        // so a timeout can never race an in-flight transcription or prewarm.
        // Waiting for only the remaining budget, rather than the full
        // constant, means a burst of quick commands does not each restart a
        // fresh ten-minute window.
        let command = match rx.recv_timeout(IDLE_UNLOAD_AFTER.saturating_sub(idle_since.elapsed()))
        {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if idle_unload_due(idle_since.elapsed()) {
                    if engine.take().is_some() {
                        tracing::info!(
                            "local STT model unloaded after {} minutes idle",
                            IDLE_UNLOAD_AFTER.as_secs() / 60
                        );
                    }
                    idle_since = Instant::now();
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        idle_since = Instant::now();
        match command {
            WorkerCommand::Unload => {
                UNLOAD_REQUESTED.store(false, Ordering::Release);
                if engine.take().is_some() {
                    tracing::info!("local STT model unloaded after provider/model change");
                }
            }
            WorkerCommand::Prewarm(model_id) => {
                let started = Instant::now();
                let result = (|| {
                    if !is_installed(&model_id) {
                        return Err(format!("local model '{model_id}' is not installed"));
                    }
                    if engine.is_none() {
                        engine = Some(unsafe { NativeEngine::load()? });
                    }
                    unsafe {
                        engine
                            .as_mut()
                            .expect("initialized above")
                            .prewarm(&model_id)
                    }
                })();
                match result {
                    Ok(warmed) if warmed => tracing::info!(
                        "local STT prewarmed '{model_id}' in {:.2}s",
                        started.elapsed().as_secs_f32()
                    ),
                    Ok(_) => tracing::debug!("local STT '{model_id}' was already warm"),
                    Err(e) => tracing::warn!(
                        "local STT prewarm for '{model_id}' failed after {:.2}s: {e}",
                        started.elapsed().as_secs_f32()
                    ),
                }
            }
            WorkerCommand::Transcribe(job) => {
                let started = Instant::now();
                let audio_seconds = job.pcm.len() as f32 / 16_000.0;
                let result = (|| {
                    if !is_installed(&job.model_id) {
                        return Err(format!(
                            "local model '{}' is not installed; install it in Settings",
                            job.model_id
                        ));
                    }
                    if engine.is_none() {
                        engine = Some(unsafe { NativeEngine::load()? });
                    }
                    unsafe {
                        engine.as_mut().expect("initialized above").run(
                            &job.model_id,
                            &job.language,
                            &job.pcm,
                            &job.cancel,
                        )
                    }
                })();
                tracing::info!(
                    "local STT processed {audio_seconds:.1}s of audio in {:.2}s",
                    started.elapsed().as_secs_f32()
                );
                let _ = job.result.send(result);
            }
        }
        if UNLOAD_REQUESTED.swap(false, Ordering::AcqRel) && engine.take().is_some() {
            tracing::info!("local STT model unloaded after provider/model change");
        }
    }
}

type Status = c_int;
type Session = c_void;

#[repr(C)]
struct ModelLoadParams {
    struct_size: u64,
    backend: c_int,
    gpu_device: c_int,
}

#[repr(C)]
struct RunParams {
    struct_size: u64,
    task: c_int,
    timestamps: c_int,
    pnc: c_int,
    itn: c_int,
    language: *const c_char,
    target_language: *const c_char,
    keep_special_tags: bool,
    family: *const c_void,
    spec_k_drafts: i32,
}

type VersionFn = unsafe extern "C" fn() -> *const c_char;
type StatusStringFn = unsafe extern "C" fn(c_int) -> *const c_char;
type InitBackendsFn = unsafe extern "C" fn(*const c_char) -> Status;
type LoadParamsInitFn = unsafe extern "C" fn(*mut ModelLoadParams);
type RunParamsInitFn = unsafe extern "C" fn(*mut RunParams);
type OpenFn = unsafe extern "C" fn(
    *const c_char,
    *const ModelLoadParams,
    *const c_void,
    *mut *mut Session,
) -> Status;
type FreeFn = unsafe extern "C" fn(*mut Session);
type RunFn = unsafe extern "C" fn(*mut Session, *const f32, c_int, *const RunParams) -> Status;
type FullTextFn = unsafe extern "C" fn(*const Session) -> *const c_char;
type AbortCallback = unsafe extern "C" fn(*mut c_void) -> bool;
type SetAbortFn = unsafe extern "C" fn(*mut Session, Option<AbortCallback>, *mut c_void);
type GetModelFn = unsafe extern "C" fn(*const Session) -> *const c_void;
type ModelBackendFn = unsafe extern "C" fn(*const c_void) -> *const c_char;

struct NativeApi {
    version: VersionFn,
    status_string: StatusStringFn,
    init_backends: InitBackendsFn,
    load_params_init: LoadParamsInitFn,
    run_params_init: RunParamsInitFn,
    open: OpenFn,
    free: FreeFn,
    run: RunFn,
    full_text: FullTextFn,
    set_abort: SetAbortFn,
    get_model: GetModelFn,
    model_backend: ModelBackendFn,
    _library: Library,
}

struct Loaded {
    model_id: String,
    session: *mut Session,
    cpu_only: bool,
    warmed: bool,
}

struct NativeEngine {
    api: NativeApi,
    loaded: Option<Loaded>,
}

impl Drop for NativeEngine {
    fn drop(&mut self) {
        if let Some(loaded) = self.loaded.take() {
            unsafe { (self.api.free)(loaded.session) };
        }
    }
}

impl NativeEngine {
    unsafe fn load() -> Result<Self, String> {
        let dir = runtime_dir()?;
        let dll = dir.join("transcribe.dll");
        // LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR is essential here: transcribe.dll
        // imports sibling ggml DLLs from its private downloaded directory,
        // which is intentionally not added to process PATH or any global DLL
        // search list.
        let library = unsafe {
            Library::load_with_flags(
                &dll,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        }
        .map_err(|e| format!("could not load {}: {e}", dll.display()))?;
        macro_rules! symbol {
            ($name:literal, $ty:ty) => {
                *unsafe { library.get::<$ty>(concat!($name, "\0").as_bytes()) }
                    .map_err(|e| format!("local runtime is missing {}: {e}", $name))?
            };
        }
        let api = NativeApi {
            version: symbol!("transcribe_version", VersionFn),
            status_string: symbol!("transcribe_status_string", StatusStringFn),
            init_backends: symbol!("transcribe_init_backends", InitBackendsFn),
            load_params_init: symbol!("transcribe_model_load_params_init", LoadParamsInitFn),
            run_params_init: symbol!("transcribe_run_params_init", RunParamsInitFn),
            open: symbol!("transcribe_open", OpenFn),
            free: symbol!("transcribe_session_free", FreeFn),
            run: symbol!("transcribe_run", RunFn),
            full_text: symbol!("transcribe_full_text", FullTextFn),
            set_abort: symbol!("transcribe_set_abort_callback", SetAbortFn),
            get_model: symbol!("transcribe_get_model", GetModelFn),
            model_backend: symbol!("transcribe_model_backend", ModelBackendFn),
            _library: library,
        };
        let version = c_string((api.version)());
        if version != RUNTIME_VERSION {
            return Err(format!(
                "local runtime ABI mismatch (expected {RUNTIME_VERSION}, found {version})"
            ));
        }
        let dir_c = path_cstring(&dir)?;
        let status = (api.init_backends)(dir_c.as_ptr());
        if status != 0 {
            return Err(format!(
                "could not initialize local compute backends: {}",
                c_string((api.status_string)(status))
            ));
        }
        Ok(Self { api, loaded: None })
    }

    unsafe fn ensure_model(&mut self, model_id: &str, cpu_only: bool) -> Result<(), String> {
        if self
            .loaded
            .as_ref()
            .map(|m| m.model_id == model_id && m.cpu_only == cpu_only)
            .unwrap_or(false)
        {
            return Ok(());
        }
        if let Some(old) = self.loaded.take() {
            unsafe { (self.api.free)(old.session) };
        }
        let path = model_path(model_id)?;
        // Re-hash before handing the file to the native runtime: length and
        // marker alone (see `is_installed`) do not prove the bytes were not
        // swapped after install.
        let spec = model(model_id).ok_or_else(|| format!("unknown local model '{model_id}'"))?;
        verify_model_hash_once(spec, &path)?;
        let path_c = path_cstring(&path)?;
        let mut load = std::mem::zeroed::<ModelLoadParams>();
        unsafe { (self.api.load_params_init)(&mut load) };
        if cpu_only {
            load.backend = 1; // TRANSCRIBE_BACKEND_CPU
        }
        let mut session = std::ptr::null_mut();
        let status =
            unsafe { (self.api.open)(path_c.as_ptr(), &load, std::ptr::null(), &mut session) };
        if status != 0 || session.is_null() {
            return Err(format!(
                "could not load local model: {}",
                c_string(unsafe { (self.api.status_string)(status) })
            ));
        }
        let model = unsafe { (self.api.get_model)(session) };
        let backend = c_string(unsafe { (self.api.model_backend)(model) });
        tracing::info!("local STT loaded '{model_id}' on {backend}");
        self.loaded = Some(Loaded {
            model_id: model_id.to_string(),
            session,
            cpu_only,
            warmed: false,
        });
        Ok(())
    }

    unsafe fn prewarm(&mut self, model_id: &str) -> Result<bool, String> {
        self.ensure_model(model_id, false)?;
        if self.loaded.as_ref().is_some_and(|loaded| loaded.warmed) {
            return Ok(false);
        }
        let silence = vec![0i16; 16_000];
        let cancel = Arc::new(AtomicBool::new(false));
        let _ = unsafe { self.run(model_id, "en", &silence, &cancel)? };
        Ok(true)
    }

    unsafe fn run(
        &mut self,
        model_id: &str,
        language: &str,
        pcm_i16: &[i16],
        cancel: &Arc<AtomicBool>,
    ) -> Result<Option<String>, String> {
        if pcm_i16.is_empty() {
            return Ok(None);
        }
        self.ensure_model(model_id, false)?;
        let ranges = if model_id == "cohere-q5" {
            cohere_chunk_ranges(pcm_i16, 16_000)
        } else {
            std::iter::once(0..pcm_i16.len()).collect()
        };
        if ranges.len() > 1 {
            tracing::info!(
                "local STT splitting {:.1}s Cohere audio into {} quiet-boundary clip(s)",
                pcm_i16.len() as f32 / 16_000.0,
                ranges.len()
            );
        }

        let mut parts = Vec::with_capacity(ranges.len());
        for (index, range) in ranges.into_iter().enumerate() {
            if cancel.load(Ordering::Acquire) {
                return Err("local transcription was cancelled".into());
            }
            let clip = &pcm_i16[range.clone()];
            let mut text = unsafe { self.run_one(model_id, language, clip, cancel)? };

            // If even a <=35 s clip loops, retry that clip as two smaller
            // quiet-boundary decodes before resorting to the conservative
            // sentence-run collapse below.
            if model_id == "cohere-q5"
                && text
                    .as_deref()
                    .is_some_and(|text| collapse_pathological_repetitions(text).1 > 0)
                && clip.len() >= 16_000 * 10
            {
                let low = clip.len() * 2 / 5;
                let high = clip.len() * 3 / 5;
                let split = quietest_cut(clip, low, high, 16_000).unwrap_or(clip.len() / 2);
                tracing::warn!(
                    "local STT Cohere clip {} entered a repetition loop; retrying as two shorter clips",
                    index + 1
                );
                text = join_transcript_parts([
                    unsafe { self.run_one(model_id, language, &clip[..split], cancel)? }
                        .unwrap_or_default(),
                    unsafe { self.run_one(model_id, language, &clip[split..], cancel)? }
                        .unwrap_or_default(),
                ]);
            }
            if let Some(text) = text {
                parts.push(text);
            }
        }

        let Some(joined) = join_transcript_parts(parts) else {
            return Ok(None);
        };
        let (cleaned, dropped) = collapse_pathological_repetitions(&joined);
        if dropped > 0 {
            tracing::warn!("local STT removed {dropped} repeated unit(s) from a decoder loop");
        }
        Ok((!cleaned.is_empty()).then_some(cleaned))
    }

    unsafe fn run_one(
        &mut self,
        model_id: &str,
        language: &str,
        pcm_i16: &[i16],
        cancel: &Arc<AtomicBool>,
    ) -> Result<Option<String>, String> {
        if pcm_i16.is_empty() {
            return Ok(None);
        }
        let pcm: Vec<f32> = pcm_i16.iter().map(|&v| v as f32 / 32768.0).collect();
        let language = if language.trim().is_empty() || language.eq_ignore_ascii_case("auto") {
            None
        } else {
            Some(
                CString::new(language)
                    .map_err(|_| "local transcription language contains a NUL byte".to_string())?,
            )
        };
        let mut params = std::mem::zeroed::<RunParams>();
        unsafe { (self.api.run_params_init)(&mut params) };
        params.language = language
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(std::ptr::null());
        let session = self.loaded.as_ref().expect("model loaded").session;
        unsafe {
            (self.api.set_abort)(
                session,
                Some(abort_callback),
                Arc::as_ptr(cancel) as *mut c_void,
            )
        };
        let mut status =
            unsafe { (self.api.run)(session, pcm.as_ptr(), pcm.len() as c_int, &params) };
        // A GPU driver can initialize successfully yet fail on its first graph.
        // transcribe.cpp explicitly makes this recoverable by reloading on CPU.
        if status == 8 {
            tracing::warn!("local STT GPU run failed; retrying this model on CPU");
            self.ensure_model(model_id, true)?;
            let session = self.loaded.as_ref().expect("CPU model loaded").session;
            unsafe {
                (self.api.set_abort)(
                    session,
                    Some(abort_callback),
                    Arc::as_ptr(cancel) as *mut c_void,
                )
            };
            status = unsafe { (self.api.run)(session, pcm.as_ptr(), pcm.len() as c_int, &params) };
        }
        let session = self.loaded.as_ref().expect("model loaded").session;
        if status == 13 || cancel.load(Ordering::Acquire) {
            return Err("local transcription was cancelled".into());
        }
        if status != 0 {
            return Err(format!(
                "local transcription failed: {}",
                c_string(unsafe { (self.api.status_string)(status) })
            ));
        }
        if let Some(loaded) = self.loaded.as_mut() {
            loaded.warmed = true;
        }
        let text = c_string(unsafe { (self.api.full_text)(session) });
        let text = text.trim().to_string();
        Ok((!text.is_empty()).then_some(text))
    }
}

unsafe extern "C" fn abort_callback(user_data: *mut c_void) -> bool {
    if user_data.is_null() {
        return false;
    }
    unsafe { &*(user_data as *const AtomicBool) }.load(Ordering::Acquire)
}

fn c_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn path_cstring(path: &Path) -> Result<CString, String> {
    CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| format!("path contains a NUL byte: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::thread::JoinHandle;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "quickdictate-{name}-{}-{nonce}.bin",
            std::process::id()
        ))
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buf[..n]);
        }
        String::from_utf8(request).unwrap()
    }

    fn requested_range(request: &str) -> Option<(usize, usize)> {
        request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.eq_ignore_ascii_case("range") {
                return None;
            }
            let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
            Some((start.parse().ok()?, end.parse().ok()?))
        })
    }

    fn spawn_download_server(
        data: Arc<Vec<u8>>,
        requests: usize,
        ranged: bool,
        chunk_delay: Duration,
    ) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let data = Arc::clone(&data);
                handlers.push(std::thread::spawn(move || {
                    let request = read_request(&mut stream);
                    let (start, end, status) = if ranged {
                        let (start, end) =
                            requested_range(&request).expect("range request expected");
                        (start, end, "206 Partial Content")
                    } else {
                        (0, data.len() - 1, "200 OK")
                    };
                    let body = &data[start..=end];
                    let content_range = if ranged {
                        format!("Content-Range: bytes {start}-{end}/{}\r\n", data.len())
                    } else {
                        String::new()
                    };
                    let headers = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{content_range}\
                         Connection: close\r\n\r\n",
                        body.len()
                    );
                    if stream.write_all(headers.as_bytes()).is_err() {
                        return;
                    }
                    for chunk in body.chunks(16 * 1024) {
                        if stream.write_all(chunk).is_err() {
                            return;
                        }
                        if !chunk_delay.is_zero() {
                            std::thread::sleep(chunk_delay);
                        }
                    }
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        (format!("http://{address}/model.bin"), handle)
    }

    #[test]
    fn model_manifest_is_complete_and_unique() {
        let mut ids = std::collections::HashSet::new();
        for spec in MODELS {
            assert!(ids.insert(spec.id));
            assert_eq!(spec.sha256.len(), 64);
            assert!(spec.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(spec
                .url
                .starts_with("https://huggingface.co/handy-computer/"));
            assert!(spec.url.contains("/resolve/"));
            assert!(!spec.url.contains("/resolve/main/"));
            assert!(spec.download_bytes > 500_000_000);
        }
    }

    #[test]
    fn runtime_marker_requires_exact_version_and_hash() {
        let dir = test_path("runtime-marker");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("transcribe.dll"), b"stub").unwrap();

        // No marker at all.
        assert!(!runtime_verified(&dir));

        // Empty marker: exactly the exploit this guards against, a
        // `.verified` file with no content sitting next to any file named
        // transcribe.dll.
        fs::write(dir.join(".verified"), b"").unwrap();
        assert!(!runtime_verified(&dir));

        // Wrong version, wrong hash.
        fs::write(dir.join(".verified"), b"version=0.0.0\nsha256=deadbeef\n").unwrap();
        assert!(!runtime_verified(&dir));

        // Right version, wrong hash.
        fs::write(
            dir.join(".verified"),
            format!("version={RUNTIME_VERSION}\nsha256=deadbeef\n"),
        )
        .unwrap();
        assert!(!runtime_verified(&dir));

        // Exactly the expected marker.
        fs::write(dir.join(".verified"), expected_runtime_marker()).unwrap();
        assert!(runtime_verified(&dir));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_hash_is_verified_once_then_cached_per_process() {
        let path = test_path("model-hash-cache");
        fs::write(&path, b"hello world").unwrap();
        let good_hash: &'static str =
            Box::leak(format!("{:x}", Sha256::digest(b"hello world")).into_boxed_str());
        let spec = ModelSpec {
            id: "test-model-hash-cache",
            label: "test",
            detail: "test",
            download_bytes: 11,
            filename: "unused.gguf",
            url: "https://example.invalid/unused",
            sha256: good_hash,
        };
        assert!(verify_model_hash_once(&spec, &path).is_ok());

        // A cached pass is per-process, not re-checked against the file on
        // disk; tampering after the first (and only) hash must not surface
        // here, which is exactly what makes caching safe to do only once.
        fs::write(&path, b"tampered").unwrap();
        assert!(verify_model_hash_once(&spec, &path).is_ok());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn model_hash_mismatch_is_reported_and_not_cached_as_passing() {
        let path = test_path("model-hash-mismatch");
        fs::write(&path, b"actual content").unwrap();
        let spec = ModelSpec {
            id: "test-model-hash-mismatch",
            label: "test",
            detail: "test",
            download_bytes: 14,
            filename: "unused.gguf",
            url: "https://example.invalid/unused",
            sha256: "0000000000000000000000000000000000000000000000000000000000000000",
        };
        assert!(verify_model_hash_once(&spec, &path)
            .unwrap_err()
            .contains("integrity verification"));

        // Not cached as a pass: fixing the file and re-checking succeeds.
        let good_hash: &'static str =
            Box::leak(format!("{:x}", Sha256::digest(b"actual content")).into_boxed_str());
        let fixed = ModelSpec {
            sha256: good_hash,
            ..spec
        };
        assert!(verify_model_hash_once(&fixed, &path).is_ok());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn idle_unload_only_fires_once_the_full_window_elapses() {
        assert!(!idle_unload_due(Duration::from_secs(0)));
        assert!(!idle_unload_due(IDLE_UNLOAD_AFTER - Duration::from_secs(1)));
        assert!(idle_unload_due(IDLE_UNLOAD_AFTER));
        assert!(idle_unload_due(IDLE_UNLOAD_AFTER + Duration::from_secs(1)));
    }

    #[test]
    fn cohere_long_audio_uses_quiet_boundaries_under_35_seconds() {
        let sample_rate = 1_000usize;
        let mut pcm = vec![2_000i16; sample_rate * 80];
        // Quiet gaps inside each 30–35 second search window.
        pcm[sample_rate * 33..sample_rate * 33 + 200].fill(0);
        pcm[sample_rate * 66..sample_rate * 66 + 200].fill(0);

        let ranges = cohere_chunk_ranges(&pcm, sample_rate);
        assert_eq!(ranges.first().unwrap().start, 0);
        assert_eq!(ranges.last().unwrap().end, pcm.len());
        assert!(ranges.windows(2).all(|pair| pair[0].end == pair[1].start));
        assert!(ranges
            .iter()
            .all(|range| range.len() <= sample_rate * COHERE_CLIP_MAX_SECONDS));
        assert!((32_900..=33_200).contains(&ranges[0].end));
        assert!((65_900..=66_200).contains(&ranges[1].end));
    }

    #[test]
    fn cohere_chunker_avoids_a_tiny_final_fragment() {
        let sample_rate = 1_000usize;
        let pcm = vec![1_000i16; sample_rate * 36];
        let ranges = cohere_chunk_ranges(&pcm, sample_rate);
        assert_eq!(ranges.len(), 2);
        assert!(ranges[0].len() <= sample_rate * COHERE_CLIP_MAX_SECONDS);
        assert!(ranges[1].len() >= sample_rate * COHERE_MIN_TAIL_SECONDS);
    }

    #[test]
    fn decoder_loop_guard_is_conservative() {
        let looped = "Useful start. And then there's a page. And then there's a page. \
                      And then there's a page. And then there's a page. Useful end.";
        let (cleaned, dropped) = collapse_pathological_sentence_runs(looped);
        assert_eq!(dropped, 2);
        assert_eq!(cleaned.matches("And then there's a page.").count(), 2);
        assert!(cleaned.starts_with("Useful start."));
        assert!(cleaned.ends_with("Useful end."));

        let intentional = "Hello, hello, hello. Test. Test. Test.";
        let (unchanged, dropped) = collapse_pathological_sentence_runs(intentional);
        assert_eq!(dropped, 0);
        assert_eq!(unchanged, intentional);

        let comma_loop =
            "Useful start, and here, and here, and here, and here, and here, useful end.";
        let (cleaned, dropped) = collapse_pathological_repetitions(comma_loop);
        assert_eq!(dropped, 6);
        assert_eq!(cleaned.matches("and here").count(), 2);
        assert!(cleaned.starts_with("Useful start"));
        assert!(cleaned.ends_with("useful end."));

        let alternating =
            "Alpha one. Beta two. Alpha one. Beta two. Alpha one. Beta two. Alpha one. Beta two.";
        let (cleaned, dropped) = collapse_pathological_repetitions(alternating);
        assert_eq!(dropped, 8);
        assert_eq!(cleaned.matches("Alpha one").count(), 2);
        assert_eq!(cleaned.matches("Beta two").count(), 2);
    }

    #[test]
    fn ffi_layout_matches_transcribe_0_1_3_x64() {
        assert_eq!(std::mem::size_of::<ModelLoadParams>(), 16);
        assert_eq!(std::mem::size_of::<RunParams>(), 64);
    }

    #[test]
    fn parallel_ranges_cover_every_byte_exactly_once() {
        let segments = range_segments(23, 4);
        assert_eq!(segments, vec![(0, 5), (6, 11), (12, 17), (18, 22)]);
        let covered: u64 = segments.iter().map(|(start, end)| end - start + 1).sum();
        assert_eq!(covered, 23);
        assert!(range_segments(0, 8).is_empty());
        assert_eq!(range_segments(2, 8), vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn parallel_downloader_reassembles_http_ranges() {
        let data = Arc::new(
            (0..1_048_603usize)
                .map(|i| ((i * 31) % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let (url, server) = spawn_download_server(Arc::clone(&data), 4, true, Duration::ZERO);
        let path = test_path("parallel-download");
        let cancel = AtomicBool::new(false);
        let client = download_client().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(download_parallel(
                &client,
                "parallel-download-test",
                InstallPhase::DownloadingModel,
                &url,
                data.len() as u64,
                &path,
                data.len() as u64,
                &cancel,
                4,
            ))
            .unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(&path).unwrap(), *data);
        let _ = fs::remove_file(path);
        finish_operation(
            "parallel-download-test",
            InstallPhase::NotInstalled,
            0,
            data.len() as u64,
        );
    }

    #[test]
    fn cancelling_download_stops_and_removes_partial_file() {
        let data = Arc::new(vec![0x5a; 4 * 1024 * 1024]);
        let expected_sha256 = format!("{:x}", Sha256::digest(data.as_slice()));
        let (url, server) =
            spawn_download_server(Arc::clone(&data), 1, false, Duration::from_millis(2));
        let dest = test_path("cancel-download");
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker_dest = dest.clone();
        let total = data.len() as u64;
        let worker = std::thread::spawn(move || {
            download_verified(
                "cancel-download-test",
                InstallPhase::DownloadingModel,
                &url,
                total,
                &expected_sha256,
                &worker_dest,
                total,
                &worker_cancel,
            )
        });
        std::thread::sleep(Duration::from_millis(30));
        cancel.store(true, Ordering::Release);
        let result = worker.join().unwrap();
        server.join().unwrap();
        assert!(result.unwrap_err().contains("cancelled"));
        assert!(!dest.exists());
        assert!(!dest.with_extension("part").exists());
        finish_operation("cancel-download-test", InstallPhase::NotInstalled, 0, total);
    }

    #[test]
    #[ignore = "downloads a 591 MiB model and runs real native inference"]
    fn live_whisper_pack_download_load_and_transcribe() {
        let root =
            std::env::temp_dir().join(format!("quickdictate-local-e2e-{}", std::process::id()));
        let old = std::env::var_os("LOCALAPPDATA");
        std::env::set_var("LOCALAPPDATA", &root);

        let result = (|| {
            let spec = model("whisper-turbo-q5").unwrap();
            if !is_installed(spec.id) {
                install(spec, &AtomicBool::new(false))?;
            }
            let mut reader = hound::WavReader::open("tests/fixtures/speech_16k.wav")
                .map_err(|e| e.to_string())?;
            assert_eq!(reader.spec().sample_rate, 16_000);
            assert_eq!(reader.spec().channels, 1);
            let pcm = reader
                .samples::<i16>()
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            let cancel = Arc::new(AtomicBool::new(false));
            let mut engine = unsafe { NativeEngine::load()? };
            let transcript =
                unsafe { engine.run(spec.id, "en", &pcm, &cancel)? }.unwrap_or_default();
            if transcript.trim().is_empty() {
                return Err("real local inference returned an empty transcript".into());
            }
            tracing::info!("local E2E transcript: {transcript}");
            Ok::<(), String>(())
        })();

        if let Some(old) = old {
            std::env::set_var("LOCALAPPDATA", old);
        } else {
            std::env::remove_var("LOCALAPPDATA");
        }
        if std::env::var_os("QUICKDICTATE_KEEP_LOCAL_E2E").is_none() {
            let _ = fs::remove_dir_all(&root);
        }
        result.unwrap();
    }

    #[test]
    #[ignore = "loads the user's installed 1.65 GiB Cohere model and runs real native inference"]
    fn live_installed_cohere_prewarm_and_transcribe() {
        let spec = model("cohere-q5").unwrap();
        assert!(
            is_installed(spec.id),
            "install '{}' in QuickDictate Settings before running this test",
            spec.label
        );

        let mut reader = hound::WavReader::open("tests/fixtures/speech_16k.wav").unwrap();
        assert_eq!(reader.spec().sample_rate, 16_000);
        assert_eq!(reader.spec().channels, 1);
        let pcm = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut engine = unsafe { NativeEngine::load().unwrap() };

        let prewarm_started = Instant::now();
        assert!(unsafe { engine.prewarm(spec.id).unwrap() });
        eprintln!(
            "Cohere prewarm completed in {:.2}s",
            prewarm_started.elapsed().as_secs_f32()
        );

        let inference_started = Instant::now();
        let transcript =
            unsafe { engine.run(spec.id, "en", &pcm, &cancel).unwrap() }.unwrap_or_default();
        eprintln!(
            "Cohere fixture inference completed in {:.2}s: {transcript}",
            inference_started.elapsed().as_secs_f32()
        );
        assert!(
            !transcript.trim().is_empty(),
            "real Cohere inference returned an empty transcript"
        );
    }
}
