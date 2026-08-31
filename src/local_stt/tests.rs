//! Tests for the local speech-to-text packs.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use super::download::{
    download_client, download_parallel, download_verified, range_segments, verify_model_hash_once,
};
use super::install::{finish_operation, install, InstallPhase};
use super::native::{ModelLoadParams, NativeEngine, RunParams};
use super::postprocess::{
    cohere_chunk_ranges, collapse_pathological_repetitions, collapse_pathological_sentence_runs,
    COHERE_CLIP_MAX_SECONDS, COHERE_MIN_TAIL_SECONDS,
};
use super::worker::{idle_unload_due, IDLE_UNLOAD_AFTER};
use super::{
    expected_runtime_marker, is_installed, model, runtime_verified, ModelSpec, MODELS,
    RUNTIME_VERSION,
};

/// RED TEAM: the local-STT runtime arrives as a downloaded `.tar.gz` and is
/// unpacked to disk. `install_runtime` relies on `tar::Archive::unpack`
/// routing every entry through the traversal-safe `unpack_in`, and says so
/// in a comment. A comment is not a guarantee -- it is a claim about a
/// dependency that a version bump or a swap to another crate could quietly
/// invalidate, and the blast radius is arbitrary file write as the user.
///
/// So: build the hostile archive by hand and prove nothing escapes. The
/// three shapes below are the whole classic family -- a relative `..` walk,
/// an absolute path, and a Windows drive-qualified path (which a
/// Unix-oriented guard can miss, and this is a Windows-only app).
#[test]
fn archive_extraction_cannot_escape_its_directory() {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let root = test_path("tarsafe").with_extension("");
    let staging = root.join("staging");
    let outside = root.join("outside");
    fs::create_dir_all(&staging).unwrap();
    fs::create_dir_all(&outside).unwrap();

    let hostile_names = [
        "../outside/escaped-relative.txt",
        "../../outside/escaped-deeper.txt",
        "/outside/escaped-absolute.txt",
        "C:/Windows/Temp/quickdictate-escaped-drive.txt",
        r"..\outside\escaped-backslash.txt",
    ];

    // The name goes into the header's raw 100-byte field, NOT through
    // `append_data`. This is load-bearing, and getting it wrong is how this
    // test was first written: `append_data` VALIDATES the path and refuses
    // every name above, so the archive ended up containing only the benign
    // entry and the test passed while extracting nothing hostile at all. A
    // real attacker writes header bytes; so does this.
    fn hostile_header(name: &str, size: usize) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        let raw = &mut header.as_old_mut().name;
        let bytes = name.as_bytes();
        assert!(bytes.len() < raw.len(), "name too long for a tar header");
        raw[..bytes.len()].copy_from_slice(bytes);
        header.set_size(size as u64);
        header.set_mode(0o644);
        header.set_cksum();
        header
    }

    let payload = b"pwned";
    let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    for name in hostile_names {
        archive
            .append(&hostile_header(name, payload.len()), &payload[..])
            .unwrap();
    }
    // One legitimate entry, so "the guard rejected the whole archive" stays
    // distinguishable from "the guard filtered the bad entries".
    let benign = b"ok";
    archive
        .append(
            &hostile_header("transcribe-native/contract.json", benign.len()),
            &benign[..],
        )
        .unwrap();
    let gz = archive.into_inner().unwrap().finish().unwrap();

    // THE HONESTY CHECK: read the archive back and prove the hostile names
    // are really in it. Without this, anything that silently drops them
    // makes the test green and vacuous again.
    let mut present: Vec<String> = Vec::new();
    let mut verify = tar::Archive::new(GzDecoder::new(std::io::Cursor::new(gz.clone())));
    for entry in verify.entries().unwrap() {
        present.push(String::from_utf8_lossy(&entry.unwrap().path_bytes()).into_owned());
    }
    for name in hostile_names {
        assert!(
            present.iter().any(|p| p == name),
            "the archive does not actually contain the hostile entry {name:?}, so this test \
             proves nothing. Present: {present:?}"
        );
    }
    assert_eq!(present.len(), hostile_names.len() + 1);

    // Exactly the call `install_runtime` makes.
    let mut reader = tar::Archive::new(GzDecoder::new(std::io::Cursor::new(gz)));
    let _ = reader.unpack(&staging);

    let escaped: Vec<PathBuf> = fs::read_dir(&outside)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    assert!(
        escaped.is_empty(),
        "a tar entry escaped the staging directory: {escaped:?}"
    );
    assert!(
        !Path::new(r"C:\Windows\Temp\quickdictate-escaped-drive.txt").exists(),
        "a drive-qualified tar entry escaped to an absolute path"
    );

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_file(r"C:\Windows\Temp\quickdictate-escaped-drive.txt");
}

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

/// Serves one accepted connection: reads the request, answers with either
/// a full 200 or a ranged 206 slice of `data`, and paces the body out in
/// 16KiB chunks with an optional per-chunk delay for the slow-server tests.
fn serve_one_download_request(
    mut stream: TcpStream,
    data: Arc<Vec<u8>>,
    ranged: bool,
    chunk_delay: Duration,
) {
    let request = read_request(&mut stream);
    let (start, end, status) = if ranged {
        let (start, end) = requested_range(&request).expect("range request expected");
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
            let (stream, _) = listener.accept().unwrap();
            let data = Arc::clone(&data);
            handlers.push(std::thread::spawn(move || {
                serve_one_download_request(stream, data, ranged, chunk_delay);
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

    let comma_loop = "Useful start, and here, and here, and here, and here, and here, useful end.";
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
    let root = std::env::temp_dir().join(format!("quickdictate-local-e2e-{}", std::process::id()));
    let old = std::env::var_os("LOCALAPPDATA");
    std::env::set_var("LOCALAPPDATA", &root);

    let result = (|| {
        let spec = model("whisper-turbo-q5").unwrap();
        if !is_installed(spec.id) {
            install(spec, &AtomicBool::new(false))?;
        }
        let mut reader =
            hound::WavReader::open("tests/fixtures/speech_16k.wav").map_err(|e| e.to_string())?;
        assert_eq!(reader.spec().sample_rate, 16_000);
        assert_eq!(reader.spec().channels, 1);
        let pcm = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let cancel = Arc::new(AtomicBool::new(false));
        let mut engine = unsafe { NativeEngine::load()? };
        let transcript = unsafe { engine.run(spec.id, "en", &pcm, &cancel)? }.unwrap_or_default();
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
