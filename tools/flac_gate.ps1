# FLAC ffmpeg-interop + size gate.
#
# For every content clip x level:
#   1. rff encode -> REFERENCE ffmpeg decode -> PCM identical to source
#      (lossless + their-decoder-reads-ours interop)
#   2. reference ffmpeg encode -> rff decode -> PCM identical
#      (our-decoder-reads-theirs interop)
#   3. size: ours vs ffmpeg at the same level (report; fail if ours > theirs
#      by more than the tolerance on any clip)
#
# Usage: tools\flac_gate.ps1 [-FfmpegBin <path>] [-RffBin <path>] [-TolerancePct 0.5]
param(
    [string]$FfmpegBin = 'C:\Users\talmo\AppData\Local\Microsoft\WinGet\Packages\Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe\ffmpeg-8.1.2-full_build\bin\ffmpeg.exe',
    [string]$RffBin = "$PSScriptRoot\..\target\release\rff.exe",
    [double]$TolerancePct = 0.5,
    [string]$WorkDir = "$env:TEMP\flac_gate"
)

# 'Continue', not 'Stop': the CLIs chat on stderr (rff prints "done — ..."),
# and under Stop PowerShell converts any native stderr line into a
# terminating NativeCommandError. Failures are detected via exit codes.
$ErrorActionPreference = 'Continue'
New-Item -ItemType Directory -Force $WorkDir | Out-Null
$failures = @()
$rows = @()

function Decode-Hash([string]$bin, [string]$inFile, [string]$fmt, [string]$outRaw) {
    & $bin -hide_banner -loglevel error -i $inFile -f $fmt -y $outRaw 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0) { return $null }
    (Get-FileHash $outRaw -Algorithm SHA256).Hash
}

# rff has no raw muxer: decode to WAV (s16 native, f32-exact for 24-bit),
# then normalize to raw via the reference ffmpeg (flt->s32 is exact for
# 24-bit-int-valued floats, matching s24->s32le).
function RffDecode-Hash([string]$rff, [string]$ffmpeg, [string]$inFile, [string]$fmt, [string]$work) {
    $tmpWav = Join-Path $work 'rffdec.wav'
    # 16-bit rides the native s16 path; wider depths go f32 (exact for 24-bit).
    $codec = if ($fmt -eq 's16le') { 'pcm_s16le' } else { 'pcm_f32le' }
    & $rff -hide_banner -loglevel error -i $inFile -c:a $codec -y $tmpWav 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0) { return $null }
    Decode-Hash $ffmpeg $tmpWav $fmt (Join-Path $work 'rffdec.raw')
}

# --- build the clip set -------------------------------------------------
$clips = @()
# Synthesized content classes (deterministic).
$synth = @(
    @('sine880',   'sine=frequency=880:duration=8',                   's16'),
    @('noise',     'anoisesrc=colour=white:seed=7:duration=8',        's16'),
    @('pinknoise', 'anoisesrc=colour=pink:seed=9:duration=8',         's24'),
    @('silence',   'anullsrc=r=44100:cl=mono,atrim=duration=8',       's16')
)
foreach ($s in $synth) {
    $name, $lavfi, $depth = $s
    $codec = if ($depth -eq 's24') { 'pcm_s24le' } else { 'pcm_s16le' }
    $wav = Join-Path $WorkDir "$name`_$depth.wav"
    if (-not (Test-Path $wav)) {
        & $FfmpegBin -hide_banner -loglevel error -f lavfi -i $lavfi -c:a $codec -y $wav | Out-Null
    }
    $clips += ,@("$name($depth)", $wav, $depth)
}
# Real corpus clips (float source -> both depths).
$corpus = Join-Path $PSScriptRoot '..\corpus'
foreach ($base in 'corp_st_mus_guitar', 'corp_st_mus_piano', 'corp_long_mus_vocal') {
    foreach ($depth in 's16', 's24') {
        $codec = if ($depth -eq 's24') { 'pcm_s24le' } else { 'pcm_s16le' }
        $wav = Join-Path $WorkDir "$base`_$depth.wav"
        if (-not (Test-Path $wav)) {
            & $FfmpegBin -hide_banner -loglevel error -i (Join-Path $corpus "$base.wav") -c:a $codec -y $wav | Out-Null
        }
        $clips += ,@("$base($depth)", $wav, $depth)
    }
}

# --- the gate matrix -----------------------------------------------------
foreach ($clip in $clips) {
    $name, $wav, $depth = $clip
    $rawFmt = if ($depth -eq 's24') { 's32le' } else { 's16le' }
    $srcHash = Decode-Hash $FfmpegBin $wav $rawFmt (Join-Path $WorkDir 'src.raw')
    foreach ($lvl in 5, 8) {
        $ourFlac = Join-Path $WorkDir 'ours.flac'
        $ffFlac = Join-Path $WorkDir 'ff.flac'
        & $RffBin -hide_banner -loglevel error -i $wav -c:a flac -compression_level $lvl -y $ourFlac | Out-Null
        if ($LASTEXITCODE -ne 0) { $failures += "$name L$lvl : rff encode failed"; continue }
        & $FfmpegBin -hide_banner -loglevel error -i $wav -compression_level $lvl -y $ffFlac | Out-Null

        # Gate 1: their decoder reads ours, losslessly.
        $h1 = Decode-Hash $FfmpegBin $ourFlac $rawFmt (Join-Path $WorkDir 'd1.raw')
        if ($h1 -ne $srcHash) { $failures += "$name L$lvl : ffmpeg-decodes-ours NOT lossless" }

        # Gate 2: our decoder reads theirs, losslessly.
        $h2 = RffDecode-Hash $RffBin $FfmpegBin $ffFlac $rawFmt $WorkDir
        if ($h2 -ne $srcHash) { $failures += "$name L$lvl : rff-decodes-theirs NOT lossless" }

        # Gate 3: size parity.
        $ourSz = (Get-Item $ourFlac).Length
        $ffSz = (Get-Item $ffFlac).Length
        $pct = ($ourSz - $ffSz) * 100.0 / $ffSz
        $rows += ('{0,-28} L{1}: ours {2,10:N0}  ffmpeg {3,10:N0}  delta {4,7:N3}%' -f $name, $lvl, $ourSz, $ffSz, $pct)
        if ($pct -gt $TolerancePct) { $failures += ("$name L$lvl : SIZE {0:N3}% over ffmpeg (tolerance $TolerancePct%)" -f $pct) }
    }
}

$rows | ForEach-Object { Write-Output $_ }
Write-Output ''
if ($failures.Count -eq 0) {
    Write-Output ('GATE PASS: {0} clip-level combos, all lossless both directions, sizes within {1}% of ffmpeg' -f ($clips.Count * 2), $TolerancePct)
    exit 0
} else {
    Write-Output ("GATE FAIL ({0}):" -f $failures.Count)
    $failures | ForEach-Object { Write-Output "  $_" }
    exit 1
}
