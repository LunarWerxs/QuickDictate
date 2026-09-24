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

const PARALLEL_DOWNLOAD_MIN_BYTES: u64 = 32 * 1024 * 1024;
const PARALLEL_DOWNLOAD_WORKERS: usize = 8;
const DOWNLOAD_BUFFER_BYTES: usize = 1024 * 1024;
const DOWNLOAD_RANGE_ATTEMPTS: usize = 3;

/// One pinned file on its way into `part`: what every step of a download
/// needs to fetch it, report progress and stop when cancelled. Built once by
/// `download_verified` and borrowed by each step below.
pub(super) struct Fetch<'a> {
    pub(super) client: &'a reqwest::Client,
    pub(super) id: &'a str,
    pub(super) phase: InstallPhase,
    pub(super) url: &'a str,
    pub(super) expected_bytes: u64,
    pub(super) part: &'a Path,
    pub(super) display_total: u64,
    pub(super) cancel: &'a AtomicBool,
}

impl Fetch<'_> {
    fn report(&self, downloaded: u64) {
        set_state(self.id, self.phase.clone(), downloaded, self.display_total);
    }
}

/// One range of a parallel download, plus the counters every range shares.
struct Range<'a> {
    start: u64,
    end: u64,
    progress: &'a AtomicU64,
    failed: &'a AtomicBool,
}

