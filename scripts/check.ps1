# Local CI — runs the EXACT gates .github/workflows/ci.yml runs, so you never
# wait on GitHub to find out a push is red. Run this before every commit.
#
#   pwsh -File scripts\check.ps1          # fmt + clippy + test + supply chain
#   pwsh -File scripts\check.ps1 -Full    # + release build + exe validation
#   pwsh -File scripts\check.ps1 -Fast    # fmt + clippy + test only (inner loop)
#
# It is also wired in as the git pre-push hook (scripts\install-hooks.ps1), so a
# push that would go red is stopped here instead. Bypass once with
# `git push --no-verify` if you genuinely need to.
#
# Exits non-zero on the first failing gate.
#
# KEEP THIS IN STEP WITH ci.yml. A gate that exists in only one of the two is a
# gate somebody discovers the slow way, after pushing.
[CmdletBinding()]
param(
    # Add the release build + exe validation (the `check` job's tail).
    [switch] $Full,
    # Skip the supply-chain gates (cargo-deny, cargo-machete) for a tight loop.
    [switch] $Fast
)

$ErrorActionPreference = 'Stop'
Push-Location (Split-Path -Parent $PSScriptRoot)
$fail = $false
$sw = [System.Diagnostics.Stopwatch]::StartNew()

function Step($name, [scriptblock] $cmd) {
    if ($script:fail) { return }
    Write-Host "`n=== $name ===" -ForegroundColor Cyan
    $stepTimer = [System.Diagnostics.Stopwatch]::StartNew()
    & $cmd
    $stepTimer.Stop()
    if ($LASTEXITCODE -ne 0) {
        Write-Host "FAILED: $name" -ForegroundColor Red
        $script:fail = $true
    } else {
        Write-Host ("  ok ({0:n1}s)" -f $stepTimer.Elapsed.TotalSeconds) -ForegroundColor DarkGray
    }
}

# A gate that needs a tool nobody installed must SAY SO, not quietly pass. A
# skipped check that reads like a passing check is worse than no check at all.
function RequireTool($exe, $installHint) {
    if (Get-Command $exe -ErrorAction SilentlyContinue) { return $true }
    Write-Host "`n=== $exe ===" -ForegroundColor Cyan
    Write-Host "  SKIPPED — $exe is not installed. CI still runs it." -ForegroundColor Yellow
    Write-Host "  Install with: $installHint" -ForegroundColor Yellow
    $script:skipped += $exe
    return $false
}

$skipped = @()

try {
    # Mirrors ci.yml -> `check` job, in the same order.
    Step 'fmt --check'  { cargo fmt --all --check }
    Step 'clippy'       { cargo clippy --locked --all-targets -- -D warnings }
    # Includes the mutation-fuzz suite in src/fuzz.rs (thousands of mutated and
    # truncated inputs through every parser that reads a network response) and
    # the archive-traversal red-team test in src/local_stt.rs.
    Step 'test'         { cargo test --locked }

    if (-not $Fast) {
        # Mirrors ci.yml -> `deny` job. Advisories, licenses, bans, sources.
        if (RequireTool 'cargo-deny' 'cargo install cargo-deny --locked') {
            Step 'cargo-deny' { cargo deny check }
        }
        # Mirrors ci.yml -> `unused-deps` job.
        if (RequireTool 'cargo-machete' 'cargo install cargo-machete --locked') {
            Step 'cargo-machete' { cargo machete }
        }

        # Mirrors ci.yml -> `msrv` job. Keep the version in step with
        # Cargo.toml's `rust-version` and with ci.yml; all three name it.
        $msrv = '1.93-x86_64-pc-windows-msvc'
        $installed = (rustup toolchain list) -match [regex]::Escape($msrv)
        if ($installed) {
            # A scratch target dir: the MSRV's artifacts are not interchangeable
            # with the pinned stable's, and sharing one makes every switch a
            # full rebuild for both.
            Step "MSRV ($msrv)" {
                $prev = $env:CARGO_TARGET_DIR
                $env:CARGO_TARGET_DIR = Join-Path $PWD 'target\msrv'
                cargo +$msrv check --locked --all-targets
                $env:CARGO_TARGET_DIR = $prev
            }
        } else {
            Write-Host "`n=== MSRV ($msrv) ===" -ForegroundColor Cyan
            Write-Host "  SKIPPED - the MSRV toolchain is not installed. CI still runs it." -ForegroundColor Yellow
            Write-Host "  Install with: rustup toolchain install $msrv --profile minimal" -ForegroundColor Yellow
            $script:skipped += "MSRV"
        }
    }

    if ($Full) {
        Step 'build --release' { cargo build --release --locked }
        Step 'release executable' {
            $version = (cargo metadata --no-deps --format-version 1 |
                ConvertFrom-Json).packages[0].version
            pwsh -File scripts\check_release_binary.ps1 `
                -ExePath target\release\quickdictate.exe `
                -ExpectedVersion $version
        }
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
if ($skipped.Count -gt 0) {
    Write-Host "`n[check] GREEN in $([int]$sw.Elapsed.TotalSeconds)s, but $($skipped -join ', ') did not run — CI will." -ForegroundColor Yellow
    exit 0
}
Write-Host "`n[check] ALL GREEN in $([int]$sw.Elapsed.TotalSeconds)s — safe to push." -ForegroundColor Green
