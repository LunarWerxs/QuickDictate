# Headless settings-UI screenshot — no screen-control tooling needed.
#
# Runs an ISOLATED copy of the exe from a scratch folder: its own
# settings.json (hotkeys off, no key probing, no update check, one demo key so
# the provider card looks configured), its own data folder, and its own
# single-instance mutex (QUICKDICTATE_DEV_PORT gives it one, see
# `single_instance_mutex_name` in src/startup.rs). Nothing it does touches a
# QuickDictate you are running: it never stops another process, and only its
# own PID is killed afterwards. The old version of this script stopped EVERY
# quickdictate process first, which took the owner's live dictation away
# mid-sentence.
#
# The window screenshots ITSELF via egui's viewport capture a few frames after
# opening (QUICKDICTATE_UI_SHOT); Settings is opened over the dev-trigger UDP
# channel. -Tab <prefix> picks a page; -Open keys|keys-bulk|keys-test|
# replacements|replacements-bulk|stats|nudge auto-opens a modal first.
#
# Usage: pwsh -File scripts\ui_shot.ps1 [-Shot out.png] [-Tab history] [-Open keys-bulk] [-UseDebugBuild]
[CmdletBinding()]
param(
    [string] $Shot = '',
    # keys-test also presses "Test all" and captures once the parallel
    # verdicts land — a headless end-to-end probe test; pair it with
    # -RealSettings, since the demo key is not a real one.
    # 'nudge' asks the real sign-in engine whether to show its banner, exactly as a save does.
    # It cannot conjure one: on a fresh profile the gate is shut and the shot honestly shows no
    # banner. Seed quickdictate-nudge.json in the scratch folder with a long-time user's history
    # to see it.
    [ValidateSet('', 'keys', 'keys-bulk', 'replacements', 'replacements-bulk', 'keys-test', 'stats', 'nudge')]
    [string] $Open = '',
    # Optional --provider override for the launched exe.
    [string] $Provider = '',
    # Which nav page to capture (prefix match on the rail label).
    [ValidateSet('', 'application', 'dictation', 'vocabulary', 'history', 'advanced')]
    [string] $Tab = '',
    [switch] $UseDebugBuild,
    [int]    $DevPort = 7460,
    # Seed the scratch copy from the project root's REAL settings.json (your
    # keys and preferences) instead of the demo settings. The safety overrides
    # below (no hotkeys, no probes, no logging) still apply.
    [switch] $RealSettings
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $projectRoot ($(if ($UseDebugBuild) { 'target\debug\quickdictate.exe' } else { 'target\release\quickdictate.exe' }))
if (-not (Test-Path $exe)) { throw "exe not found: $exe (cargo build --release first)" }
if ([string]::IsNullOrWhiteSpace($Shot)) {
    $Shot = Join-Path $projectRoot ("ui-shot" + $(if ($Tab) { "-$Tab" } else { "" }) + $(if ($Open) { "-$Open" } else { "" }) + ".png")
}
$Shot = [IO.Path]::GetFullPath($Shot)
Remove-Item $Shot -Force -ErrorAction SilentlyContinue

# The scratch folder IS the isolation: exe, settings, and every runtime file
# live here and nowhere else.
$scratch = Join-Path ([IO.Path]::GetTempPath()) "quickdictate-ui-shot-$DevPort"
if (Test-Path $scratch) { Remove-Item $scratch -Recurse -Force }
New-Item -ItemType Directory -Path $scratch | Out-Null
$scratchExe = Join-Path $scratch 'quickdictate.exe'
Copy-Item -LiteralPath $exe -Destination $scratchExe

$source = if ($RealSettings) { Join-Path $projectRoot 'settings.json' } else { Join-Path $projectRoot 'settings.example.json' }
if (-not (Test-Path $source)) { throw "settings source not found: $source" }
$cfg = Get-Content -LiteralPath $source -Raw | ConvertFrom-Json
function Set-Field($obj, [string] $name, $value) {
    $obj | Add-Member -NotePropertyName $name -NotePropertyValue $value -Force
}
# Never fight the QuickDictate you may be running: no global hotkeys (so no
# mouse hook either), no cursor pip, no key probing, no update check, no
# autostart, no log file, and every runtime file in the scratch folder.
Set-Field $cfg 'hotkeys_enabled' $false
Set-Field $cfg 'mouse_follower_enabled' $false
Set-Field $cfg 'prewarm_keys' $false
Set-Field $cfg 'update_auto_check' $false
Set-Field $cfg 'run_at_startup' $false
Set-Field $cfg 'enable_logging' $false
Set-Field $cfg 'enable_sound' $false
Set-Field $cfg 'share_usage_stats' $false
Set-Field $cfg 'data_dir' ''
if (-not $RealSettings) {
    # A configured-looking demo: one (fake) key, so the onboarding banner
    # stays away and the provider card reads as a real install would.
    Set-Field $cfg 'stt_provider' 'elevenlabs'
    Set-Field $cfg 'elevenlabs_keys' @('sk_demo_0000000000000000000000000000000000000000')
    Set-Field $cfg 'custom_vocabulary' @('Supabase', 'Cloudflare', 'QuickDictate')
    Set-Field $cfg 'text_replacements' ([pscustomobject]@{ 'Chat GPT' = 'ChatGPT'; 'Github' = 'GitHub' })
}
$cfg | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $scratch 'settings.json') -Encoding utf8

$env:QUICKDICTATE_DEV_PORT = "$DevPort"
$env:QUICKDICTATE_UI_SHOT = $Shot
$env:QUICKDICTATE_UI_OPEN = $Open
$env:QUICKDICTATE_UI_TAB = $Tab
$procArgs = @{ FilePath = $scratchExe; PassThru = $true; WorkingDirectory = $scratch }
if ($Provider) { $procArgs.ArgumentList = @('--provider', $Provider) }
$proc = Start-Process @procArgs
foreach ($name in 'QUICKDICTATE_DEV_PORT', 'QUICKDICTATE_UI_SHOT', 'QUICKDICTATE_UI_OPEN', 'QUICKDICTATE_UI_TAB') {
    Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
}

try {
    Start-Sleep -Seconds 3   # app boot
    $udp = New-Object System.Net.Sockets.UdpClient
    $bytes = [Text.Encoding]::ASCII.GetBytes('settings')
    $null = $udp.Send($bytes, $bytes.Length, '127.0.0.1', $DevPort)
    $udp.Close()

    # Wait for a NON-ZERO file (the app writes atomically via tmp+rename, so any
    # file we see is complete; the size check is belt-and-suspenders).
    $deadline = (Get-Date).AddSeconds(25)
    while ((Get-Date) -lt $deadline) {
        if ((Test-Path $Shot) -and (Get-Item $Shot).Length -gt 0) { break }
        if ($proc.HasExited) { break }
        Start-Sleep -Milliseconds 250
    }
    $sz = if (Test-Path $Shot) { (Get-Item $Shot).Length } else { 0 }
    if ($sz -gt 0) {
        Write-Host "[ui-shot] wrote $Shot ($sz bytes)"
    } else {
        if ($proc.HasExited) {
            Write-Warning "[ui-shot] the scratch instance exited (code $($proc.ExitCode)) before writing a screenshot"
        } else {
            Write-Warning "[ui-shot] no screenshot appeared within 25s"
        }
        exit 1
    }
}
finally {
    # Only ever our own process, never "every quickdictate".
    if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    $proc.WaitForExit(5000) | Out-Null
    Remove-Item $scratch -Recurse -Force -ErrorAction SilentlyContinue
}
