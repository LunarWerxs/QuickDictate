# Regenerates the speech fixtures used by the live provider tests
# (src/stt/live_test.rs). Uses Windows SAPI TTS so the audio is deterministic
# and the transcript is known. Output: tests/fixtures/speech_{16,24}k.wav
# (16 kHz for streaming providers, 24 kHz for OpenAI Realtime). These are
# gitignored — run this once locally before `cargo test -- --ignored`.
$ErrorActionPreference = "Stop"
$dir = Join-Path $PSScriptRoot "..\tests\fixtures"
New-Item -ItemType Directory -Force $dir | Out-Null
Add-Type -AssemblyName System.Speech
# Distinctive content words so tests can assert on the transcript.
$phrase = "The quick brown fox jumps over the lazy dog. Testing one two three four five."
foreach ($rate in 16000, 24000) {
    $s = New-Object System.Speech.Synthesis.SpeechSynthesizer
    $s.Rate = -1  # slightly slower = cleaner recognition
    $fmt = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(
        $rate,
        [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen,
        [System.Speech.AudioFormat.AudioChannel]::Mono)
    $out = Join-Path $dir "speech_$($rate/1000)k.wav"
    $s.SetOutputToWaveFile($out, $fmt)
    $s.Speak($phrase)
    $s.Dispose()
    Write-Host "wrote $out"
}
