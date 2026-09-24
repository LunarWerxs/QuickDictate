# Live test of "Other audio" ducking against the real Windows audio service.
#
#   pwsh -File scripts\duck_live_test.ps1 [-Exe target\release\quickdictate.exe]
#
# An isolated copy of the app (its own data folder via QUICKDICTATE_DATA_DIR, its
# own single-instance mutex via QUICKDICTATE_DEV_PORT, hotkeys off, a deliberately
# invalid key so each press ends at once) ducks a hidden PowerShell that loops a
# SILENT wav, so the test itself makes no sound. Two toggles over the dev UDP port,
# then quit; PASS needs a "muted N app(s)" line, a "restored N app(s)" line and no
# leftovers file on disk afterwards.
#
# Heads-up: it ducks EVERY app playing sound, so music you have on dips for a
# second or two while it runs. It never touches the running QuickDictate.
param([string]$Exe = (Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\quickdictate.exe'))
$ErrorActionPreference = 'Stop'
$port = 47611
$sandbox = Join-Path $env:TEMP 'qd-duck-live-test'
if (Test-Path $sandbox) { Remove-Item $sandbox -Recurse -Force }
New-Item -ItemType Directory -Path $sandbox | Out-Null

@{
  duck_other_audio = $true; duck_volume_percent = 0; hotkeys_enabled = $false
  enable_logging = $true; update_auto_check = $false; prewarm_keys = $false
  mouse_follower_enabled = $false; hide_tray_icon = $true; persist_history = $false
  enable_sound = $false
  # A deliberately invalid key: keeps the "add a key" toast off the screen, and
  # the provider refuses it in milliseconds, which ends each press at once.
  stt_provider = 'elevenlabs'; elevenlabs_keys = @('sk_invalid_duck_live_test')
} | ConvertTo-Json | Set-Content (Join-Path $sandbox 'settings.json')

# 3 s of 16 kHz mono 16-bit silence.
$wav = Join-Path $sandbox 'silence.wav'
$samples = 16000 * 3; $dataLen = $samples * 2
$ms = New-Object IO.MemoryStream; $bw = New-Object IO.BinaryWriter($ms)
$bw.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $bw.Write([int](36 + $dataLen))
$bw.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt ')); $bw.Write([int]16); $bw.Write([int16]1); $bw.Write([int16]1)
$bw.Write([int]16000); $bw.Write([int]32000); $bw.Write([int16]2); $bw.Write([int16]16)
$bw.Write([Text.Encoding]::ASCII.GetBytes('data')); $bw.Write([int]$dataLen); $bw.Write((New-Object byte[] $dataLen))
[IO.File]::WriteAllBytes($wav, $ms.ToArray())

$player = Start-Process powershell -WindowStyle Hidden -PassThru -ArgumentList '-NoProfile', '-Command',
  "`$p = New-Object System.Media.SoundPlayer '$wav'; `$p.PlayLooping(); Start-Sleep -Seconds 30"
Start-Sleep -Seconds 2

$env:QUICKDICTATE_DATA_DIR = $sandbox
$env:QUICKDICTATE_DEV_PORT = "$port"
$env:QUICKDICTATE_LOG = 'info'
$app = Start-Process -FilePath $Exe -PassThru -WindowStyle Hidden -WorkingDirectory $sandbox
$env:QUICKDICTATE_DATA_DIR = $null; $env:QUICKDICTATE_DEV_PORT = $null; $env:QUICKDICTATE_LOG = $null

$log = Join-Path $sandbox 'logs\quickdictate.log'
function LogText { if (Test-Path $log) { Get-Content $log -Raw } else { '' } }
$deadline = (Get-Date).AddSeconds(15)
while ((Get-Date) -lt $deadline -and (LogText) -notmatch 'QuickDictate ready') { Start-Sleep -Milliseconds 200 }
if ((LogText) -notmatch 'QuickDictate ready') { Write-Host 'FAIL: app never became ready'; Stop-Process -Id $app.Id -Force; Stop-Process -Id $player.Id -Force; exit 1 }

$udp = New-Object System.Net.Sockets.UdpClient
function Send-Cmd([string]$c) { $b = [Text.Encoding]::UTF8.GetBytes($c); [void]$udp.Send($b, $b.Length, '127.0.0.1', $port) }
Send-Cmd 'toggle'; Start-Sleep -Seconds 2
$midFile = Test-Path (Join-Path $sandbox 'quickdictate-ducked-apps.json')
Send-Cmd 'toggle'; Start-Sleep -Seconds 3
Send-Cmd 'quit'
if (-not $app.WaitForExit(15000)) { Stop-Process -Id $app.Id -Force; Write-Host 'WARN: app did not exit on quit' }
Stop-Process -Id $player.Id -Force -ErrorAction SilentlyContinue

$lines = (LogText) -split "`r?`n" | Where-Object { $_ -match 'duck:|Starting session|session\[\d+\] (starting|ended)' }
$lines | ForEach-Object { ($_ -replace '^\S+\s+', '') }
$left = Test-Path (Join-Path $sandbox 'quickdictate-ducked-apps.json')
$ducked = ($lines -match 'duck: muted [1-9]').Count
$restored = ($lines -match 'duck: restored [1-9]').Count
Write-Host "leftovers file after exit: $left"
if ($ducked -ge 1 -and $restored -ge 1 -and -not $left) { Write-Host 'PASS: muted the playing app and restored it; nothing left on disk' } else { Write-Host "FAIL: ducked=$ducked restored=$restored leftovers=$left" }
