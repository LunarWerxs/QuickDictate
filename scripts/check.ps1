# Local CI — runs the EXACT gates .github/workflows/ci.yml runs, so you never
# wait on GitHub to find out a push is red. Run this before every commit.
#
#   pwsh -File scripts\check.ps1          # fmt + clippy + test (fast, ~1 min warm)
#   pwsh -File scripts\check.ps1 -Full    # + release builds (matches CI exactly)
#
# Exits non-zero on the first failing gate.
[CmdletBinding()]
param([switch] $Full)

$ErrorActionPreference = 'Stop'
Push-Location (Split-Path -Parent $PSScriptRoot)
$fail = $false
$sw = [System.Diagnostics.Stopwatch]::StartNew()

function Step($name, [scriptblock] $cmd) {
    if ($script:fail) { return }
    Write-Host "`n=== $name ===" -ForegroundColor Cyan
    & $cmd
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAILED: $name" -ForegroundColor Red
        $script:fail = $true
    }
}

try {
    Step 'fmt --check'                 { cargo fmt --all --check }
    Step 'clippy (default)'            { cargo clippy --all-targets -- -D warnings }
    Step 'clippy (--features google)'  { cargo clippy --all-targets --features google -- -D warnings }
    Step 'test (--features google)'    { cargo test --features google }
    if ($Full) {
        Step 'build --release'                 { cargo build --release }
        Step 'build --release --features google' { cargo build --release --features google }
    }
}
finally {
    Pop-Location
}

$sw.Stop()
if ($fail) {
    Write-Host "`n[check] RED in $([int]$sw.Elapsed.TotalSeconds)s — fix before pushing." -ForegroundColor Red
    exit 1
}
Write-Host "`n[check] ALL GREEN in $([int]$sw.Elapsed.TotalSeconds)s — safe to push." -ForegroundColor Green
