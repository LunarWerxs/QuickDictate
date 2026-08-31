//! Fetching a pinned file over HTTP and proving it is the pinned file.
//!
//! Parallel ranged downloads with per-range retry, single-stream fallback,
//! cancellation, and SHA-256 verification; a partial file is never activated.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::install::{set_state, InstallPhase};
use super::ModelSpec;

const USER_AGENT: &str = concat!("QuickDictate/", env!("CARGO_PKG_VERSION"));
const PARALLEL_DOWNLOAD_MIN_BYTES: u64 = 32 * 1024 * 1024;
const PARALLEL_DOWNLOAD_WORKERS: usize = 8;
const DOWNLOAD_BUFFER_BYTES: usize = 1024 * 1024;
const DOWNLOAD_RANGE_ATTEMPTS: usize = 3;

/// Download `url` into `part`, choosing parallel ranged fetch or a single
/// stream, and return the resulting file's SHA-256. Split out of
/// `download_verified` so its choice-of-strategy branching doesn't add to
/// that function's own retry/cleanup nesting.
#[allow(clippy::too_many_arguments)]
fn fetch_to_part(
    runtime: &tokio::runtime::Runtime,
    client: &reqwest::Client,
    id: &str,
    phase: InstallPhase,
    url: &str,
    expected_bytes: u64,
    part: &Path,
    display_total: u64,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let parallel = expected_bytes >= PARALLEL_DOWNLOAD_MIN_BYTES
        && runtime.block_on(server_supports_ranges(client, url, expected_bytes, cancel))?;
    if parallel {
        tracing::info!(
            "downloading {expected_bytes} bytes with {PARALLEL_DOWNLOAD_WORKERS} parallel ranges"
        );
        runtime.block_on(download_parallel(
            client,
            id,
            phase,
            url,
            expected_bytes,
            part,
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
        hash_file(part, cancel)
    } else {
        tracing::info!("downloading {expected_bytes} bytes as one HTTP stream");
        runtime.block_on(download_single(
            client,
            id,
            phase,
            url,
            expected_bytes,
            part,
            display_total,
            cancel,
        ))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn download_verified(
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
        let actual = fetch_to_part(
            &runtime,
            &client,
            id,
            phase,
            url,
            expected_bytes,
            &part,
            display_total,
            cancel,
        )?;
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

pub(super) fn check_cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err("download cancelled".into())
    } else {
        Ok(())
    }
}

pub(super) fn download_client() -> Result<reqwest::Client, String> {
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

pub(super) fn range_segments(total: u64, workers: usize) -> Vec<(u64, u64)> {
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
pub(super) async fn download_parallel(
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

/// Reads one attempt's response body into `file`, chunk by chunk, advancing
/// `progress` and the write position as it goes. Returns the byte offset
/// reached, plus a retry reason when the stream ended early (a read failure
/// or a truncated response) so the caller's retry loop keeps that message to
/// report if every attempt runs out. `Err` only for the cases that should
/// abort the whole download outright: cancellation, a sibling range failing,
/// or the server sending more than was asked for.
#[allow(clippy::too_many_arguments)]
async fn write_range_chunks(
    response: &mut reqwest::Response,
    file: &mut File,
    start: u64,
    end: u64,
    mut next: u64,
    id: &str,
    phase: InstallPhase,
    display_total: u64,
    progress: &AtomicU64,
    cancel: &AtomicBool,
    failed: &AtomicBool,
) -> Result<(u64, Option<String>), String> {
    let mut last_error = Some(format!("response ended before byte {end}"));
    while next <= end {
        check_cancelled(cancel)?;
        if failed.load(Ordering::Acquire) {
            return Err("parallel download stopped after another range failed".into());
        }
        let limit = (end - next + 1).min(DOWNLOAD_BUFFER_BYTES as u64) as usize;
        let chunk = match next_chunk_with_cancel(response, cancel).await {
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
        last_error = None;
    }
    Ok((next, last_error))
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

        let (updated_next, retry_reason) = write_range_chunks(
            &mut response,
            &mut file,
            start,
            end,
            next,
            id,
            phase.clone(),
            display_total,
            progress,
            cancel,
            failed,
        )
        .await?;
        next = updated_next;
        if next > end {
            return Ok(());
        }
        last_error = retry_reason;
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
pub(super) fn verify_model_hash_once(spec: &ModelSpec, path: &Path) -> Result<(), String> {
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

pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
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
