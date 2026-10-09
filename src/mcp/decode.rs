//! Decodes an audio file to the 16 kHz mono PCM every speech engine takes.
//!
//! Streams packet by packet: a 90-minute recording never exists as float
//! samples at the file's own rate, only as the 16 kHz i16 the engines need.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub const TARGET_RATE: u32 = 16_000;

pub const SUPPORTED_EXTENSIONS: [&str; 7] = ["wav", "mp3", "m4a", "mp4", "aac", "flac", "ogg"];

pub struct Decoded {
    pub pcm: Vec<i16>,
    pub source_rate: u32,
}

impl Decoded {
    pub fn duration_seconds(&self) -> f64 {
        self.pcm.len() as f64 / f64::from(TARGET_RATE)
    }
}

pub fn decode_file(path: &Path) -> Result<Decoded, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
        return Err(format!(
            "unsupported audio type '.{ext}': use one of {}",
            SUPPORTED_EXTENSIONS.join(", ")
        ));
    }
    let file = File::open(path).map_err(|e| format!("could not open the file: {e}"))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(&ext);
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("could not read the audio container: {e}"))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("the file has no decodable audio track")?;
    let track_id = track.id;
    let source_rate = track
        .codec_params
        .sample_rate
        .ok_or("the audio track does not state its sample rate")?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("no decoder for this audio codec: {e}"))?;

    let mut resampler = Resampler::new(source_rate);
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => {
                return Err(
                    "the audio stream changed format mid-file, which is not supported".into(),
                )
            }
            Err(e) => return Err(format!("the audio is damaged or unreadable: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("could not decode the audio: {e}")),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        for frame in buf.samples().chunks_exact(channels) {
            let mono = frame.iter().sum::<f32>() / channels as f32;
            resampler.push(mono);
        }
    }
    Ok(Decoded {
        pcm: resampler.finish(),
        source_rate,
    })
}

/// Box-averages each output sample's input span when downsampling and repeats
/// the last sample when upsampling. Streaming, so no float copy of the file.
struct Resampler {
    ratio: f64,
    out: Vec<i16>,
    bin_sum: f64,
    bin_count: u64,
    last: f64,
    seen: u64,
    next_boundary: f64,
}

impl Resampler {
    fn new(source_rate: u32) -> Self {
        let ratio = f64::from(source_rate) / f64::from(TARGET_RATE);
        Self {
            ratio,
            out: Vec::new(),
            bin_sum: 0.0,
            bin_count: 0,
            last: 0.0,
            seen: 0,
            next_boundary: ratio,
        }
    }

    fn push(&mut self, sample: f32) {
        let sample = f64::from(sample);
        self.bin_sum += sample;
        self.bin_count += 1;
        self.last = sample;
        self.seen += 1;
        while self.seen as f64 >= self.next_boundary {
            let value = if self.bin_count > 0 {
                self.bin_sum / self.bin_count as f64
            } else {
                self.last
            };
            self.out.push(to_i16(value));
            self.bin_sum = 0.0;
            self.bin_count = 0;
            self.next_boundary += self.ratio;
        }
    }

    fn finish(self) -> Vec<i16> {
        self.out
    }
}

fn to_i16(sample: f64) -> i16 {
    (sample.clamp(-1.0, 1.0) * f64::from(i16::MAX)).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resample(source_rate: u32, input: &[f32]) -> Vec<i16> {
        let mut r = Resampler::new(source_rate);
        for s in input {
            r.push(*s);
        }
        r.finish()
    }

    #[test]
    fn same_rate_passes_samples_through() {
        let out = resample(16_000, &[0.5, -0.5, 1.0]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2], i16::MAX);
    }

    #[test]
    fn downsampling_from_48k_yields_one_sample_per_three_inputs() {
        let input = vec![0.25_f32; 4_800];
        let out = resample(48_000, &input);
        assert_eq!(out.len(), 1_600);
        assert!((i32::from(out[10]) - i32::from(to_i16(0.25))).abs() <= 1);
    }

    #[test]
    fn unsupported_extension_is_rejected_before_opening() {
        let err = decode_file(Path::new(r"C:\no\such\file.opus"))
            .err()
            .unwrap_or_default();
        assert!(err.contains("unsupported audio type"), "{err}");
    }
}
