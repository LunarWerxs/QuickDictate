# End-to-end smoke test for QuickDictate.
#
# Drives the app over a UDP control channel (env var QUICKDICTATE_DEV_PORT)
# instead of synthesizing F14/F13 keystrokes, because Windows does not
# reliably route SendInput-injected keys to RegisterHotKey listeners across
# every input session.
#
# - Kills any existing quickdictate.exe.
# - Clears the log.
# - Launches the (release by default) exe with QUICKDICTATE_DEV_PORT set.
# - Waits for "QuickDictate ready".
# - Sends "toggle" -> waits -> "toggle" (simulates F14 toggle on / off).
# - Sends "hold_press" -> waits -> "hold_release" (simulates F13 hold).
# - Sends "quit", reads the log, asserts the pipeline markers.
#
# Usage:  pwsh -File scripts\smoke_test.ps1 [-UseDebugBuild] [-SessionMillis 4000]
[CmdletBinding()]
param(
    [switch] $UseDebugBuild,
    [int]    $SessionMillis = 4000,
    [int]    $DevPort = 7457,
    # Which STT provider settings.json currently selects. Used to build the
    # provider-specific log markers ("<provider> connected in", and the
    # session_started marker for providers that emit one).
    [string] $Provider = 'elevenlabs'
)

# Providers whose adapters surface an explicit session-started event in the log.
# (deepgram/google never emit one; dashscope's is consumed during connect.)
$ProvidersWithSessionStarted = @('elevenlabs', 'assemblyai', 'openai')

$ErrorActionPreference = 'Stop'

$projectRoot = Split-Path -Parent $PSScriptRoot
$exeName     = if ($UseDebugBuild) { 'target\debug\quickdictate.exe' } else { 'target\release\quickdictate.exe' }
$exePath     = Join-Path $projectRoot $exeName
$logPath     = Join-Path (Split-Path $exePath) 'quickdictate.log'

if (-not (Test-Path $exePath)) {
    throw "exe not found at $exePath - run 'cargo build [--release]' first"
}

Write-Host "[smoke] killing any existing quickdictate.exe..."
Get-Process -Name 'quickdictate' -ErrorAction SilentlyContinue | ForEach-Object {
    Stop-Process -Id $_.Id -Force
}
Start-Sleep -Milliseconds 300
if (Test-Path $logPath) { Remove-Item $logPath -Force }

function Send-Cmd([string]$cmd) {
    $udp = New-Object System.Net.Sockets.UdpClient
    try {
        $bytes = [System.Text.Encoding]::ASCII.GetBytes($cmd)
        $null  = $udp.Send($bytes, $bytes.Length, '127.0.0.1', $DevPort)
    } finally {
        $udp.Close()
    }
}

function Wait-LogContains([string]$needle, [int]$timeoutMs = 8000) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $logPath) {
            $content = Get-Content $logPath -Raw -ErrorAction SilentlyContinue
            if ($content -and $content.Contains($needle)) { return $true }
        }
        Start-Sleep -Milliseconds 100
    }
    return $false
}

Write-Host "[smoke] launching $exePath (DEV_PORT=$DevPort)"
$env:QUICKDICTATE_DEV_PORT = "$DevPort"
# Force file logging on for the smoke run regardless of settings.json.
$env:QUICKDICTATE_LOG       = 'info,quickdictate=debug'
$proc = Start-Process -FilePath $exePath -PassThru -WorkingDirectory $projectRoot
$env:QUICKDICTATE_DEV_PORT = $null
$env:QUICKDICTATE_LOG       = $null

