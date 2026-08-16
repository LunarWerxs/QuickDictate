# Install the git hooks. Run once per clone:
#
#     pwsh -File scripts\install-hooks.ps1
#
# Right now that is a single pre-push hook that runs `scripts\check.ps1`, the
# local mirror of .github/workflows/ci.yml. The point is to fail in ~40 seconds
# on this machine instead of five minutes later on a runner, with a red X
# sitting on a public repo while you find out.
#
# Hooks live in .git/hooks, which git does NOT clone — that is why this script
# exists rather than a committed hook file. Bypass a hook once with
# `git push --no-verify`.
[CmdletBinding()]
param(
    # Overwrite an existing pre-push hook instead of refusing.
    [switch] $Force
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot

$gitDir = & git -C $repoRoot rev-parse --git-dir 2>$null
if ($LASTEXITCODE -ne 0) { throw "not a git repository: $repoRoot" }
if (-not [System.IO.Path]::IsPathRooted($gitDir)) {
    $gitDir = Join-Path $repoRoot $gitDir
}
$hooksDir = Join-Path $gitDir 'hooks'
New-Item -ItemType Directory -Force -Path $hooksDir | Out-Null
$hookPath = Join-Path $hooksDir 'pre-push'

if ((Test-Path $hookPath) -and -not $Force) {
    Write-Host "pre-push hook already exists at $hookPath" -ForegroundColor Yellow
    Write-Host "Re-run with -Force to replace it." -ForegroundColor Yellow
    exit 0
}

# git runs hooks through its bundled sh, so the hook is a shell script that
# shells out to pwsh. `-Fast` is deliberately NOT used: the supply-chain gates
# are the ones whose verdict can change without a code change, which makes them
# exactly the ones worth running before the code leaves this machine.
$hook = @'
#!/bin/sh
# Installed by scripts/install-hooks.ps1. Runs the local mirror of CI.
# Bypass once with: git push --no-verify
exec pwsh -NoProfile -File "$(git rev-parse --show-toplevel)/scripts/check.ps1"
'@

# LF endings and no BOM: git's sh will not run a CRLF script.
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($hookPath, ($hook -replace "`r`n", "`n"), $utf8NoBom)

Write-Host "Installed pre-push hook -> $hookPath" -ForegroundColor Green
Write-Host "It runs: pwsh -File scripts\check.ps1" -ForegroundColor DarkGray
Write-Host "Bypass once with: git push --no-verify" -ForegroundColor DarkGray