/// Download `url` into `part`, choosing parallel ranged fetch or a single
/// stream, and return the resulting file's SHA-256. Split out of
/// `download_verified` so its choice-of-strategy branching doesn't add to
/// that function's own retry/cleanup nesting.
fn fetch_to_part(runtime: &tokio::runtime::Runtime, fetch: &Fetch<'_>) -> Result<String, String> {
    let expected_bytes = fetch.expected_bytes;
    let parallel = expected_bytes >= PARALLEL_DOWNLOAD_MIN_BYTES
        && runtime.block_on(server_supports_ranges(
            fetch.client,
            fetch.url,
            expected_bytes,
            fetch.cancel,
        ))?;
    if parallel {
        tracing::info!(
            "downloading {expected_bytes} bytes with {PARALLEL_DOWNLOAD_WORKERS} parallel ranges"
        );
        runtime.block_on(download_parallel(fetch, PARALLEL_DOWNLOAD_WORKERS))?;
        set_state(
            fetch.id,
            InstallPhase::VerifyingDownload,
            expected_bytes,
            fetch.display_total,
        );
        hash_file(fetch.part, fetch.cancel)
    } else {
        tracing::info!("downloading {expected_bytes} bytes as one HTTP stream");
        runtime.block_on(download_single(fetch))
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
    let part = fresh_part_path(dest)?;
    let result = (|| {
        check_cancelled(cancel)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("could not start download runtime: {e}"))?;
        let client = download_client()?;
        let fetch = Fetch {
            client: &client,
            id,
            phase,
            url,
            expected_bytes,
            part: &part,
            display_total,
            cancel,
        };
        let actual = fetch_to_part(&runtime, &fetch)?;
        activate_verified(&actual, expected_sha256, &part, dest, cancel)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

/// Create `dest`'s folder and return its `.part` sibling, with any stale
/// partial from an earlier attempt already removed.
fn fresh_part_path(dest: &Path) -> Result<std::path::PathBuf, String> {
    let parent = dest
        .parent()
        .ok_or_else(|| "download destination has no parent".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    let part = dest.with_extension("part");
    let _ = fs::remove_file(&part);
    Ok(part)
}

/// Move a fully downloaded `part` over `dest`, but only once its hash matches
/// the pinned one: a partial or tampered file is never activated.
fn activate_verified(
    actual_sha256: &str,
    expected_sha256: &str,
    part: &Path,
    dest: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    check_cancelled(cancel)?;
    if actual_sha256 != expected_sha256 {
        return Err("download failed SHA-256 verification".into());
    }
    if dest.exists() {
        fs::remove_file(dest).map_err(|e| format!("could not replace {}: {e}", dest.display()))?;
    }
    fs::rename(part, dest).map_err(|e| format!("could not activate {}: {e}", dest.display()))
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
        .user_agent(crate::http::USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(4 * 60 * 60))
        .build()
        .map_err(|e| format!("could not create download client: {e}"))
}

/// Also fails the download once a sibling range has failed, so the other
/// ranges stop instead of finishing a file that will be thrown away.
fn check_aborted(cancel: &AtomicBool, failed: &AtomicBool) -> Result<(), String> {
    check_cancelled(cancel)?;
    if failed.load(Ordering::Acquire) {
        return Err("parallel download stopped after another range failed".into());
    }
    Ok(())
}

/// Await one network step, polling `cancel` every 100 ms so a stalled
/// connection cannot hold a cancelled install open. `what` prefixes the
/// network error.
async fn until_cancelled<T>(
    step: impl std::future::Future<Output = reqwest::Result<T>>,
    cancel: &AtomicBool,
    what: &str,
) -> Result<T, String> {
    tokio::pin!(step);
    loop {
        tokio::select! {
            result = &mut step => {
                return result.map_err(|e| format!("{what}: {e}"));
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                check_cancelled(cancel)?;
            }
        }
    }
}

async fn send_with_cancel(
    request: reqwest::RequestBuilder,
    cancel: &AtomicBool,
) -> Result<reqwest::Response, String> {
    until_cancelled(request.send(), cancel, "download request failed").await
}

async fn next_chunk_with_cancel(
    response: &mut reqwest::Response,
    cancel: &AtomicBool,
) -> Result<Option<bytes::Bytes>, String> {
    until_cancelled(response.chunk(), cancel, "download read failed").await
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

async fn download_single(fetch: &Fetch<'_>) -> Result<String, String> {
    let (expected_bytes, part, cancel) = (fetch.expected_bytes, fetch.part, fetch.cancel);
    let mut response = send_with_cancel(fetch.client.get(fetch.url), cancel)
        .await
        .map_err(|e| format!("download failed: {e}"))?;
    check_single_response(&response, expected_bytes)?;
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
        fetch.report(downloaded);
    }
    file.sync_all()
        .map_err(|e| format!("could not flush download: {e}"))?;
    if downloaded != expected_bytes {
        return Err(format!(
            "download was incomplete (expected {expected_bytes} bytes, got {downloaded})"
        ));
    }
    Ok(hex_digest(hasher))
}

/// Reject a single-stream response that failed, or whose announced size is
/// not the pinned one, before anything is written.
fn check_single_response(response: &reqwest::Response, expected_bytes: u64) -> Result<(), String> {
    if !response.status().is_success() {
        return Err(format!("download failed: HTTP {}", response.status()));
    }
    match response.content_length() {
        Some(len) if len != expected_bytes => Err(format!(
            "download size changed upstream (expected {expected_bytes}, got {len})"
        )),
        _ => Ok(()),
    }
}

/// Lowercase hex SHA-256, the form the pinned hashes are written in. One
/// helper so the streamed and the re-read hash cannot drift apart.
fn hex_digest(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
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

pub(super) async fn download_parallel(fetch: &Fetch<'_>, workers: usize) -> Result<(), String> {
    let (expected_bytes, part, cancel) = (fetch.expected_bytes, fetch.part, fetch.cancel);
    presize_part(part, expected_bytes)?;
    let progress = AtomicU64::new(0);
    let failed = AtomicBool::new(false);
    let first_error = Mutex::new(None::<String>);
    let downloads = range_segments(expected_bytes, workers)
        .into_iter()
        .map(|(start, end)| {
            let range = Range {
                start,
                end,
                progress: &progress,
                failed: &failed,
            };
            let first_error = &first_error;
            async move {
                if let Err(error) = download_range(fetch, &range).await {
                    record_first_error(range.failed, first_error, error);
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

/// Create `part` at its full pinned size up front, so every range writer can
/// seek straight to its own offset.
fn presize_part(part: &Path, expected_bytes: u64) -> Result<(), String> {
    let file =
        File::create(part).map_err(|e| format!("could not create {}: {e}", part.display()))?;
    file.set_len(expected_bytes)
        .map_err(|e| format!("could not size {}: {e}", part.display()))
}

/// Keep only the first range's error: it is the cause, the others are the
/// ranges it stopped. Setting `failed` is also what stops them.
fn record_first_error(failed: &AtomicBool, first_error: &Mutex<Option<String>>, error: String) {
    if failed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        if let Ok(mut first) = first_error.lock() {
            *first = Some(error);
        }
    }
}

/// Reads one attempt's response body into `file`, chunk by chunk, advancing
/// `progress` and the write position as it goes. Returns the byte offset
/// reached, plus a retry reason when the stream ended early (a read failure
/// or a truncated response) so the caller's retry loop keeps that message to
/// report if every attempt runs out. `Err` only for the cases that should
/// abort the whole download outright: cancellation, a sibling range failing,
/// or the server sending more than was asked for.
async fn write_range_chunks(
    response: &mut reqwest::Response,
    file: &mut File,
    fetch: &Fetch<'_>,
    range: &Range<'_>,
    mut next: u64,
) -> Result<(u64, Option<String>), String> {
    let (start, end, cancel) = (range.start, range.end, fetch.cancel);
    let mut last_error = Some(format!("response ended before byte {end}"));
    while next <= end {
        check_aborted(cancel, range.failed)?;
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
        fetch.report(range.progress.fetch_add(n as u64, Ordering::AcqRel) + n as u64);
    }
    if next > end {
        last_error = None;
    }
    Ok((next, last_error))
}

async fn download_range(fetch: &Fetch<'_>, range: &Range<'_>) -> Result<(), String> {
    let (start, end) = (range.start, range.end);
    let mut file = open_part_at(fetch.part, start)?;
    let mut next = start;
    let mut last_error = None;
    for attempt in 1..=DOWNLOAD_RANGE_ATTEMPTS {
        check_aborted(fetch.cancel, range.failed)?;
        let (updated_next, retry_reason) = attempt_range(fetch, range, &mut file, next).await?;
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

/// One attempt at the rest of a range: request bytes `next..=end` and write
/// what arrives. Returns the offset reached and the retry reason, a rejected
/// request counting as a retry that got nowhere. Split out of
/// `download_range` so its retry loop does not also nest the per-attempt
/// branching.
async fn attempt_range(
    fetch: &Fetch<'_>,
    range: &Range<'_>,
    file: &mut File,
    next: u64,
) -> Result<(u64, Option<String>), String> {
    match request_range(fetch, next, range.end).await {
        Ok(mut response) => write_range_chunks(&mut response, file, fetch, range, next).await,
        Err(retry_reason) => Ok((next, Some(retry_reason))),
    }
}

/// Request bytes `next..=end` and accept the response only when it is
/// exactly that range of the pinned file. `Err` is the retry reason the
/// caller keeps in case every attempt fails.
async fn request_range(
    fetch: &Fetch<'_>,
    next: u64,
    end: u64,
) -> Result<reqwest::Response, String> {
    let response = send_with_cancel(
        fetch
            .client
            .get(fetch.url)
            .header(reqwest::header::RANGE, format!("bytes={next}-{end}")),
        fetch.cancel,
    )
    .await
    .map_err(|e| format!("request failed: {e}"))?;
    let remaining = end - next + 1;
    let expected_range = format!("bytes {next}-{end}/{}", fetch.expected_bytes);
    let actual_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok());
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
        || actual_range != Some(expected_range.as_str())
        || response.content_length() != Some(remaining)
    {
        return Err(format!(
            "server returned unexpected metadata ({})",
            response.status()
        ));
    }
    Ok(response)
}

/// The presized `part`, opened for writing at this range's own offset.
fn open_part_at(part: &Path, start: u64) -> Result<File, String> {
    let mut file = OpenOptions::new()
        .write(true)
        .open(part)
        .map_err(|e| format!("could not open {}: {e}", part.display()))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|e| format!("could not seek {}: {e}", part.display()))?;
    Ok(file)
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
    Ok(hex_digest(hasher))
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
