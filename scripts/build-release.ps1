<#
.SYNOPSIS
  Builds the release binaries and copies them into dist/.
.DESCRIPTION
  Runs `cargo build --release` at the repository root, copies pm.exe and
  partition-manager.exe from target/release to dist/, prints their sizes, the
  `pm.exe --version` output, and whether partition-manager.exe embeds the
  requireAdministrator manifest. dist/ is gitignored.
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    # cargo writes progress to stderr; Windows PowerShell 5.1 would treat that as an error
    $ErrorActionPreference = 'Continue'
    cargo build --release
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) { throw "cargo build --release failed (exit $code)" }

    $dist = Join-Path $root 'dist'
    New-Item -ItemType Directory -Force -Path $dist | Out-Null

    foreach ($name in 'pm.exe', 'partition-manager.exe') {
        $src = Join-Path $root "target\release\$name"
        if (-not (Test-Path $src)) { throw "missing build output: $src" }
        Copy-Item -Force $src (Join-Path $dist $name)
    }

    Write-Host ''
    Write-Host 'dist/:'
    Get-ChildItem $dist -File | ForEach-Object {
        '{0,-24} {1,12:N0} bytes ({2:N2} MiB)' -f $_.Name, $_.Length, ($_.Length / 1MB)
    }

    Write-Host ''
    & (Join-Path $dist 'pm.exe') --version

    # The GUI must ask for elevation. build.rs replaces MinGW's default asInvoker manifest with
    # ours, so the string must occur exactly once; two would mean a doubled manifest. The CLI
    # must not carry one at all.
    function Count-Marker([string]$Path, [string]$Marker) {
        $text = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($Path))
        return ([regex]::Matches($text, [regex]::Escape($Marker))).Count
    }
    $guiCount = Count-Marker (Join-Path $dist 'partition-manager.exe') 'requireAdministrator'
    $cliCount = Count-Marker (Join-Path $dist 'pm.exe') 'requireAdministrator'
    Write-Host ('requireAdministrator in partition-manager.exe: {0} (expected 1)' -f $guiCount)
    Write-Host ('requireAdministrator in pm.exe: {0} (expected 0)' -f $cliCount)
    if ($guiCount -ne 1) { throw "partition-manager.exe must embed exactly one requireAdministrator manifest (found $guiCount)" }
    if ($cliCount -ne 0) { throw "pm.exe must not embed an elevation manifest (found $cliCount)" }
}
finally {
    Pop-Location
}
