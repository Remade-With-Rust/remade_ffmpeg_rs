<#
.SYNOPSIS
  Paired A/B benchmark of one of our codecs against a reference, with the
  measurement discipline ENFORCED rather than described.

.DESCRIPTION
  One harness, copied verbatim into each codec repo, so every public number we
  publish is produced the same way and can be reproduced by anyone.

  What it enforces, and why each rule exists:

    * PINNED to one core at High priority. Unpinned, the same binary on the same
      stream spread 2.02x; pinned, 1.06x. The minima agree either way -- it is
      the VARIANCE that destroys paired A/B, and paired A/B is what every
      keep/revert rests on.

    * A CLOCK WITH REAL RESOLUTION, checked against its own quantum.
      `TotalProcessorTime` is kernel TICK ACCOUNTING, not a clock: on Windows
      every reading it returns is a multiple of 15.625 ms. This harness used it
      as the primary quantity and published differences of "31 ms" that were
      TWO TICKS -- and the tie-exclusion below then discarded exactly the pairs
      that landed on the same tick, which is most of the close ones. On one
      stream the paired-ratio and per-arm-median estimators disagreed in SIGN
      because both were reading a 15.6 ms lattice.

      So: timing comes from `Stopwatch` (QueryPerformanceCounter, sub-us), or
      better from a self-reported internal time via `-OursMs`/`-RefMs`, which
      also excludes process launch. CPU time is still collected, but for the
      job it is actually good at -- `cpu/wall` per sample proves the process was
      neither descheduled (< 1) nor multi-threaded (> 1). And the harness now
      MEASURES ITS OWN QUANTUM and refuses to report a difference smaller than
      a few of them. `$p.Handle` must be touched before WaitForExit or
      TotalProcessorTime reads empty.

    * ABBA INTERLEAVED. Running all of A then all of B puts machine drift
      between the blocks: one quantity read 3.9% / 34.1% / 49.4% block-wise and
      a tight 16.0-20.2% interleaved.

    * PAIRED WIN RATE with a z-score, not a ratio of medians. |z| > 2 is a
      verdict however far the medians drifted.

    * WORK PARITY, checked, not assumed. **This is the one that matters most.**
      Both arms must decode the same number of frames. Without it this harness
      reported our decoder **46x faster than ffmpeg** on a stream it cannot
      decode at all: it exited 0 having produced `frames=0 errors=60`, took 6 ms,
      and the clock was not wrong -- it was measuring nothing. Divergent counts
      VOID a comparison; they do not weaken it.

    * NON-ZERO EXIT is fatal. A crashed arm is not a fast arm.

    * A NULL ARM, on request: the reference against itself. That is the
      resolution floor, and it must be reported next to any result. On this box
      it read 1.12x on best-of-11 -- wider than plenty of effects people quote.

  Nothing here is HEVC-specific. `-Fmt`, `-OursArgs` and `-RefArgs` carry the
  codec; the rest is the method.

