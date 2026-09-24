//! The dedicated capture thread: picking the input device, holding the cpal
//! stream open, and reopening it when a microphone disappears.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::*;

// ---------------------------------------------------------------------------
// Capture thread (runs on a dedicated std thread; owns the cpal Stream)
// ---------------------------------------------------------------------------

/// How often the capture thread polls the stop/health flags while streaming.
const HEALTH_POLL: std::time::Duration = std::time::Duration::from_millis(100);
/// Delay between attempts to reopen the input device after a stream failure.
const REOPEN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// How often the streaming loop re-checks which microphone it *should* be on.
/// Cheap (a name comparison against the resolved device) and the reason a
/// microphone that appears mid-run gets picked up at all — see
/// [`resolve_input`].
const DEVICE_RECHECK: std::time::Duration = std::time::Duration::from_secs(2);

/// The configured microphone preference. Empty means "follow the Windows
/// default input device". Swapped in at startup and whenever settings are
/// saved, so a change applies without a restart.
pub(super) static PREFERRED_INPUT: once_cell::sync::Lazy<arc_swap::ArcSwap<String>> =
    once_cell::sync::Lazy::new(|| arc_swap::ArcSwap::from_pointee(String::new()));

/// Publish the microphone preference from `settings.json`.
pub fn set_preferred_input(name: &str) {
    let name = name.trim();
    let previous = PREFERRED_INPUT.load();
    if previous.as_str() != name {
        PREFERRED_INPUT.store(Arc::new(name.to_string()));
        tracing::info!(
            "AudioSource: microphone preference set to {}",
            if name.is_empty() {
                "<system default>".to_string()
            } else {
                format!("'{name}'")
            }
        );
    }
}

/// Pick the input device to capture from, honouring the `input_device`
/// preference and falling back to the system default whenever the preferred
/// one is not present.
///
/// Matching is a plain case-insensitive substring of the device name, with no
/// knowledge of any particular vendor or transport. That is deliberate: a
/// remote-desktop tool can only hand this machine a microphone by publishing a
/// real audio input device, and once it does, its name is just a name like any
/// other. Special-casing one product's endpoint would buy nothing the
/// substring does not already cover, and would go stale.
///
/// The fallback is the important half: a device named here may be absent (a
/// USB mic unplugged, a virtual device that only exists while something is
/// connected), and an absent microphone must never be the reason dictation
/// stops working.
pub(super) fn resolve_input() -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    let host = cpal::default_host();
    let pref = PREFERRED_INPUT.load();
    let pref = pref.trim();

    let chosen = if pref.is_empty() {
        None
    } else {
        let needle = pref.to_ascii_lowercase();
        host.input_devices()
            .ok()
            .and_then(|mut devices| {
                devices.find(|d| {
                    d.description()
                        .is_ok_and(|desc| desc.name().to_ascii_lowercase().contains(&needle))
                })
            })
            .or_else(|| {
                tracing::debug!(
                    "AudioSource: preferred microphone '{pref}' not present; using the default"
                );
                None
            })
    };

    let device = match chosen {
        Some(d) => d,
        None => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?,
    };
    let supported = device
        .default_input_config()
        .map_err(|e| anyhow!("default_input_config: {e}"))?;
    Ok((device, supported))
}

/// The name [`resolve_input`] would pick right now, or `None` if nothing can
/// be opened. Used by the streaming loop to notice that the right microphone
/// has changed underneath it (an RDP session connecting or disconnecting, a
/// USB mic being plugged in, Windows promoting a new default).
fn resolved_input_name() -> Option<String> {
    resolve_input()
        .ok()
        .and_then(|(d, _)| d.description().ok().map(|desc| desc.name().to_string()))
}

/// The device's name for the log, or empty when Windows will not say.
fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|desc| desc.name().to_string())
        .unwrap_or_default()
}

/// Make a newly opened device the current one: publish its format to the
/// shared atomics (each session's resampler rebuilds from them on the next
/// callback) and log the switch, saying how it came about (`how`).
fn adopt_device(
    (device, supported): (cpal::Device, cpal::SupportedStreamConfig),
    device_rate: &AtomicU32,
    channels: &AtomicUsize,
    how: &str,
) -> (cpal::Device, cpal::SupportedStreamConfig) {
    device_rate.store(supported.sample_rate(), Ordering::Release);
    channels.store(supported.channels() as usize, Ordering::Release);
    tracing::info!(
        "AudioSource: {how} '{}' @ {} Hz, {} ch",
        device_name(&device),
        supported.sample_rate(),
        supported.channels(),
    );
    (device, supported)
}

