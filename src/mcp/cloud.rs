//! Batch (whole-file) REST transcription for the cloud providers. The app's own
//! providers stream over WebSocket for dictation; a file needs one request per
//! chunk, so these are separate, small clients.

use std::time::Duration;

use serde_json::Value;

use crate::http::USER_AGENT;

/// Ten minutes of 16 kHz mono 16-bit audio is about 19 MB: under OpenAI's
/// 25 MB upload limit with room for the multipart framing.
const CHUNK_SAMPLES: usize = 16_000 * 60 * 10;
const BOUNDARY: &str = "QuickDictateMcpBoundary7d3f9a2c";

pub fn transcribe(
    provider: &str,
    key: &str,
    pcm: &[i16],
    language: &str,
) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| format!("could not start the HTTP client: {e}"))?;
    let chunks: Vec<&[i16]> = pcm.chunks(CHUNK_SAMPLES).collect();
    let mut parts = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        if chunks.len() > 1 {
            eprintln!(
                "quickdictate mcp: {provider} chunk {} of {}",
                index + 1,
                chunks.len()
            );
        }
        let wav = wav_bytes(chunk);
        let text = match provider {
            "openai" => openai(&client, key, &wav, language),
            "elevenlabs" => elevenlabs(&client, key, &wav, language),
            "deepgram" => deepgram(&client, key, &wav, language),
            other => Err(format!("'{other}' has no file transcription path")),
        }?;
        let text = text.trim();
        if !text.is_empty() {
            parts.push(text.to_string());
        }
    }
    Ok(parts.join(" "))
}

fn openai(
    client: &reqwest::blocking::Client,
    key: &str,
    wav: &[u8],
    language: &str,
) -> Result<String, String> {
    let mut fields = vec![("model", "gpt-4o-transcribe"), ("response_format", "json")];
    if !language.is_empty() {
        fields.push(("language", language));
    }
    let (content_type, body) = multipart(&fields, wav);
    let response = client
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(key)
        .header("Content-Type", content_type)
        .body(body)
        .send()
        .map_err(|e| format!("openai request failed: {e}"))?;
    text_field(response, "openai", "text")
}

fn elevenlabs(
    client: &reqwest::blocking::Client,
    key: &str,
    wav: &[u8],
    language: &str,
) -> Result<String, String> {
    let mut fields = vec![("model_id", "scribe_v1")];
    if !language.is_empty() {
        fields.push(("language_code", language));
    }
    let (content_type, body) = multipart(&fields, wav);
    let response = client
        .post("https://api.elevenlabs.io/v1/speech-to-text")
        .header("xi-api-key", key)
        .header("Content-Type", content_type)
        .body(body)
        .send()
        .map_err(|e| format!("elevenlabs request failed: {e}"))?;
    text_field(response, "elevenlabs", "text")
}

fn deepgram(
    client: &reqwest::blocking::Client,
    key: &str,
    wav: &[u8],
    language: &str,
) -> Result<String, String> {
    let language = if language.is_empty() {
        "multi"
    } else {
        language
    };
    let url = url::Url::parse_with_params(
        "https://api.deepgram.com/v1/listen",
        &[
            ("model", "nova-3"),
            ("smart_format", "true"),
            ("punctuate", "true"),
            ("language", language),
        ],
    )
    .map_err(|e| format!("deepgram URL could not be built: {e}"))?;
    let response = client
        .post(url)
        .header("Authorization", format!("Token {key}"))
        .header("Content-Type", "audio/wav")
        .body(wav.to_vec())
        .send()
        .map_err(|e| format!("deepgram request failed: {e}"))?;
    let body = read_json(response, "deepgram")?;
    body.pointer("/results/channels/0/alternatives/0/transcript")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "deepgram answered without a transcript".to_string())
}

fn text_field(
    response: reqwest::blocking::Response,
    provider: &str,
    field: &str,
) -> Result<String, String> {
    let body = read_json(response, provider)?;
    body.get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{provider} answered without a transcript"))
}

fn read_json(response: reqwest::blocking::Response, provider: &str) -> Result<Value, String> {
    let status = response.status();
    let text = response
        .text()
        .map_err(|e| format!("{provider} response could not be read: {e}"))?;
    if !status.is_success() {
        let snippet: String = text.chars().take(200).collect();
        return Err(format!("{provider} returned HTTP {status}: {snippet}"));
    }
    serde_json::from_str(&text).map_err(|_| format!("{provider} sent a response that is not JSON"))
}

fn multipart(fields: &[(&str, &str)], wav: &[u8]) -> (String, Vec<u8>) {
    let mut body = Vec::with_capacity(wav.len() + 512);
    for (name, value) in fields {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n",
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

/// 16 kHz mono 16-bit PCM WAV, the format every batch provider accepts.
pub fn wav_bytes(pcm: &[i16]) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&16_000u32.to_le_bytes());
    out.extend_from_slice(&32_000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_describes_the_pcm_that_follows() {
        let wav = wav_bytes(&[1, -2, 3]);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]), 6);
        assert_eq!(wav.len(), 44 + 6);
    }

    #[test]
    fn multipart_carries_fields_and_file_with_closing_boundary() {
        let (content_type, body) = multipart(&[("model", "m")], b"RIFFxx");
        assert!(content_type.ends_with(BOUNDARY));
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"model\"\r\n\r\nm\r\n"));
        assert!(text.contains("name=\"file\"; filename=\"audio.wav\""));
        assert!(text.ends_with(&format!("--{BOUNDARY}--\r\n")));
    }
}
