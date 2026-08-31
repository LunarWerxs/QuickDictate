//! The transcribe.cpp FFI boundary.
//!
//! Loads the pinned runtime DLL from its own private directory, checks its ABI
//! version, and drives one session's model load and inference.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use libloading::os::windows::{
    Library, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

use super::download::verify_model_hash_once;
use super::postprocess::{
    cohere_chunk_ranges, collapse_pathological_repetitions, join_transcript_parts, quietest_cut,
};
use super::{model, model_path, runtime_dir, RUNTIME_VERSION};

type Status = c_int;
type Session = c_void;

#[repr(C)]
pub(super) struct ModelLoadParams {
    struct_size: u64,
    backend: c_int,
    gpu_device: c_int,
}

#[repr(C)]
pub(super) struct RunParams {
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

pub(super) struct NativeEngine {
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
    pub(super) unsafe fn load() -> Result<Self, String> {
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

    pub(super) unsafe fn prewarm(&mut self, model_id: &str) -> Result<bool, String> {
        self.ensure_model(model_id, false)?;
        if self.loaded.as_ref().is_some_and(|loaded| loaded.warmed) {
            return Ok(false);
        }
        let silence = vec![0i16; 16_000];
        let cancel = Arc::new(AtomicBool::new(false));
        let _ = unsafe { self.run(model_id, "en", &silence, &cancel)? };
        Ok(true)
    }

    pub(super) unsafe fn run(
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
        // `run_one` is only reached through `run`, which calls `ensure_model`
        // first -- but "only reached through" is an invariant a future caller
        // can break, and breaking it here would abort a background thread with
        // no console to print to. Report it as the error it is instead.
        let session = self
            .loaded
            .as_ref()
            .ok_or("no local model is loaded")?
            .session;
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
            let session = self
                .loaded
                .as_ref()
                .ok_or("the CPU reload left no local model loaded")?
                .session;
            unsafe {
                (self.api.set_abort)(
                    session,
                    Some(abort_callback),
                    Arc::as_ptr(cancel) as *mut c_void,
                )
            };
            status = unsafe { (self.api.run)(session, pcm.as_ptr(), pcm.len() as c_int, &params) };
        }
        // Re-read: the GPU-failure branch above may have swapped the session.
        let session = self
            .loaded
            .as_ref()
            .ok_or("no local model is loaded")?
            .session;
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
