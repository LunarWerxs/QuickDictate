<#
  Builds the GitHub release body from one CHANGELOG.md version section.
  The section opens with its own **TL;DR** line and headline bullets, then the full list
  inside <details>. On the release page the TL;DR becomes a heading and the fold reads
  "Read more: everything in X.Y.Z". A section of more than two changes with no TL;DR is
  refused, so a release page never opens with the whole list.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$')]
    [string]$Version,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$OutPath,

    [string]$ChangelogPath = 'CHANGELOG.md'
)

$ErrorActionPreference = 'Stop'
$lines = @(Get-Content -LiteralPath $ChangelogPath)
$start = -1
for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match "^## \[$([regex]::Escape($Version))\](?:\s|$)") {
        $start = $i
        break
    }
}
if ($start -lt 0) { throw "CHANGELOG.md has no section for $Version" }
$end = $lines.Count
for ($i = $start + 1; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match '^## \[') { $end = $i; break }
}
$notes = @($lines | Select-Object -Skip ($start + 1) -First ($end - $start - 1))
if (-not (($notes -join "`n") -match '\S')) {
    throw "CHANGELOG.md section for $Version is empty"
}

$tldr = @($notes | Where-Object { $_ -ceq '**TL;DR**' }).Count
if ($tldr -eq 0) {
    $changes = @($notes | Where-Object { $_ -match '^- ' }).Count
    if ($changes -gt 2) {
        throw "CHANGELOG.md section for $Version lists $changes changes and has no **TL;DR**: open it with a **TL;DR** line, one bold headline per change that matters, then the full list inside <details><summary><b>Everything in $Version</b></summary> (see 1.5.0)"
    }
} else {
    $summaries = @($notes | Where-Object { $_ -match '^<details><summary>' }).Count
    $closes = @($notes | Where-Object { $_ -ceq '</details>' }).Count
    if ($tldr -ne 1 -or $summaries -ne 1 -or $closes -ne 1) {
        throw "CHANGELOG.md section for $Version needs exactly one **TL;DR** line, one <details><summary> line and one </details> line"
    }
    $notes = @($notes | ForEach-Object {
            if ($_ -ceq '**TL;DR**') { '## TL;DR' }
            elseif ($_ -match '^<details><summary>') { "<details><summary><b>Read more: everything in $Version</b></summary>" }
            else { $_ }
        })
}

$notes | Set-Content -LiteralPath $OutPath -Encoding utf8
