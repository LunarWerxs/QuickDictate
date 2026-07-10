# Headless settings-UI screenshot — no screen-control tooling needed.
#
# Launches the exe with QUICKDICTATE_UI_SHOT (the window screenshots ITSELF via
# egui's viewport-capture a few frames after opening and writes the PNG), opens
# the Settings window over the dev-trigger UDP channel, waits for the PNG, and
# kills the app. Use -Open keys|replacements to auto-open a modal first.
#
# Usage: pwsh -File scripts\ui_shot.ps1 [-Shot out.png] [-Open keys] [-UseDebugBuild]
[CmdletBinding()]
param(
    [string] $Shot = '',
    # keys-test also presses "Test all" and captures once the parallel
    # verdicts land — a headless end-to-end probe test with real keys.
    [ValidateSet('', 'keys', 'replacements', 'replacements-bulk', 'keys-test')]
    [string] $Open = '',
    # Optional --provider override for the launched exe.
    [string] $Provider = '',
    [switch] $UseDebugBuild,
    [int]    $DevPort = 7460
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $projectRoot ($(if ($UseDebugBuild) { 'target\debug\quickdictate.exe' } else { 'target\release\quickdictate.exe' }))
if (-not (Test-Path $exe)) { throw "exe not found: $exe" }
if ([string]::IsNullOrWhiteSpace($Shot)) {
    $Shot = Join-Path $projectRoot ("ui-shot" + $(if ($Open) { "-$Open" } else { "" }) + ".png")
}
Remove-Item $Shot -Force -ErrorAction SilentlyContinue

Get-Process quickdictate -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400

$env:QUICKDICTATE_DEV_PORT = "$DevPort"
$env:QUICKDICTATE_UI_SHOT = $Shot
$env:QUICKDICTATE_UI_OPEN = $Open
$procArgs = @{ FilePath = $exe; PassThru = $true; WorkingDirectory = $projectRoot }
if ($Provider) { $procArgs.ArgumentList = @('--provider', $Provider) }
$proc = Start-Process @procArgs
$env:QUICKDICTATE_DEV_PORT = $null
$env:QUICKDICTATE_UI_SHOT = $null
$env:QUICKDICTATE_UI_OPEN = $null

try {
    Start-Sleep -Seconds 3   # app boot
    $udp = New-Object System.Net.Sockets.UdpClient
    $bytes = [Text.Encoding]::ASCII.GetBytes('settings')
    $null = $udp.Send($bytes, $bytes.Length, '127.0.0.1', $DevPort)
    $udp.Close()

    # Wait for a NON-ZERO file (the app writes atomically via tmp+rename, so any
    # file we see is complete; the size check is belt-and-suspenders).
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $deadline) {
        if ((Test-Path $Shot) -and (Get-Item $Shot).Length -gt 0) { break }
        Start-Sleep -Milliseconds 250
    }
    $sz = if (Test-Path $Shot) { (Get-Item $Shot).Length } else { 0 }
    if ($sz -gt 0) {
        Write-Host "[ui-shot] wrote $Shot ($sz bytes)"
    } else {
        Write-Warning "[ui-shot] no screenshot appeared within 20s"
        exit 1
    }
}
finally {
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
}