$failures = @()
try {
    if (-not (Wait-LogContains 'QuickDictate ready')) {
        throw "Did not see 'QuickDictate ready' within 8s"
    }
    if (-not (Wait-LogContains "dev_trigger: listening on 127.0.0.1:$DevPort" 3000)) {
        throw "dev_trigger did not bind 127.0.0.1:$DevPort"
    }
    Write-Host "[smoke] app reports ready, dev_trigger up"

    # === Toggle pair ===
    Write-Host "[smoke] cmd: toggle (on)"
    Send-Cmd 'toggle'
    if (-not (Wait-LogContains 'TogglePressed' 3000)) {
        throw "UDP 'toggle' did not produce TogglePressed event"
    }
    if (-not (Wait-LogContains "$Provider connected in" 6000)) {
        $failures += 'provider connect timed out'
    }
    Start-Sleep -Milliseconds $SessionMillis
    Write-Host "[smoke] cmd: toggle (off)"
    Send-Cmd 'toggle'
    if (-not (Wait-LogContains 'Stopping session (toggle off)' 3000)) {
        throw "Toggle-off event missing"
    }
    if (-not (Wait-LogContains 'audio chunks sent =' 5000)) {
        throw "Session did not finalize (no chunk count)"
    }
    if (-not (Wait-LogContains 'session[1] ended' 4000)) {
        $failures += 'session[1] did not end cleanly'
    }

    # === Hold pair ===
    Start-Sleep -Milliseconds 500
    Write-Host "[smoke] cmd: hold_press"
    Send-Cmd 'hold_press'
    if (-not (Wait-LogContains 'HoldPressed' 3000)) {
        throw "hold_press did not produce HoldPressed"
    }
    Start-Sleep -Milliseconds 1500
    Write-Host "[smoke] cmd: hold_release"
    Send-Cmd 'hold_release'
    if (-not (Wait-LogContains 'HoldReleased' 3000)) {
        throw "hold_release did not produce HoldReleased"
    }
    if (-not (Wait-LogContains 'session[2] ended' 5000)) {
        $failures += 'session[2] did not end cleanly'
    }

    # === Rapid restart: stop, then start again before previous session has finalized.
    # This is the user-visible behavior we just added -- "blue icon hangs around
    # blocking new dictation" is what regresses if main waits for status=Idle
    # instead of using SttHandle.is_done().
    Start-Sleep -Milliseconds 200
    Write-Host "[smoke] rapid: toggle on"
    Send-Cmd 'toggle'
    if (-not (Wait-LogContains 'session[3] starting' 3000)) {
        $failures += 'rapid: session[3] did not start'
    }
    Start-Sleep -Milliseconds 600
    Write-Host "[smoke] rapid: toggle off"
    Send-Cmd 'toggle'
    # Within ~50 ms of the stop press, an immediate start should produce session[4].
    Start-Sleep -Milliseconds 50
    Write-Host "[smoke] rapid: toggle on AGAIN (immediate)"
    Send-Cmd 'toggle'
    if (-not (Wait-LogContains 'session[4] starting' 3000)) {
        $failures += 'rapid: session[4] did not start while session[3] still finalizing'
    }
    Start-Sleep -Milliseconds 400
    Send-Cmd 'toggle'   # stop session 4

    # === Paste path (bypasses STT/mic via dev_trigger fake) ===
    Start-Sleep -Milliseconds 500
    $sample = 'QuickDictate paste path test 12345'
    Write-Host "[smoke] cmd: fake transcript"
    Send-Cmd "fake:$sample"
    if (-not (Wait-LogContains 'pasting' 3000)) {
        $failures += 'paste path was not exercised'
    }
    if (-not (Wait-LogContains 'paste OK' 3000)) {
        $failures += 'paste did not complete OK'
    }

    Write-Host "[smoke] cmd: quit"
    Send-Cmd 'quit'
    Start-Sleep -Milliseconds 500
}
finally {
    if (-not $proc.HasExited) {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    }
}

Write-Host ''
Write-Host '========= LOG ========='
if (Test-Path $logPath) { Get-Content $logPath -Raw } else { Write-Warning 'no log file produced' }
Write-Host '========================'

if ($failures.Count -gt 0) {
    Write-Warning ("smoke test failures: " + ($failures -join '; '))
    exit 1
}

# Final, fail-fast structural check.
$log = if (Test-Path $logPath) { Get-Content $logPath -Raw } else { '' }
$required = @(
    'Loaded settings from',
    'Registered toggle hotkey f14',
    'Registered hold hotkey f13',
    "dev_trigger: listening on 127.0.0.1:$DevPort",
    'QuickDictate ready',
    'session[1] starting',
    "$Provider connected in",
    'Stopping session (toggle off)',
    'audio chunks sent =',
    'session[1] ended',
    'HoldPressed',
    'HoldReleased',
    'session[2] ended',
    'session[3] starting',
    'session[4] starting',
    'pasting',
    'paste OK'
)
if ($ProvidersWithSessionStarted -contains $Provider) {
    $required += "session[1] $Provider session_started"
}

# Note: hybrid paste mode (the new default) deliberately allows live pastes
# during the dynamic tail, so we no longer assert on the absence of a
# "committed (live ...)" log line.
$missing = @($required | Where-Object { -not $log.Contains($_) })
if ($missing.Count -gt 0) {
    Write-Warning ("Missing markers: " + ($missing -join ', '))
    exit 1
}
Write-Host "[smoke] PASS: all $($required.Count) markers present"
