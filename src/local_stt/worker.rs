//! The single background thread that owns the loaded model.
//!
//! One resident [`NativeEngine`] serialises every transcribe and prewarm, and
//! releases multi-gigabyte weights on request or after an idle timeout.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use super::is_installed;
use super::native::NativeEngine;

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
pub(super) const IDLE_UNLOAD_AFTER: Duration = Duration::from_secs(10 * 60);

/// Pure decision behind the idle unload: has the worker gone at least
/// `IDLE_UNLOAD_AFTER` without a Transcribe or Prewarm command reaching it.
/// Split out from `worker_loop` so the threshold has a fast unit test
/// instead of needing a live worker thread and a real ten-minute wait.
pub(super) fn idle_unload_due(idle_for: Duration) -> bool {
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

/// The idle-timeout branch of [`worker_loop`]: release the model once it has
/// sat unused for `IDLE_UNLOAD_AFTER`, and hand back the `idle_since` the loop
/// should keep waiting from.
fn worker_handle_idle_timeout(engine: &mut Option<NativeEngine>, idle_since: Instant) -> Instant {
    if !idle_unload_due(idle_since.elapsed()) {
        return idle_since;
    }
    if engine.take().is_some() {
        tracing::info!(
            "local STT model unloaded after {} minutes idle",
            IDLE_UNLOAD_AFTER.as_secs() / 60
        );
    }
    Instant::now()
}

/// The `WorkerCommand::Unload` arm of [`worker_loop`].
fn worker_handle_unload(engine: &mut Option<NativeEngine>) {
    UNLOAD_REQUESTED.store(false, Ordering::Release);
    if engine.take().is_some() {
        tracing::info!("local STT model unloaded after provider/model change");
    }
}

/// The `WorkerCommand::Prewarm` arm of [`worker_loop`].
fn worker_handle_prewarm(engine: &mut Option<NativeEngine>, model_id: &str) {
    let started = Instant::now();
    let result = (|| {
        if !is_installed(model_id) {
            return Err(format!("local model '{model_id}' is not installed"));
        }
        let loaded = match engine.as_mut() {
            Some(e) => e,
            None => engine.insert(unsafe { NativeEngine::load()? }),
        };
        unsafe { loaded.prewarm(model_id) }
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

/// The `WorkerCommand::Transcribe` arm of [`worker_loop`].
fn worker_handle_transcribe(engine: &mut Option<NativeEngine>, job: Job) {
    let started = Instant::now();
    let audio_seconds = job.pcm.len() as f32 / 16_000.0;
    let result = (|| {
        if !is_installed(&job.model_id) {
            return Err(format!(
                "local model '{}' is not installed; install it in Settings",
                job.model_id
            ));
        }
        let loaded = match engine.as_mut() {
            Some(e) => e,
            None => engine.insert(unsafe { NativeEngine::load()? }),
        };
        unsafe { loaded.run(&job.model_id, &job.language, &job.pcm, &job.cancel) }
    })();
    tracing::info!(
        "local STT processed {audio_seconds:.1}s of audio in {:.2}s",
        started.elapsed().as_secs_f32()
    );
    let _ = job.result.send(result);
}

/// Release the model if a concurrent [`request_unload`] came in while the
/// command above was running (checked once per loop iteration, after every
/// arm).
fn worker_release_if_requested(engine: &mut Option<NativeEngine>) {
    if UNLOAD_REQUESTED.swap(false, Ordering::AcqRel) && engine.take().is_some() {
        tracing::info!("local STT model unloaded after provider/model change");
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
                idle_since = worker_handle_idle_timeout(&mut engine, idle_since);
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        idle_since = Instant::now();
        match command {
            WorkerCommand::Unload => worker_handle_unload(&mut engine),
            WorkerCommand::Prewarm(model_id) => worker_handle_prewarm(&mut engine, &model_id),
            WorkerCommand::Transcribe(job) => worker_handle_transcribe(&mut engine, job),
        }
        worker_release_if_requested(&mut engine);
    }
}
