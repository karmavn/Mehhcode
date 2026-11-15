# Baseline/verify performance measurement for the mehh-code Rust workspace.
# Usage: pwsh -File measure.ps1 -Label baseline
# Records: toolchain versions, release build time, binary size, test summary.
# Writes JSON to measurements/<label>.json and prints it.

param(
    [Parameter(Mandatory = $true)]
    [string]$Label
)

$ErrorActionPreference = "Continue"
$root = $PSScriptRoot
$outDir = Join-Path $root "measurements"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$result = [ordered]@{
    label       = $Label
    measured_at = (Get-Date).ToString("o")
    machine     = $env:COMPUTERNAME
    cpu_count   = [Environment]::ProcessorCount
    cargo       = (cargo --version)
    rustc       = (rustc --version)
}

$sw = [System.Diagnostics.Stopwatch]::StartNew()
& cargo build --release --offline --manifest-path (Join-Path $root "Cargo.toml") *>&1 | Out-Null
$sw.Stop()
$result.release_build_seconds = [math]::Round($sw.Elapsed.TotalSeconds, 1)

$bin = Join-Path $root "target\release\mehh.exe"
if (Test-Path $bin) {
    $result.release_binary_bytes = (Get-Item $bin).Length
} else {
    $result.release_binary_bytes = "not-built"
}

$testSw = [System.Diagnostics.Stopwatch]::StartNew()
$testOut = & cargo test --workspace --offline --manifest-path (Join-Path $root "Cargo.toml") -- --skip test_rpc_rejects_unknown_model_and_reports_provider_error 2>&1
$testSw.Stop()
$result.test_seconds = [math]::Round($testSw.Elapsed.TotalSeconds, 1)
$summary = $testOut | Select-String "test result:" | ForEach-Object { $_.Line }
$result.test_summary = $summary -join " | "

$json = $result | ConvertTo-Json -Depth 4
[System.IO.File]::WriteAllText((Join-Path $outDir "$Label.json"), $json, [System.Text.UTF8Encoding]::new($false))
Write-Output $json
