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

    * CPU TIME, not elapsed. Affinity restricts you; it does not reserve the
      core. Elapsed time counts the time you spent descheduled, CPU time does
      not accrue off-core. Measured 5x tighter on a busy box. `$p.Handle` must
      be touched before WaitForExit or TotalProcessorTime reads empty.

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
    [string]$RefFrames = "frame=\s*(\d+)"
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
# this decoder. If the binary reports an allocator, it must be the shipping one.
$probe = (& $OursExe $StreamList[0] "-" 2>&1 | Out-String)
if ($probe -match "alloc=(\w+)") {
    if ($Matches[1] -ne "rusty") {
        throw "$OursExe reports alloc=$($Matches[1]); rebuild with the shipping allocator before publishing any number from it."
    }
}
$isa = if ($probe -match "isa=(\w+)") { $Matches[1] } else { "?" }

function Run-Pinned($exe, $argv) {
    $so = Join-Path $env:TEMP "cb_out.txt"
    $se = Join-Path $env:TEMP "cb_err.txt"
    $p = Start-Process -FilePath $exe -ArgumentList $argv -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $so -RedirectStandardError $se
    $null = $p.Handle          # MUST precede WaitForExit or CPU time reads empty
    $p.ProcessorAffinity = [IntPtr](1 -shl $Core)
    $p.PriorityClass = 'High'
    $p.WaitForExit()
    return @{
        ms   = $p.TotalProcessorTime.TotalMilliseconds
        code = $p.ExitCode
        text = ((Get-Content $so -Raw -EA SilentlyContinue) + (Get-Content $se -Raw -EA SilentlyContinue))
    }
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
        $ta.Add($ra.ms); $tb.Add($rb.ms)
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

    $rows += [pscustomobject]@{
        stream = (Split-Path $stream -Leaf); mpx = $mpx; frames = $want
        refMs = $medA; oursMs = $medB; ratio = $median; wins = $wins; n = $n; z = $z; void = $null
    }
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
            Write-Output ("| {0} | {1:N1} | {2:N0} ms | {3:N0} ms | {4:N3}x | {5} |" -f `
                $name, $r.mpx, $r.refMs, $r.oursMs, $r.ratio, $verdict)
        }
    }
    Write-Output ""
    Write-Output ("*Method: pinned core {0}, High priority, CPU time, ABBA-interleaved, {1} pairs, paired win rate + z, work parity checked against ffprobe. Both arms discard output. {2} built with the shipping allocator (isa={3}).*" -f $Core, $Rounds, $OursName, $isa)
    if ($nullTxt -ne "") {
        Write-Output ("*Resolution floor -- {0} measured against itself: {1}{2}{1}.*" -f $RefName, $bt, $nullTxt)
    }
}
