# Rebuilds the full-featured release exe and copies it to the project root as
# the owner's local test binary (root quickdictate.exe is gitignored via *.exe).
#
# The root copy picks up the root settings.json (real keys) automatically via
# Config::settings_path()'s exe-dir-first lookup, and writes its log /
# update-cache files next to itself — all gitignored.
#
# Usage:  pwsh -File scripts\refresh_test_exe.ps1 [-SkipBuild]
[CmdletBinding()]
param(
    # Skip the cargo build and just re-copy the last-built exe.
    [switch] $SkipBuild
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$built = Join-Path $projectRoot 'target\release\quickdictate.exe'
$dest  = Join-Path $projectRoot 'quickdictate.exe'

if (-not $SkipBuild) {
    Write-Host '[refresh] cargo build --release --features google'
    Push-Location $projectRoot
    try {
        cargo build --release --features google
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path $built)) { throw "no built exe at $built" }

# If the test exe is currently running, stop it so the copy doesn't fail on a
# locked file.
Get-Process -Name 'quickdictate' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $dest } |
    ForEach-Object {
        Write-Host "[refresh] stopping running test exe (pid $($_.Id))"
        Stop-Process -Id $_.Id -Force
        Start-Sleep -Milliseconds 300
    }

Copy-Item $built $dest -Force
$vi = (Get-Item $dest).VersionInfo
Write-Host ("[refresh] deployed {0}  (v{1}, {2:N1} MB)" -f $dest, $vi.FileVersion, ((Get-Item $dest).Length / 1MB))
