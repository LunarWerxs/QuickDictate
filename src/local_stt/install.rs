//! Installing and removing a local model pack.
//!
//! Owns the install state machine the Settings UI polls, and the
//! download-then-activate sequence for the shared runtime and the model file.

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use flate2::read::GzDecoder;

use super::download::{check_cancelled, download_verified, write_atomic};
use super::{
    expected_marker, expected_runtime_marker, is_installed, marker_path, model, model_dir,
    root_dir, runtime_dir, runtime_verified, ModelSpec, RUNTIME_SHA256, RUNTIME_VERSION,
};

const RUNTIME_URL: &str = "https://github.com/handy-computer/transcribe.cpp/releases/download/v0.1.3/transcribe-native-0.1.3-windows-x86_64-cpu-vulkan.tar.gz";
const RUNTIME_BYTES: u64 = 25_957_910;
const RUNTIME_ARCHIVE_ROOT: &str = "transcribe-native-windows-x86_64-cpu-vulkan";

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

pub(super) fn set_state(id: &str, phase: InstallPhase, downloaded: u64, total: u64) {
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

pub(super) fn finish_operation(id: &str, phase: InstallPhase, downloaded: u64, total: u64) {
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

pub(super) fn install(spec: &ModelSpec, cancel: &AtomicBool) -> Result<(), String> {
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
    let unpack_result = unpack_runtime(&archive, &staging, &final_dir, cancel);
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&staging);
    unpack_result
}

/// Extract a downloaded runtime archive into `staging` and, once verified,
/// activate it at `final_dir`. Split out of `ensure_runtime` so callers there
/// don't have to track its cleanup-on-either-outcome nesting too.
fn unpack_runtime(
    archive: &Path,
    staging: &Path,
    final_dir: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let file =
        File::open(archive).map_err(|e| format!("could not open downloaded runtime: {e}"))?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    // `unpack` routes every entry through tar's traversal-safe `unpack_in`.
    tar.unpack(staging)
        .map_err(|e| format!("could not extract local runtime: {e}"))?;
    check_cancelled(cancel)?;
    let extracted = staging.join(RUNTIME_ARCHIVE_ROOT);
    if !extracted.join("transcribe.dll").is_file() || !extracted.join("contract.json").is_file() {
        return Err("downloaded runtime did not contain its required files".into());
    }
    write_atomic(
        &extracted.join(".verified"),
        expected_runtime_marker().as_bytes(),
    )?;
    if final_dir.exists() {
        fs::remove_dir_all(final_dir)
            .map_err(|e| format!("could not replace {}: {e}", final_dir.display()))?;
    }
    fs::rename(&extracted, final_dir)
        .map_err(|e| format!("could not activate local runtime: {e}"))?;
    check_cancelled(cancel)?;
    Ok(())
}