/// Sleep for `total` in `HEALTH_POLL` steps so shutdown never has to sit
/// through a full delay to join this thread. False as soon as `stop` is set.
pub(super) fn sleep_unless_stopped(stop: &AtomicBool, total: std::time::Duration) -> bool {
    let mut waited = std::time::Duration::ZERO;
    while waited < total {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        std::thread::sleep(HEALTH_POLL);
        waited += HEALTH_POLL;
    }
    true
}

/// Poll for a working input device (the user may replug the mic or Windows
/// may promote another device) until one opens, or `None` once the app shuts
/// down.
fn wait_for_input(stop: &AtomicBool) -> Option<(cpal::Device, cpal::SupportedStreamConfig)> {
    loop {
        if !sleep_unless_stopped(stop, REOPEN_RETRY_DELAY) {
            return None;
        }
        match resolve_input() {
            Ok(found) => return Some(found),
            Err(e) => tracing::debug!("AudioSource reopen attempt failed: {e:#}"),
        }
    }
}

/// Why `stream_until_failure` returned without an error.
enum StreamOutcome {
    /// The app is shutting down.
    Shutdown,
    /// The microphone preference now resolves to a different device; the
    /// caller should reopen. Distinct from `Err` so a routine swap never
    /// looks like a fault.
    DeviceChanged,
}

/// Outer capture loop: stream from the device until shutdown, and on a stream
/// failure (device unplugged/disabled, `stream.play()` race) keep retrying to
/// reopen the (possibly different) default device instead of dying silently.
/// The `healthy` flag is `false` for the whole degraded window, so sessions
/// surface a visible error instead of recording nothing; it flips back to
/// `true` the moment a reopen succeeds.
pub(super) fn run_global_capture(
    sessions: Arc<parking_lot::RwLock<Vec<SessionEntry>>>,
    stop: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    device_rate: Arc<AtomicU32>,
    channels: Arc<AtomicUsize>,
    device: cpal::Device,
    supported: cpal::SupportedStreamConfig,
) {
    let mut current = (device, supported);
    loop {
        match stream_until_failure(
            &sessions,
            &stop,
            &healthy,
            &device_rate,
            &channels,
            &current.0,
            &current.1,
        ) {
            Ok(StreamOutcome::Shutdown) => return,
            // A different microphone should be in use now. Nothing has failed,
            // so reopen immediately and stay "healthy" throughout — a device
            // swap is not a degraded state and must not raise the error pip.
            Ok(StreamOutcome::DeviceChanged) => match resolve_input() {
                Ok(next) => {
                    current = adopt_device(next, &device_rate, &channels, "now on");
                    continue;
                }
                Err(e) => {
                    // The device vanished between deciding to switch and
                    // opening it. Fall through to the degraded retry loop,
                    // which re-resolves from scratch.
                    healthy.store(false, Ordering::Release);
                    tracing::warn!("AudioSource: switch target unavailable: {e:#}");
                }
            },
            Err(e) => {
                healthy.store(false, Ordering::Release);
                if stop.load(Ordering::Acquire) {
                    return;
                }
                tracing::error!("AudioSource capture failed: {e:#}; will retry the device");
            }
        }
        // Degraded: stay unhealthy until a device opens again.
        let Some(next) = wait_for_input(&stop) else {
            return;
        };
        current = adopt_device(next, &device_rate, &channels, "reopened");
    }
}

/// Build and run one capture stream. Returns `Ok(())` on a clean shutdown
/// (stop flag) or `Err` if the stream could not be built/played or its error
/// callback fired.
fn stream_until_failure(
    sessions: &Arc<parking_lot::RwLock<Vec<SessionEntry>>>,
    stop: &Arc<AtomicBool>,
    healthy: &Arc<AtomicBool>,
    device_rate: &Arc<AtomicU32>,
    channels: &Arc<AtomicUsize>,
    device: &cpal::Device,
    supported: &cpal::SupportedStreamConfig,
) -> Result<StreamOutcome> {
    let feed = FeedTargets {
        sessions: Arc::clone(sessions),
        device_rate: Arc::clone(device_rate),
        channels: Arc::clone(channels),
    };
    let stream = build_stream(device, supported, feed, stream_error_handler(healthy))?;

    stream.play().map_err(|e| anyhow!("stream.play: {e}"))?;
    healthy.store(true, Ordering::Release);
    tracing::info!("AudioSource: streaming");

    let open_name = device_name(device);
    watch_stream(stream, stop, healthy, &open_name)
}

/// A stream error (e.g. the mic is unplugged mid-session) arrives out-of-band:
/// cpal keeps the Stream object alive but stops delivering data, so without
/// this the app would keep "listening" while capturing nothing. Flip the
/// shared health flag so callers can detect it (and the outer loop can
/// rebuild the stream).
fn stream_error_handler(healthy: &Arc<AtomicBool>) -> impl FnMut(cpal::Error) + Send + 'static {
    let healthy = Arc::clone(healthy);
    move |e: cpal::Error| {
        if stream_survives(e.kind()) {
            tracing::warn!("audio: {e} (the stream keeps running)");
            return;
        }
        tracing::error!("audio stream error: {e}");
        healthy.store(false, Ordering::Release);
    }
}

