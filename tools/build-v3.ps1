<#
.SYNOPSIS
  Build the `x86-64-v3` (AVX2) artifact alongside the portable baseline one.

.DESCRIPTION
  Measured 1.066x faster than the default build, bit-exact, on both an
  x265-encoded clip and a JCT-VC conformance stream (12/15 z=2.32 and 14/15
  z=3.36 on `codec-bench.ps1`).

  Why a second artifact rather than a flag on the first: the SIMD kernels carry
  `#[target_feature(enable = "avx2")]` and are chosen at runtime, so they are
  already AVX2 wherever the CPU allows. Everything ELSE -- the entropy decoder,
  the syntax layer, the per-block bookkeeping, which is where roughly 87% of the
  time is -- compiles for the portable x86-64 baseline, i.e. SSE2, because the
  binary has to run anywhere. `-C target-cpu=x86-64-v3` lets that majority use
  VEX encoding, three-operand forms and sixteen ymm registers too.

  x86-64-v3 means AVX2 + BMI1/2 + FMA + LZCNT/MOVBE: Haswell (2013) and later,
  Excavator (2015) and later. It will SIGILL on anything older, which is exactly
  why it ships as a separate binary next to the portable one rather than
  replacing it.

.EXAMPLE
  ./build-v3.ps1                 # -> target-v3/release/rusty_h265.exe
  ./build-v3.ps1 -Native         # -C target-cpu=native, this machine only
#>
param(
    # Tune for THIS machine instead of the v3 baseline. Faster still on a newer
    # CPU, and not distributable.
    [switch]$Native,
    [string]$TargetDir = "target-v3"
)

$ErrorActionPreference = "Stop"
$cpu = if ($Native) { "native" } else { "x86-64-v3" }
$root = Split-Path -Parent $PSScriptRoot

Write-Host "building with -C target-cpu=$cpu -> $TargetDir" -ForegroundColor Cyan
$env:RUSTFLAGS = "-C target-cpu=$cpu"
try {
    & cargo build --release --manifest-path (Join-Path $root "Cargo.toml") --target-dir (Join-Path $root $TargetDir)
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}
finally {
    Remove-Item Env:\RUSTFLAGS -ErrorAction SilentlyContinue
}

$exe = Join-Path $root "$TargetDir/release/rusty_h265.exe"
if (-not (Test-Path $exe)) { $exe = Join-Path $root "$TargetDir/release/rusty_h265" }
Write-Host "`n  $exe" -ForegroundColor Green
Write-Host @"

  This binary requires $cpu. Verify it against the portable build before
  shipping -- the two must be bit-identical on every stream:

    ./target/release/rusty_h265 IN.hevc --pipe x | md5sum
    ./$TargetDir/release/rusty_h265 IN.hevc --pipe x | md5sum
"@ -ForegroundColor DarkGray
