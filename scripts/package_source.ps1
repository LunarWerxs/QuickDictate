# Package the QuickDictate source as a zip for sharing.
# Excludes target/, git history, IDE files, and per-run logs.
#
# Usage:  pwsh -File scripts\package_source.ps1 [-Output path\to\quickdictate-src.zip]
[CmdletBinding()]
param(
    [string] $Output = ''
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$projectName = Split-Path -Leaf $projectRoot

if ([string]::IsNullOrWhiteSpace($Output)) {
    $stamp  = Get-Date -Format 'yyyyMMdd-HHmmss'
    $Output = Join-Path (Split-Path -Parent $projectRoot) "$projectName-src-$stamp.zip"
}

$temp = Join-Path $env:TEMP "qd-pkg-$(Get-Random)"
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    $staging = Join-Path $temp $projectName
    New-Item -ItemType Directory -Path $staging | Out-Null

    # Copy only what's needed.  Robocopy is fastest and handles excludes natively;
    # /MIR mirrors, /XD excludes directories, /XF excludes files.
    $excludeDirs  = @('target', '.git', '.saydeploy', '.vscode', '.idea', 'node_modules')
    $excludeFiles = @(
        'settings.json',   # a dev's real API keys live here — never ship it
        'quickdictate.log',
        'quickdictate-panic.log',
        'quickdictate-dev-port.txt',
        'quickdictate-update.txt',
        '.env',
        '*.env',           # broad: my.keys.env and any local env file with keys
        '*.pem',
        '*.key',
        '*.wav',           # generated test fixtures (scripts/gen_test_audio.ps1)
        '*.zip',
        '*.7z',
        '*.rar',
        '*.tar',
        '*.tar.gz',
        '*.exe',
        '*.rs.bk',
        '*.rs.b4'
    )

    $robocopyArgs = @(
        $projectRoot, $staging, '/E', '/NFL', '/NDL', '/NJH', '/NJS', '/NP', '/NS', '/NC'
    )
    $robocopyArgs += '/XD'
    $robocopyArgs += $excludeDirs
    $robocopyArgs += '/XF'
    $robocopyArgs += $excludeFiles

    & robocopy @robocopyArgs | Out-Null
    # Robocopy exits 0/1/2/3 on success ("files copied" / "extras"). Treat <8 as success.
    if ($LASTEXITCODE -ge 8) { throw "robocopy failed with exit code $LASTEXITCODE" }

    if (Test-Path $Output) { Remove-Item $Output -Force }
    Compress-Archive -Path $staging -DestinationPath $Output -CompressionLevel Optimal

    $size = (Get-Item $Output).Length
    Write-Host ("[package] wrote {0} ({1:N1} KB)" -f $Output, ($size / 1KB))
}
finally {
    Remove-Item $temp -Recurse -Force -ErrorAction SilentlyContinue
}
