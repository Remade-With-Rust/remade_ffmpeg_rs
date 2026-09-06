# HEVC decode north-star bench (docs/plans/rusty_hevc.md, §6 / H6.3 shape).
#
# Measures single-thread DECODE CPU TIME of several decoders on the same
# Annex-B stream, the codec-measurement way: each run pinned to one core at
# High priority, CPU time (not wall) read from the process, arms interleaved
# ABBA per round, best-of-N and median reported, plus a null arm (the
# reference against itself) so the table carries its own noise floor.
#
#   cargo build --release -p rusty_h265 --bin rusty_h265 --features bench-alloc
#   powershell -File tools/hevc/pinbench.ps1 -Stream hevc-vectors/bench/in_to_tree_720p_8bit.hevc -Rounds 7
#
# BUILD OURS WITH `--features bench-alloc`. CLAUDE.md: every performance
# measurement runs under rusty_alloc, because that is what `rff-cli` ships and
# "an arm measured under the system allocator is not comparable". Measured on
# this decoder the difference is 1.133x (13/15, z = 2.84) -- larger than most of
# the wins being measured.
#
# Arms (edit $Arms below): ffmpeg's native hevc decoder (-threads 1, output
# discarded), and the pure-Rust candidates through the _hevc_score binary
# (which writes the YUV; the write is the same fixed cost for every Rust arm
# and is reported separately by a null-write arm if you add one).
param(
    [string]$Stream = "hevc-vectors/bench/in_to_tree_720p_8bit.hevc",
    [int]$Rounds = 7,
    [int]$Core = 4,            # affinity mask bit; avoid core 0
    [string]$Score = "..\_hevc_score\target\release\_hevc_score.exe",
    [string]$Ours = "target\release\rusty_h265.exe"
)
$ErrorActionPreference = "Stop"

# ---- refuse to measure the wrong binary ------------------------------------
#
# CLAUDE.md requires every performance number to come from a rusty_alloc build,
# because that is what `rff-cli` ships and "an arm measured under the system
# allocator is not comparable". This whole HEVC campaign was measured under the
# system allocator before anyone checked, and a comment in this file did not
# prevent it. The binary now reports `alloc=` and `isa=`, and this refuses to
# run if either is wrong -- a law the tooling obeys, not one it merely states.
if (Test-Path $Ours) {
    $probe = & $Ours $Stream "-" 2>$null | Out-String
    if ($probe -match "alloc=(\w+)") {
        $alloc = $Matches[1]
        if ($alloc -ne "rusty") {
            throw ("$Ours reports alloc=$alloc. Rebuild with:`n" +
                   "  cargo build --release -p rusty_h265 --bin rusty_h265 --features bench-alloc`n" +
                   "Measuring under the system allocator is not comparable to what ships " +
                   "(measured 1.133x apart on this decoder).")
        }
    }
    else {
        throw "$Ours did not report alloc=; it predates the build-provenance field and cannot be trusted for timing."
    }
    if ($probe -match "isa=(\w+)") {
        Write-Host ("build check: {0} alloc=rusty isa={1}" -f (Split-Path $Ours -Leaf), $Matches[1])
    }
}
$ffmpeg = (Get-Command ffmpeg).Source
$ffprobe = (Get-Command ffprobe).Source
$info = & $ffprobe -v error -f hevc -count_frames -show_entries stream=width,height,nb_read_frames,pix_fmt -of csv=p=0 $Stream
$parts = $info.Trim().Split(",")
$w = [int]$parts[0]; $h = [int]$parts[1]; $pix = $parts[2]; $frames = [int]$parts[3]
$mpx = $w * $h * $frames / 1e6
$null_out = Join-Path $env:TEMP "hevc_pinbench_out.yuv"

$Arms = @(
    @{ name = "ffmpeg-hevc-1T (north star)"; exe = $ffmpeg; args = @("-v","error","-threads","1","-f","hevc","-i",$Stream,"-f","null","-") },
    @{ name = "ffmpeg-hevc-1T (null arm)";   exe = $ffmpeg; args = @("-v","error","-threads","1","-f","hevc","-i",$Stream,"-f","null","-") },
    # `-` discards, exactly as ffmpeg's `-f null -` does. Writing a real .yuv
    # here while the reference streamed to nothing would price OUR output path
    # into the comparison -- that mistake once put 38% of a measured "decode"
    # into the file write (codec-measurement §4).
    @{ name = "rusty_h265 (ours)";           exe = $Ours;   args = @($Stream,"-") },
    @{ name = "rust_h265 0.1.0";             exe = $Score;  args = @("rust_h265",$Stream,"-") },
    @{ name = "hpvcd 0.3.2 (1 thread)";      exe = $Score;  args = @("hpvcd",$Stream,"-","--threads","1") }
)

function Run-Pinned($exe, $argv) {
    $p = Start-Process -FilePath $exe -ArgumentList $argv -PassThru -WindowStyle Hidden -RedirectStandardError (Join-Path $env:TEMP "hevc_pinbench_err.txt")
    $null = $p.Handle            # MUST precede WaitForExit or TotalProcessorTime reads empty
    $p.ProcessorAffinity = [IntPtr]$Core
    $p.PriorityClass = 'High'
    $p.WaitForExit()
    if ($p.ExitCode -ne 0) { throw "exit $($p.ExitCode) from $exe $argv" }
    return $p.TotalProcessorTime.TotalMilliseconds
}

Write-Host "stream: $Stream  ${w}x${h} $pix  frames=$frames  ($([math]::Round($mpx,1)) Mpx)"
Write-Host "method: pinned core mask $Core, High priority, CPU time (TotalProcessorTime), ABBA-interleaved, $Rounds rounds, best + median"
$times = @{}
foreach ($a in $Arms) { $times[$a.name] = New-Object System.Collections.Generic.List[double] }
for ($r = 0; $r -lt $Rounds; $r++) {
    # ABBA: alternate the leading arm between rounds.
    $seq = if ($r % 2 -eq 0) { $Arms } else { $Arms[($Arms.Count-1)..0] }
    foreach ($a in $seq) {
        $ms = Run-Pinned $a.exe $a.args
        $times[$a.name].Add($ms)
    }
}
Write-Host ""
Write-Host ("{0,-30} {1,10} {2,10} {3,10} {4,10} {5,8}" -f "arm","best ms","median ms","fps(best)","Mpx/s","x ref")
$refBest = ($times[$Arms[0].name] | Measure-Object -Minimum).Minimum
foreach ($a in $Arms) {
    $s = $times[$a.name] | Sort-Object
    $best = $s[0]; $med = $s[[math]::Floor($s.Count/2)]
    $fps = $frames / ($best / 1000.0); $mpxs = $mpx / ($best / 1000.0)
    Write-Host ("{0,-30} {1,10:N0} {2,10:N0} {3,10:N1} {4,10:N0} {5,8:N2}" -f $a.name, $best, $med, $fps, $mpxs, ($best / $refBest))
}
Remove-Item $null_out -ErrorAction SilentlyContinue