.EXAMPLE
  ./codec-bench.ps1 -Ours target/release/rusty_h265.exe -Fmt hevc `
      -Streams bench/a.hevc,bench/b.hevc -Markdown
#>
param(
    [Parameter(Mandatory = $true)][string]$Ours,
    [string]$OursName = "ours",
    [string]$Reference = "ffmpeg",
    [string]$RefName = "",
    # argv templates, "|"-separated. {S} = stream path, {FMT} = container/codec.
    [string]$OursArgs = "{S}|-",
    # `-stats` is load-bearing, not cosmetic: at `-v error` ffmpeg prints no
    # progress line, so the work-parity check cannot read its frame count and
    # every comparison VOIDs. Progress goes to stderr and is CR-separated, so
    # the frame regex takes the LAST match, not the first.
    [string]$RefArgs = "-v|error|-stats|-threads|1|-f|{FMT}|-i|{S}|-f|null|-",
    # A comma-separated STRING, not string[]: invoked via `powershell -File`
    # every argument arrives as a literal, so an array parameter binds the whole
    # list as one element and the first "stream" is the entire list. Split here
    # so the harness behaves the same however it is launched.
    [Parameter(Mandatory = $true)][string]$Streams,
    [string]$Fmt = "hevc",
    [int]$Rounds = 15,
    [int]$Core = 4,
    [switch]$Markdown,
    [switch]$NullArm,
    # Regexes that pull the decoded-frame count out of each arm's output.
    [string]$OursFrames = "frames=(\d+)",
    [string]$RefFrames = "frame=\s*(\d+)",
    # Optional: a regex pulling a SELF-REPORTED internal duration in ms out of
    # each arm's output. Preferred over the wall clock when both arms have one
    # -- it excludes process launch entirely, so it neither dilutes the ratio
    # nor depends on how fast the OS starts a process today. Our decoders print
    # `decode_ms=`; ffmpeg has no equivalent, so a mixed comparison falls back
    # to the wall clock for BOTH arms rather than comparing unlike quantities.
    [string]$OursMs = "",
    [string]$RefMs = "",
    # Unit scale for the captured group, to ms. ffmpeg's `-benchmark` prints
    # `rtime=0.198s`, so the ffmpeg comparison needs `-RefMsScale 1000`. Without
    # this the published table cannot be reproduced from this file, which is
    # exactly what happened once: the parameter was added here, the mirror was
    # synced from an older copy, and the copy-back silently reverted it.
    [double]$OursMsScale = 1.0,
    [double]$RefMsScale = 1.0
)

$ErrorActionPreference = "Stop"
if ($RefName -eq "") { $RefName = Split-Path $Reference -Leaf }

function Resolve-Exe($p) {
    if (Test-Path $p) { return (Resolve-Path $p).Path }
    $c = Get-Command $p -ErrorAction SilentlyContinue
    if ($c) { return $c.Source }
    throw "cannot find executable: $p"
}
$StreamList = @($Streams.Split(",") | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne "" })
if ($StreamList.Count -eq 0) { throw "no streams given" }
$OursExe = Resolve-Exe $Ours
$RefExe = Resolve-Exe $Reference

# ---- provenance --------------------------------------------------------------
# Our binaries run under rusty_alloc in production, and an arm measured under the
# system allocator is not comparable to what ships -- measured 1.133x apart on
# this decoder. If a binary reports an allocator, it must be the shipping one.
#
# BOTH arms, not just ours. This guard existed for `-Ours` only, and the arm it
# did not check is exactly the one that went wrong: a release gate ran
# `cargo test --release`, which relinks the same path WITHOUT `bench-alloc`, so
# the 0.3.0 reference silently became a system-allocator build. Measured against
# a rusty_alloc `-Ours` it manufactured 1.61x-2.20x at 15/15, z = 3.87 -- a
# verdict-strength phantom, and the second time this exact trap has fired. An
# arm nobody checks is an arm that is wrong.
function Probe-Arm($exe, $label) {
    # A FOREIGN arm does not take our CLI's argument shape, and must not be able
    # to abort the run. Two things conspire: ffmpeg exits non-zero on
    # `ffmpeg <stream> -`, and on Windows PowerShell `2>&1` against a native
    # command wraps every stderr line in an ErrorRecord -- which under
    # `$ErrorActionPreference = "Stop"` killed the whole benchmark before one
    # measurement was taken. The guard's job is to catch one of OUR binaries
    # built without `bench-alloc`; a binary that does not answer in our CLI's
    # shape cannot be one of those, so it is skipped, not fatal. The teeth are
    # unchanged: any arm that DOES print `alloc=` must print `rusty`.
    $out = ""
    try {
        $out = (& $exe $StreamList[0] "-" 2>&1 | Out-String)
    } catch {
        $out = "$_"
    }
    if ($out -match "alloc=(\w+)" -and $Matches[1] -ne "rusty") {
        throw "$label ($exe) reports alloc=$($Matches[1]); rebuild it with --features bench-alloc. Both arms must run the shipping allocator."
    }
    return $out
}
$probe = Probe-Arm $OursExe "-Ours"
$null = Probe-Arm $RefExe "-Reference"   # silently skipped for ffmpeg: it prints no alloc= line
$isa = if ($probe -match "isa=(\w+)") { $Matches[1] } else { "?" }

function Run-Pinned($exe, $argv) {
    $so = Join-Path $env:TEMP "cb_out.txt"
    $se = Join-Path $env:TEMP "cb_err.txt"
    $p = Start-Process -FilePath $exe -ArgumentList $argv -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $so -RedirectStandardError $se
    $null = $p.Handle          # MUST precede WaitForExit or CPU time reads empty
    $p.ProcessorAffinity = [IntPtr](1 -shl $Core)
    $p.PriorityClass = 'High'
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $p.WaitForExit()
    $sw.Stop()
    return @{
        # QueryPerformanceCounter: sub-microsecond, unlike the 15.625 ms tick
        # lattice of TotalProcessorTime.
        ms   = $sw.Elapsed.TotalMilliseconds
        cpu  = $p.TotalProcessorTime.TotalMilliseconds
        code = $p.ExitCode
        text = ((Get-Content $so -Raw -EA SilentlyContinue) + (Get-Content $se -Raw -EA SilentlyContinue))
    }
}

# The smallest non-zero gap between distinct readings -- the clock's effective
# quantum. A difference of one or two of these is not a measurement.
function Quantum($vals) {
    $u = @($vals | Sort-Object -Unique)
    if ($u.Count -lt 2) { return 0 }
    $q = [double]::MaxValue
    for ($i = 1; $i -lt $u.Count; $i++) {
        $d = $u[$i] - $u[$i - 1]
        if ($d -gt 0 -and $d -lt $q) { $q = $d }
    }
    if ($q -eq [double]::MaxValue) { 0 } else { $q }
}

function Frames($text, $rx) {
    $m = [regex]::Matches($text, $rx)
    if ($m.Count -eq 0) { return -1 }
    return [int]$m[$m.Count - 1].Groups[1].Value
}

function Build($spec, $stream) { return @($spec.Replace("{S}", $stream).Replace("{FMT}", $Fmt).Split("|")) }

$rows = @()
foreach ($stream in $StreamList) {
    if (-not (Test-Path $stream)) { throw "stream not found: $stream" }

    # Expected frame count, from a third party so neither arm defines truth.
    $probe2 = & (Get-Command ffprobe).Source -v error -f $Fmt -count_frames `
        -show_entries stream=width,height,nb_read_frames -of csv=p=0 $stream
    $parts = $probe2.Trim().Split(",")
    $w = [int]$parts[0]; $h = [int]$parts[1]; $want = [int]$parts[2]
    $mpx = $w * $h * $want / 1e6

    $ta = New-Object System.Collections.Generic.List[double]
    $tb = New-Object System.Collections.Generic.List[double]
    $deschedIssue = 0; $threadIssue = 0
    $aArgs = Build $RefArgs $stream
    $bArgs = Build $OursArgs $stream
    $skip = $null

    for ($r = 0; $r -lt $Rounds; $r++) {
        if ($r % 2 -eq 0) { $ra = Run-Pinned $RefExe $aArgs; $rb = Run-Pinned $OursExe $bArgs }
        else { $rb = Run-Pinned $OursExe $bArgs; $ra = Run-Pinned $RefExe $aArgs }

        if ($ra.code -ne 0) { $skip = "reference exited $($ra.code)"; break }
        if ($rb.code -ne 0) { $skip = "$OursName exited $($rb.code)"; break }
        $fa = Frames $ra.text $RefFrames
        $fb = Frames $rb.text $OursFrames
        if ($fa -ne $want -or $fb -ne $want) {
            # The check that turns a phantom into a VOID.
            $skip = "work parity: expected $want frames, reference decoded $fa, $OursName decoded $fb"
            break
        }
        # Prefer each arm's own internal timer when BOTH report one.
        if ($OursMs -ne "" -and $RefMs -ne "") {
            $sa = [regex]::Match($ra.text, $RefMs)
            $sb = [regex]::Match($rb.text, $OursMs)
            if (-not $sa.Success -or -not $sb.Success) {
                $skip = "self-reported time requested but not found in output"
                break
            }
            $ta.Add([double]$sa.Groups[1].Value * $RefMsScale)
            $tb.Add([double]$sb.Groups[1].Value * $OursMsScale)
        }
        else {
            $ta.Add($ra.ms); $tb.Add($rb.ms)
        }
        # cpu/wall is what CPU time is actually good for: below 1 the process
        # was descheduled, above 1 it used more than one core. Either voids the
        # like-for-like comparison this harness claims to make.
        # NOT `$r` -- that is the rounds-loop counter, and rebinding it to a
        # hashtable makes the loop's own `$r++` fail.
        foreach ($smp in @($ra, $rb)) {
            if ($smp.ms -gt 0) {
                $cw = $smp.cpu / $smp.ms
                if ($cw -lt 0.80) { $deschedIssue++ }
                if ($cw -gt 1.20) { $threadIssue++ }
            }
        }
    }

    if ($skip) {
        Write-Host ("{0,-28} VOID -- {1}" -f (Split-Path $stream -Leaf), $skip) -ForegroundColor Yellow
        $rows += [pscustomobject]@{ stream = (Split-Path $stream -Leaf); mpx = $mpx; void = $skip }
        continue
    }

    # Paired ratios, ties at the timer quantum excluded.
    $ratios = @(); $wins = 0; $n = 0
    for ($i = 0; $i -lt $ta.Count; $i++) {
        if ($ta[$i] -eq $tb[$i]) { continue }
        $ratios += $ta[$i] / $tb[$i]
        if ($tb[$i] -lt $ta[$i]) { $wins++ }
        $n++
    }
    $sorted = $ratios | Sort-Object
    $median = $sorted[[int][math]::Floor($sorted.Count / 2)]
    $z = if ($n -gt 0) { ($wins - $n / 2) / (0.5 * [math]::Sqrt($n)) } else { 0 }
    $medA = ($ta | Sort-Object)[[int][math]::Floor($ta.Count / 2)]
    $medB = ($tb | Sort-Object)[[int][math]::Floor($tb.Count / 2)]

    # The harness must ENFORCE the discipline, not describe it: a claimed
    # difference of fewer than ~3 quanta is the clock talking, not the code.
    $q = [math]::Max((Quantum $ta), (Quantum $tb))
    $diff = [math]::Abs($medA - $medB)
    $note = $null
    if ($q -gt 0 -and $diff -lt 3 * $q) {
        $note = ("difference {0:N1} ms is under 3 clock quanta ({1:N3} ms)" -f $diff, $q)
    }
    if ($deschedIssue -gt 0) { $note = "$note; $deschedIssue sample(s) had cpu/wall < 0.80 (descheduled)" }
    if ($threadIssue -gt 0) { $note = "$note; $threadIssue sample(s) had cpu/wall > 1.20 (multi-threaded)" }
    $ties = $ta.Count - $n

    $rows += [pscustomobject]@{
        stream = (Split-Path $stream -Leaf); mpx = $mpx; frames = $want
        refMs = $medA; oursMs = $medB; ratio = $median; wins = $wins; n = $n; z = $z
        quantum = $q; ties = $ties; note = $note; void = $null
    }
    if ($note) { Write-Host ("  ! {0}" -f $note.TrimStart('; ')) -ForegroundColor Yellow }
    if ($ties -gt 0) { Write-Host ("  ! {0} of {1} pairs were exact ties and were EXCLUDED" -f $ties, $ta.Count) -ForegroundColor Yellow }
    Write-Host ("{0,-28} {1} {2,7:N0} ms   {3} {4,7:N0} ms   ratio {5,6:N3}  {6}/{7}  z={8,6:N2}" -f `
        (Split-Path $stream -Leaf), $RefName, $medA, $OursName, $medB, $median, $wins, $n, $z)
}

# ---- the resolution floor ----------------------------------------------------
$nullTxt = ""
if ($NullArm) {
    $s0 = $StreamList[0]
    $a = New-Object System.Collections.Generic.List[double]
    $b = New-Object System.Collections.Generic.List[double]
    $args0 = Build $RefArgs $s0
    for ($r = 0; $r -lt $Rounds; $r++) {
        if ($r % 2 -eq 0) { $a.Add((Run-Pinned $RefExe $args0).ms); $b.Add((Run-Pinned $RefExe $args0).ms) }
        else { $b.Add((Run-Pinned $RefExe $args0).ms); $a.Add((Run-Pinned $RefExe $args0).ms) }
    }
    $rr = @(); for ($i = 0; $i -lt $a.Count; $i++) { if ($a[$i] -ne $b[$i]) { $rr += $a[$i] / $b[$i] } }
    $sortedN = $rr | Sort-Object
    $nullRatio = if ($sortedN.Count) { $sortedN[[int][math]::Floor($sortedN.Count / 2)] } else { 1.0 }
    $nullTxt = "{0:N3}x" -f $nullRatio
    Write-Host ("null arm ({0} against itself): {1}" -f $RefName, $nullTxt)
}

# ---- the display -------------------------------------------------------------
# Built with -f rather than interpolation: Windows PowerShell 5.1 parses `|`
# inside a poorly-quoted string as a pipeline, and reads .ps1 as ANSI unless it
# has a BOM, so a stray non-ASCII character becomes mojibake in the published
# table. ASCII only, one format string per row.
if ($Markdown) {
    $bt = [char]96
    Write-Output ""
    Write-Output ("| stream | Mpx | {0} | {1} | ratio | paired verdict |" -f $RefName, $OursName)
    Write-Output "|---|---:|---:|---:|---:|---|"
    foreach ($r in $rows) {
        $name = "{0}{1}{0}" -f $bt, $r.stream
        if ($r.void) {
            Write-Output ("| {0} | {1:N1} | - | - | - | **VOID** - {2} |" -f $name, $r.mpx, $r.void)
        }
        else {
            $verdict = if ([math]::Abs($r.z) -gt 2) {
                "{0}/{1}, z = {2:N2}" -f $r.wins, $r.n, $r.z
            }
            else {
                "{0}/{1}, z = {2:N2} -- **not a verdict**" -f $r.wins, $r.n, $r.z
            }
            if ($r.note) { $verdict = "$verdict -- **{0}**" -f $r.note.TrimStart('; ') }
            Write-Output ("| {0} | {1:N1} | {2:N0} ms | {3:N0} ms | {4:N3}x | {5} |" -f `
                $name, $r.mpx, $r.refMs, $r.oursMs, $r.ratio, $verdict)
        }
    }
    Write-Output ""
    $clock = if ($OursMs -ne "" -and $RefMs -ne "") { "self-reported decode time" } else { "QPC wall clock" }
    $qmax = ($rows | Where-Object { $_.quantum } | ForEach-Object { $_.quantum } | Measure-Object -Maximum).Maximum
    Write-Output ("*Method: pinned core {0}, High priority, {1}, ABBA-interleaved, {2} pairs, paired win rate + z, work parity checked against ffprobe, cpu/wall checked per sample. Clock quantum {3:N3} ms. Both arms discard output. {4} built with the shipping allocator (isa={5}).*" -f $Core, $clock, $Rounds, $qmax, $OursName, $isa)
    if ($nullTxt -ne "") {
        Write-Output ("*Resolution floor -- {0} measured against itself: {1}{2}{1}.*" -f $RefName, $bt, $nullTxt)
    }
}
