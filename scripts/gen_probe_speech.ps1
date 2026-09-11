# Generate the three TTS speech segments scripts/elevenlabs_probe.py streams
# (16 kHz, mono, 16-bit WAV), using the Windows speech synthesizer that is
# already on every machine. The phrases are the ones from the 2026-09-11
# "it stops listening" report, so a probe run reproduces that dictation's shape.
#
#   pwsh scripts/gen_probe_speech.ps1 -OutDir $env:TEMP/qd-probe
param([Parameter(Mandatory = $true)][string]$OutDir)

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
Add-Type -AssemblyName System.Speech
$fmt = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(
    16000,
    [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen,
    [System.Speech.AudioFormat.AudioChannel]::Mono)
$s = New-Object System.Speech.Synthesis.SpeechSynthesizer
$segments = [ordered]@{
    segA = "We recently did some work on this, and for some reason, occasionally, when I start"
    segB = "it stops listening. It's done it four times and I'm just trying to write this sentence."
    segC = "And this is the third part of the test, spoken after a long quiet pause."
}
foreach ($k in $segments.Keys) {
    $path = Join-Path $OutDir "$k.wav"
    $s.SetOutputToWaveFile($path, $fmt)
    $s.Speak($segments[$k])
    $s.SetOutputToNull()   # flushes the WAV header
    $len = (Get-Item $path).Length
    Write-Output "$k -> $path ($len bytes)"
}
$s.Dispose()
