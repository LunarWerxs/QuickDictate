//! Optional, on-demand local speech-to-text packs.
//!
//! QuickDictate itself ships no model weights and no native inference DLLs.
//! Settings can install one of the pinned model packs below into
//! `%LOCALAPPDATA%\QuickDictate\local-stt`. Downloads use an immutable upstream
//! revision, an exact byte count, and SHA-256; partial files never become active.
//! Both models share one pinned transcribe.cpp CPU/Vulkan runtime.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

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
            .map(|p| p.join("transcribe.dll").is_file() && p.join(".verified").is_file())
            .unwrap_or(false)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallPhase {
    NotInstalled,
    DownloadingRuntime,
    InstallingRuntime,
    DownloadingModel,
    Installed,
    Removing,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct InstallSnapshot {
    pub phase: InstallPhase,
    pub downloaded: u64,
    pub total: u64,
}

impl InstallSnapshot {
    pub fn busy(&self) -> bool {
        matches!(
            self.phase,
            InstallPhase::DownloadingRuntime
                | InstallPhase::InstallingRuntime
                | InstallPhase::DownloadingModel
                | InstallPhase::Removing
        )
    }
}

fn states() -> &'static Mutex<HashMap<String, InstallSnapshot>> {
    static STATES: OnceLock<Mutex<HashMap<String, InstallSnapshot>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn install_snapshot(id: &str) -> InstallSnapshot {
    if let Some(state) = states().lock().ok().and_then(|s| s.get(id).cloned()) {
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
    if let Ok(mut states) = states().lock() {
        states.insert(
            id.to_string(),
            InstallSnapshot {
                phase,
                downloaded,
                total,
            },
        );
    }
}

fn claim_operation(id: &str, phase: InstallPhase, total: u64) -> Result<(), String> {
    let mut states = states()
        .lock()
        .map_err(|_| "local model installer state is unavailable".to_string())?;
    if states.values().any(InstallSnapshot::busy) {
        return Err("another local model install/remove operation is already running".into());
    }
    states.insert(
        id.to_string(),
        InstallSnapshot {
            phase,
            downloaded: 0,
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
    claim_operation(id, InstallPhase::DownloadingRuntime, spec.download_bytes)?;
    let spawn = std::thread::Builder::new()
        .name(format!("qd-model-install-{}", spec.id))
        .spawn(move || {
            let result = install(&spec);
            match result {
                Ok(()) => set_state(
                    spec.id,
                    InstallPhase::Installed,
                    spec.download_bytes,
                    spec.download_bytes,
                ),
                Err(e) => {
                    tracing::error!("local model '{}' install failed: {e}", spec.id);
                    set_state(spec.id, InstallPhase::Failed(e), 0, spec.download_bytes);
                }
            }
        });
    match spawn {
        Ok(_) => Ok(()),
        Err(e) => {
            let message = format!("could not start model installer: {e}");
            set_state(
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
    claim_operation(spec.id, InstallPhase::Removing, spec.download_bytes)?;
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
                Ok(()) => set_state(spec.id, InstallPhase::NotInstalled, 0, spec.download_bytes),
                Err(e) => set_state(spec.id, InstallPhase::Failed(e), 0, spec.download_bytes),
            }
        });
    match spawn {
        Ok(_) => Ok(()),
        Err(e) => {
            let message = format!("could not start model removal: {e}");
            set_state(
                spec.id,
                InstallPhase::Failed(message.clone()),
                0,
                spec.download_bytes,
            );
            Err(message)
        }
    }
}

fn install(spec: &ModelSpec) -> Result<(), String> {
    ensure_runtime(spec)?;
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
    )?;
    let marker = marker_path(spec)?;
    write_atomic(&marker, expected_marker(spec).as_bytes())?;
    Ok(())
}

fn ensure_runtime(spec: &ModelSpec) -> Result<(), String> {
    let final_dir = runtime_dir()?;
    if final_dir.join("transcribe.dll").is_file() && final_dir.join(".verified").is_file() {
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
    )?;
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
        let extracted = staging.join(RUNTIME_ARCHIVE_ROOT);
        if !extracted.join("transcribe.dll").is_file() || !extracted.join("contract.json").is_file()
        {
            return Err("downloaded runtime did not contain its required files".into());
        }
        write_atomic(
            &extracted.join(".verified"),
            format!("version={RUNTIME_VERSION}\nsha256={RUNTIME_SHA256}\n").as_bytes(),
        )?;
        if final_dir.exists() {
            fs::remove_dir_all(&final_dir)
                .map_err(|e| format!("could not replace {}: {e}", final_dir.display()))?;
        }
        fs::rename(&extracted, &final_dir)
            .map_err(|e| format!("could not activate local runtime: {e}"))?;
        Ok(())
    })();
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&staging);
    unpack_result
}

fn download_verified(
    id: &str,
    phase: InstallPhase,
    url: &str,
    expected_bytes: u64,
    expected_sha256: &str,
    dest: &Path,
    display_total: u64,
) -> Result<(), String> {
    let parent = dest
        .parent()
        .ok_or_else(|| "download destination has no parent".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    let part = dest.with_extension("part");
    let _ = fs::remove_file(&part);
    let result = (|| {
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(4 * 60 * 60))
            .build()
            .map_err(|e| format!("could not create download client: {e}"))?;
        let mut response = client
            .get(url)
            .send()
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
            File::create(&part).map_err(|e| format!("could not create {}: {e}", part.display()))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        let mut downloaded = 0u64;
        loop {
            let n = response
                .read(&mut buf)
                .map_err(|e| format!("download read failed: {e}"))?;
            if n == 0 {
                break;
            }
            downloaded = downloaded.saturating_add(n as u64);
            if downloaded > expected_bytes {
                return Err("download exceeded its pinned size".into());
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])
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
        let actual = format!("{:x}", hasher.finalize());
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

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    if path.exists() {
        fs::remove_file(path).map_err(|e| format!("could not replace {}: {e}", path.display()))?;
    }
    fs::rename(&tmp, path).map_err(|e| format!("could not save {}: {e}", path.display()))
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
    Unload,
}

static WORKER: OnceLock<Result<mpsc::SyncSender<WorkerCommand>, String>> = OnceLock::new();
static UNLOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

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

/// Drop a cached multi-gigabyte model when Settings switches away from Local
/// (or changes local models). The worker also unloads after five idle minutes.
pub fn request_unload() {
    if let Some(Ok(worker)) = WORKER.get() {
        UNLOAD_REQUESTED.store(true, Ordering::Release);
        let _ = worker.try_send(WorkerCommand::Unload);
    }
}

fn worker_loop(rx: mpsc::Receiver<WorkerCommand>) {
    let mut engine: Option<NativeEngine> = None;
    loop {
        let command = match rx.recv_timeout(Duration::from_secs(5 * 60)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if engine.take().is_some() {
                    tracing::info!("local STT model unloaded after five idle minutes");
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let WorkerCommand::Transcribe(job) = command else {
            UNLOAD_REQUESTED.store(false, Ordering::Release);
            if engine.take().is_some() {
                tracing::info!("local STT model unloaded after provider/model change");
            }
            continue;
        };
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
        let _ = job.result.send(result);
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
        });
        Ok(())
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
    fn ffi_layout_matches_transcribe_0_1_3_x64() {
        assert_eq!(std::mem::size_of::<ModelLoadParams>(), 16);
        assert_eq!(std::mem::size_of::<RunParams>(), 64);
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
                install(spec)?;
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
}