/// What every capture callback feeds: the live sessions, and the device
/// format their resamplers are checked against.
struct FeedTargets {
    sessions: Arc<parking_lot::RwLock<Vec<SessionEntry>>>,
    device_rate: Arc<AtomicU32>,
    channels: Arc<AtomicUsize>,
}

impl FeedTargets {
    fn feed(&self, data: &[i16]) {
        feed_sessions(&self.sessions, &self.device_rate, &self.channels, data);
    }
}

/// Open the device's input stream in its native sample format, with every
/// callback fed to the sessions as i16.
fn build_stream(
    device: &cpal::Device,
    supported: &cpal::SupportedStreamConfig,
    feed: FeedTargets,
    err_fn: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream> {
    let mut config: cpal::StreamConfig = supported.config();
    config.buffer_size = cpal::BufferSize::Default;
    Ok(match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            build_converting_stream(device, config, feed, err_fn, f32_to_i16)?
        }
        cpal::SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _| {
                // WASAPI already gave us the exact representation the
                // resamplers consume, so avoid copying every callback into
                // an otherwise-identical scratch buffer.
                feed.feed(data);
            },
            err_fn,
            None,
        )?,
        cpal::SampleFormat::U16 => {
            build_converting_stream(device, config, feed, err_fn, u16_to_i16)?
        }
        other => return Err(anyhow!("unsupported sample format {other:?}")),
    })
}

/// An input stream whose native samples must become i16 before the feed.
/// The scratch buffer lives in the callback and keeps its capacity, so after
/// the first callback the conversion allocates nothing. Generic over
/// `convert` (not a fn pointer) so each format's loop still compiles to an
/// inlined per-sample conversion.
fn build_converting_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    feed: FeedTargets,
    err_fn: impl FnMut(cpal::Error) + Send + 'static,
    convert: impl Fn(T) -> i16 + Send + 'static,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + 'static,
{
    let mut scratch: Vec<i16> = Vec::new();
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            scratch.clear();
            scratch.reserve(data.len());
            for s in data {
                scratch.push(convert(*s));
            }
            feed.feed(&scratch);
        },
        err_fn,
        None,
    )?)
}

/// Stream errors that report something the stream already lived through,
/// rather than a stream that stopped delivering. cpal 0.18 began reporting
/// WASAPI's data-discontinuity flag as `Xrun` (0.15 dropped it silently), and
/// treated as a failure it tore the microphone down and reopened it: a
/// two-second hole in whatever was being said, several times an hour under
/// load (2026-09-18, the first build shipped with cpal 0.18).
pub(super) fn stream_survives(kind: cpal::ErrorKind) -> bool {
    matches!(
        kind,
        cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied
    )
}

/// Idle until shutdown or failure, watching the error callback and (every
/// `DEVICE_RECHECK`) whether the microphone that should be open has changed.
/// Split out of `stream_until_failure` so its own format-dispatch match
/// doesn't also carry this loop's nesting.
fn watch_stream(
    stream: cpal::Stream,
    stop: &Arc<AtomicBool>,
    healthy: &Arc<AtomicBool>,
    open_name: &str,
) -> Result<StreamOutcome> {
    let mut since_recheck = std::time::Duration::ZERO;
    while !stop.load(Ordering::Acquire) {
        if !healthy.load(Ordering::Acquire) {
            drop(stream);
            return Err(anyhow!("stream error callback fired"));
        }
        std::thread::sleep(HEALTH_POLL);

        // Notice when the microphone we *should* be on has changed. Without
        // this the stream is only ever rebuilt after it fails, so a device
        // that merely appears alongside a perfectly healthy one is never
        // switched to. The case that matters: connecting over Remote Desktop
        // publishes the client's microphone into the session, and the mic on
        // the desk keeps working, so nothing fails and dictation would go on
        // recording an empty room while you talk into your phone.
        since_recheck += HEALTH_POLL;
        if since_recheck >= DEVICE_RECHECK {
            since_recheck = std::time::Duration::ZERO;
            if let Some(want) = resolved_input_name() {
                if want != open_name {
                    tracing::info!(
                        "AudioSource: switching microphone '{open_name}' \u{2192} '{want}'"
                    );
                    drop(stream);
                    return Ok(StreamOutcome::DeviceChanged);
                }
            }
        }
    }

    drop(stream);
    tracing::info!("AudioSource: capture stopped");
    Ok(StreamOutcome::Shutdown)
}
