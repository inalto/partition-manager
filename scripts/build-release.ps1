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

    # The GUI must ask for elevation: the manifest is stored as UTF-8 XML in the resources.
    $bytes = [System.IO.File]::ReadAllBytes((Join-Path $dist 'partition-manager.exe'))
    $text = [System.Text.Encoding]::ASCII.GetString($bytes)
    $elevated = $text.Contains('requireAdministrator')
    Write-Host ('partition-manager.exe manifest requireAdministrator: {0}' -f $elevated)
    if (-not $elevated) { throw 'partition-manager.exe does not embed the requireAdministrator manifest' }
}
finally {
    Pop-Location
}
