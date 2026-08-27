# Screenshot the sign-in banner, honestly.
#
# The banner is deliberately hard to see: it needs a week of ownership, several sessions, a real
# save, AND a signed-out app. Nothing a headless capture does on a fresh profile produces it, which
# is exactly why it needs a harness -- and exactly why that harness must not fake the banner into
# existence. This fakes only the CALENDAR. Everything else is the shipping build: the real engine
# decides, the real renderer draws, and if the gate is shut the capture honestly shows no banner.
#
# Two things bit hard enough to be worth stating:
#
#   * The app MIGRATES its data files into a newly-pointed data folder, credentials included. So
#     pointing QUICKDICTATE_DATA_DIR at an empty sandbox does NOT give you a signed-out app -- the
#     token follows. The credential has to be moved out of reach of both folders for the duration.
#   * Every run consumes an ask, and the daily gap then gates the next one, so the seed is
#     rewritten before each capture; without that, the second run silently shows nothing.
#
# Usage: pwsh -File scripts\nudge_shot.ps1 [-Shot out.png] [-Light] [-AskCount 3]
[CmdletBinding()]
param(
    [string] $Shot = '',
    # QuickDictate follows the Windows app theme; -Light captures the other one.
    [switch] $Light,
    # How many asks this user has ALREADY seen. The month-long dismissal only exists from the
    # FOURTH ask on, so -AskCount 3 is the only way to capture the three-button banner; the
    # default 0 captures the two-button one. Without this the layout that most people will
    # eventually see is not capturable at all.
    [int]    $AskCount = 0
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($Shot)) {
    $Shot = Join-Path $projectRoot ("nudge-shot" + $(if ($Light) { "-light" } else { "-dark" }) + ".png")
}

$sandbox = Join-Path $env:TEMP "qd-nudge-shot"
$aside   = Join-Path $env:TEMP "qd-creds-aside.dat"
New-Item -ItemType Directory -Force $sandbox | Out-Null

# A long-time user's history: installed a month ago, several sessions, and $AskCount asks already
# behind them. `last_ask_at` stays null so the next ask is never gated on the daily gap; the count
# alone decides whether the month-long dismissal is on the card.
[int64]$now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
[int64]$installed = $now - (30 * 86400000)
Set-Content -Path (Join-Path $sandbox "quickdictate-nudge.json") -Encoding utf8 -Value @"
{
  "v": 1, "installed_at": $installed, "session_count": 6, "last_ask_at": null,
  "ask_count": $AskCount, "consecutive_declines": 0, "cadence": "default",
  "stopped": null, "pending_ask": null, "converted": []
}
"@

foreach ($p in @((Join-Path $sandbox "quickdictate-connections.dat"), (Join-Path $projectRoot "quickdictate-connections.dat"))) {
    if (Test-Path $p) { Move-Item $p $aside -Force }
}

$env:QUICKDICTATE_DATA_DIR = $sandbox
if ($Light) { $env:QUICKDICTATE_THEME = 'light' }
try {
    & (Join-Path $PSScriptRoot 'ui_shot.ps1') -Open nudge -Shot $Shot
    $state = Get-Content (Join-Path $sandbox "quickdictate-nudge.json") -Raw | ConvertFrom-Json
    if ($state.ask_count -le $AskCount) {
        # A PNG with no banner in it is worse than no PNG: it looks like a successful check of
        # something that was never on screen. Say so and fail.
        Write-Warning "[nudge-shot] the engine did not ask -- the capture shows NO banner. Gate still shut, or the app was signed in."
        exit 1
    }
    $buttons = if ($state.ask_count -gt 3) { '3 buttons' } else { '2 buttons' }
    Write-Host "[nudge-shot] ask #$($state.ask_count) ($buttons) rendered -> $Shot"
}
finally {
    $env:QUICKDICTATE_DATA_DIR = $null
    $env:QUICKDICTATE_THEME = $null
    if (Test-Path $aside) { Move-Item $aside (Join-Path $projectRoot "quickdictate-connections.dat") -Force }
}
