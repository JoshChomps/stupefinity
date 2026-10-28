<#
.SYNOPSIS
    Measures Stupefinity memory usage (Private Bytes, Working Set, Peak) across operational states.

.DESCRIPTION
    Samples process performance counters to benchmark Stupefinity against its strict
    <= 10 MB Private Bytes memory budget.
    Supports monitoring an active instance or launching release binary for benchmarking.

.PARAMETER DurationSeconds
    Duration in seconds to sample memory statistics. Default is 5 seconds.

.PARAMETER Launch
    If specified, builds and launches `target/release/stupefinity.exe` automatically.
#>

param (
    [int]$DurationSeconds = 5,
    [switch]$Launch = $false
)

$ErrorActionPreference = "Stop"

$proc = Get-Process -Name "stupefinity" -ErrorAction SilentlyContinue | Select-Object -First 1

if (-not $proc -and $Launch) {
    Write-Host "[*] Building release binary..." -ForegroundColor Cyan
    cargo build --release --bin stupefinity
    if ($LASTEXITCODE -ne 0) {
        Write-Error "Cargo build failed with exit code $LASTEXITCODE"
        exit 1
    }

    $binPath = ".\target\release\stupefinity.exe"
    Write-Host "[*] Launching $binPath in background..." -ForegroundColor Cyan
    $proc = Start-Process -FilePath $binPath -PassThru
    Start-Sleep -Seconds 2
}

if (-not $proc) {
    Write-Host "[-] Stupefinity is not currently running." -ForegroundColor Yellow
    Write-Host "    Either start Stupefinity or run this script with -Launch to start a benchmark instance:"
    Write-Host "    powershell -ExecutionPolicy Bypass -File scripts\benchmark_memory.ps1 -Launch"
    exit 1
}

Write-Host "==========================================================" -ForegroundColor Green
Write-Host "  Stupefinity Memory Benchmark (PID: $($proc.Id))" -ForegroundColor Green
Write-Host "  Budget Target: <= 10.0 MB Private Bytes" -ForegroundColor Green
Write-Host "==========================================================" -ForegroundColor Green

$privateBytesSamples = [System.Collections.Generic.List[double]]::new()
$workingSetSamples = [System.Collections.Generic.List[double]]::new()

$startTime = [System.DateTime]::UtcNow
$endTime = $startTime.AddSeconds($DurationSeconds)

Write-Host "[*] Sampling process metrics for $DurationSeconds seconds..." -ForegroundColor Cyan

while ([System.DateTime]::UtcNow -lt $endTime) {
    $proc.Refresh()
    if ($proc.HasExited) {
        Write-Error "Process exited unexpectedly during benchmark!"
        exit 1
    }

    $pBytes = [math]::Round($proc.PrivateMemorySize64 / 1MB, 2)
    $wSet = [math]::Round($proc.WorkingSet64 / 1MB, 2)

    $privateBytesSamples.Add($pBytes)
    $workingSetSamples.Add($wSet)

    Start-Sleep -Milliseconds 250
}

$avgPrivate = [math]::Round(($privateBytesSamples | Measure-Object -Average).Average, 2)
$minPrivate = ($privateBytesSamples | Measure-Object -Minimum).Minimum
$maxPrivate = ($privateBytesSamples | Measure-Object -Maximum).Maximum

$avgWorking = [math]::Round(($workingSetSamples | Measure-Object -Average).Average, 2)
$minWorking = ($workingSetSamples | Measure-Object -Minimum).Minimum
$maxWorking = ($workingSetSamples | Measure-Object -Maximum).Maximum
$peakWorking = [math]::Round($proc.PeakWorkingSet64 / 1MB, 2)

Write-Host ""
Write-Host "--- Benchmark Results ---" -ForegroundColor White
Write-Host ("Private Bytes (Committed):  Avg: {0,5} MB  | Min: {1,5} MB  | Max: {2,5} MB" -f $avgPrivate, $minPrivate, $maxPrivate)
Write-Host ("Working Set (Physical RAM): Avg: {0,5} MB  | Min: {1,5} MB  | Max: {2,5} MB" -f $avgWorking, $minWorking, $maxWorking)
Write-Host ("Peak Working Set:           {0,5} MB" -f $peakWorking)
Write-Host ""

if ($maxPrivate -le 10.0) {
    Write-Host "[PASS] Memory target strictly met! Maximum Private Bytes ($maxPrivate MB) <= 10.0 MB." -ForegroundColor Green
} else {
    Write-Host "[FAIL] Memory budget exceeded! Maximum Private Bytes was $maxPrivate MB (> 10.0 MB)." -ForegroundColor Red
    exit 1
}
