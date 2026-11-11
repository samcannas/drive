# Builds the `drive` service and copies it to app/src-tauri/binaries/drive-<target-triple>.exe,
# where Tauri's `externalBin` expects it.
#
#   scripts/prepare-sidecar.ps1                 # release build for the host
#   scripts/prepare-sidecar.ps1 -Debug          # debug build (faster, for local development)
#   scripts/prepare-sidecar.ps1 -Target x86_64-pc-windows-msvc
param(
    [switch]$Debug,
    [string]$Target = ""
)
$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$triple = $Target
if (-not $triple) {
    $triple = (rustc -vV | Select-String "^host: ").ToString().Substring(6).Trim()
}
$profile = if ($Debug) { "debug" } else { "release" }

$cargoArgs = @("build", "-p", "drive-daemon", "--bin", "drive")
if (-not $Debug) { $cargoArgs += "--release" }
if ($Target) { $cargoArgs += @("--target", $Target) }
Push-Location $root
try {
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    $targetDir = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory
} finally {
    Pop-Location
}

$ext = if ($triple -like "*windows*") { ".exe" } else { "" }
$sub = if ($Target) { Join-Path $Target $profile } else { $profile }
$src = Join-Path (Join-Path $targetDir $sub) "drive$ext"
$destDir = Join-Path $root "app/src-tauri/binaries"
New-Item -ItemType Directory -Force $destDir | Out-Null
$dest = Join-Path $destDir "drive-$triple$ext"
Copy-Item $src $dest -Force
Write-Host "sidecar ready: $dest"
