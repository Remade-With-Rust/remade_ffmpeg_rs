#!/usr/bin/env python3
"""rusty_aac vs ffmpeg's native AAC — side-by-side quality and speed, per content type.

Pieces 2, 3 and 4 of the comparison harness (piece 1 is `rusty_aac::lab::wav`):

  2. the ffmpeg arm      — encode the SAME clip with both encoders
  3. the PEAQ bridge     — score both with an EXTERNAL oracle, not our own metric
  4. verification        — null arms, matched-rate check, sign agreement

Discipline (codec-measurement, codec-tune-quality):

  * BOTH candidates are decoded with the SAME neutral decoder (system ffmpeg), so
    the comparison isolates the encoders rather than the decoders.
  * The verdict metric is PEAQ ODG. Our own NMR is a self-metric and provably
    flatters the encoder it came from; it is not used for ranking here.
  * Speed runs BOTH encoders as subprocesses with identical invocation shape, so
    process-launch overhead is paid by both arms (codec-measurement §5), on long
    clips where that overhead is a small fraction of the run.
  * A null arm runs first. If PEAQ(ref, ref) is not ~0, or the two encoders land
    at materially different bitrates, the comparison is void and we say so.

Usage:
    python tools/quality/aac_vs_ffmpeg.py --workdir <dir> [--bitrates 64,96,128,192]
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]

# ABSOLUTE path to the SYSTEM ffmpeg. This repo builds its own `ffmpeg.exe`
# (a drop-in CLI), so resolving "ffmpeg" from PATH is a coin flip that could
# silently benchmark our encoder against itself and report parity.
SYS_FFMPEG = os.environ.get("SYS_FFMPEG") or shutil.which("ffmpeg")

# OUR CLI. Deliberately `rff.exe`, not our `ffmpeg.exe`: same binary content,
# unambiguous name, and it was the target verified fresh.
OURS = REPO / "target" / "release" / ("rff.exe" if os.name == "nt" else "rff")

PEAQ_RUN = REPO / "tools" / "quality" / "peaq_run.py"
PEAQ_DIR = REPO / "tools" / "quality" / "PEAQ_python"


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def probe_duration(path):
    r = run([SYS_FFMPEG, "-hide_banner", "-i", str(path)])
    m = re.search(r"Duration: (\d+):(\d+):(\d+\.\d+)", r.stderr)
    if not m:
        return None
    h, mn, s = int(m.group(1)), int(m.group(2)), float(m.group(3))
    return h * 3600 + mn * 60 + s


ARMS = []          # set from --arm; empty = shipped defaults
AACENC = REPO / "target" / "release" / "examples" / (
    "aacenc.exe" if os.name == "nt" else "aacenc")


def encode_ours(src, dst, kbps):
    """Our encoder.

    With no arms selected this goes through the shipped CLI, which is what users
    actually run. With arms selected it goes through the `aacenc` example, which
    is the only way to switch the experimental flags. Both drive the identical
    encoder core; the example emits ADTS rather than MP4, which the neutral
    decoder reads just the same.
    """
    if ARMS:
        d = str(dst).replace(".m4a", ".aac")
        return run([str(AACENC), str(src), d, str(kbps * 1000), *ARMS])
    return run([str(OURS), "-i", str(src), "-c:a", "aac", "-b:a", f"{kbps}000",
                "-y", str(dst)])


def encode_ffmpeg(src, dst, kbps):
    """ffmpeg's NATIVE aac encoder (not libfdk, not MediaFoundation)."""
    return run([SYS_FFMPEG, "-hide_banner", "-loglevel", "error", "-y",
                "-i", str(src), "-c:a", "aac", "-b:a", f"{kbps}k", str(dst)])


def decode_neutral(src, dst, sample_rate):
    """Decode with the SAME neutral decoder for both arms: system ffmpeg."""
    return run([SYS_FFMPEG, "-hide_banner", "-loglevel", "error", "-y",
                "-i", str(src), "-ac", "1", "-ar", str(sample_rate),
                "-c:a", "pcm_s16le", str(dst)])


def peaq(ref, test):
    """External oracle. Returns ODG in [-4, 0] (0 = imperceptible) or None."""
    r = run([sys.executable, str(PEAQ_RUN), str(ref), str(test), str(PEAQ_DIR)])
    m = re.search(r"ODG=\s*(-?\d+\.?\d*)", r.stdout)
    return float(m.group(1)) if m else None


def measured_kbps(path, duration):
    if not duration or duration <= 0:
        return None
    return os.path.getsize(path) * 8 / duration / 1000.0


def best_of(fn, reps):
    best = None
    for _ in range(reps):
        t = time.perf_counter()
        fn()
        el = time.perf_counter() - t
        best = el if best is None else min(best, el)
    return best


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--workdir", default=str(REPO / "target" / "aacvsff"))
    ap.add_argument("--bitrates", default="64,96,128,192")
    ap.add_argument("--speed-reps", type=int, default=5)
    ap.add_argument("--skip-speed", action="store_true")
    ap.add_argument("--arm", default="", help="space/comma separated arm flags")
    args = ap.parse_args()

    if not SYS_FFMPEG or not Path(SYS_FFMPEG).exists():
        sys.exit("no system ffmpeg found; set SYS_FFMPEG")
    if not OURS.exists():
        sys.exit(f"our CLI not built: {OURS} (cargo build --release -p rff-cli)")

    global ARMS
    ARMS = [x for x in args.arm.replace(",", " ").split() if x]
    if ARMS:
        print(f"# ARMS          : {' '.join(ARMS)}  (via aacenc)")

    work = Path(args.workdir)
    work.mkdir(parents=True, exist_ok=True)
    bitrates = [int(b) for b in args.bitrates.split(",")]

    # ---- the clip set -------------------------------------------------------
    # REAL content carries the headline; synthetic classes fill in content types
    # the real corpus does not contain. Labelled, never mixed in one average.
    clips = []
    corpus_dir = REPO / "corpus"
    for name, label in [("corp_mus_piano", "piano (real, CC0)"),
                        ("corp_mus_guitar", "guitar (real, PD)"),
                        ("corp_mus_vocal", "vocal (real, PD)"),
                        ("corp_st_mus_piano", "piano stereo (real, CC0)"),
                        ("corp_st_mus_guitar", "guitar stereo (real, PD)")]:
        p = corpus_dir / f"{name}.wav"
        if p.exists():
            clips.append((name, label, p, "real"))
    # Only pristine sources. The workdir also fills with this harness's OWN
    # decoded outputs (`syn_x_128_ours.wav`, `_ref`, `probe_`...), and a naive
    # glob sweeps those back in as "clips" — which silently turned a 13-clip run
    # into an 85-clip one scoring our output against our output.
    for p in sorted(work.glob("syn_*.wav")):
        st = p.stem
        if re.search(r"_\d+_(ours|ff)$", st) or st.endswith("_ref") or "probe_" in st:
            continue
        clips.append((st, st.replace("syn_", "") + " (synthetic)", p, "synthetic"))

    if not clips:
        sys.exit("no clips found; run the aacexport example and/or fetch_corpus.sh")

    print(f"# system ffmpeg : {SYS_FFMPEG}")
    print(f"# ours          : {OURS}")
    print(f"# clips         : {len(clips)}  bitrates: {bitrates}")

    # ---- piece 4: the null arms, BEFORE anything is believed ----------------
    ref0 = clips[0][2]
    sr0 = 44100
    dec0 = work / "null_ref.wav"
    decode_neutral(ref0, dec0, sr0)
    null_odg = peaq(ref0, dec0)
    print(f"# NULL ARM (ref vs itself through the neutral decoder): ODG = {null_odg}")
    if null_odg is None:
        sys.exit("PEAQ produced no ODG on the null arm — the bridge is broken")
    if null_odg < -0.30:
        print("# WARNING: null-arm ODG is far from 0; the harness floor is poor")

    # ---- stage A: encode + decode everything (fast, ffmpeg does the work) ---
    jobs = []
    for name, label, src, kind in clips:
        dur = probe_duration(src)
        ref_d = work / f"{name}_ref.wav"
        decode_neutral(src, ref_d, 44100)   # once per clip, not per bitrate
        for kbps in bitrates:
            ours_f = work / (f"{name}_{kbps}_ours.aac" if ARMS else f"{name}_{kbps}_ours.m4a")
            ff_f = work / f"{name}_{kbps}_ff.m4a"
            r1 = encode_ours(src, ours_f, kbps)
            r2 = encode_ffmpeg(src, ff_f, kbps)
            if not ours_f.exists() or not ff_f.exists():
                print(f"! {name} @{kbps}k: encode failed "
                      f"(ours rc={r1.returncode}, ff rc={r2.returncode})")
                continue
            ours_d = work / f"{name}_{kbps}_ours.wav"
            ff_d = work / f"{name}_{kbps}_ff.wav"
            decode_neutral(ours_f, ours_d, 44100)
            decode_neutral(ff_f, ff_d, 44100)
            jobs.append(dict(clip=name, label=label, kind=kind, target_kbps=kbps,
                             ref=str(ref_d), ours=str(ours_d), ff=str(ff_d),
                             kbps_ours=measured_kbps(ours_f, dur),
                             kbps_ff=measured_kbps(ff_f, dur)))
    print(f"# encoded {len(jobs)} (clip, bitrate) cells; scoring {2*len(jobs)} PEAQ runs")

    # ---- stage B: PEAQ, in parallel -----------------------------------------
    # PEAQ_python is pure Python and single-threaded; the runs are independent,
    # so a process pool turns ~100 minutes into a few. This changes nothing about
    # any individual score - each is the same deterministic computation.
    import multiprocessing as mp
    pairs = []
    for j in jobs:
        pairs.append((j["ref"], j["ours"]))
        pairs.append((j["ref"], j["ff"]))
    workers = max(1, min(mp.cpu_count() - 2, 16))
    print(f"# PEAQ pool: {workers} workers")
    with mp.Pool(workers) as pool:
        odgs = pool.starmap(peaq, pairs)

    results = []
    for i, j in enumerate(jobs):
        odg_ours, odg_ff = odgs[2 * i], odgs[2 * i + 1]
        j = dict(j)
        j.pop("ref"); j.pop("ours"); j.pop("ff")
        j["odg_ours"] = odg_ours
        j["odg_ff"] = odg_ff
        results.append(j)
        d = (odg_ours - odg_ff) if (odg_ours is not None and odg_ff is not None) else None
        print(f"{j['label']:<28} {j['target_kbps']:>4}k  ours ODG {odg_ours}  "
              f"ff ODG {odg_ff}  delta {d if d is None else round(d, 3)}  "
              f"rate {None if j['kbps_ours'] is None else round(j['kbps_ours'])}/"
              f"{None if j['kbps_ff'] is None else round(j['kbps_ff'])} kbps")

    # ---- speed: both as subprocesses, identical shape, long clips -----------
    speed = []
    if not args.skip_speed:
        print("\n# SPEED — both encoders as subprocesses (equal launch overhead),"
              f" best-of-{args.speed_reps}, ABBA")
        for name in ["corp_long_mus_piano", "corp_long_mus_guitar", "corp_long_mus_vocal"]:
            p = corpus_dir / f"{name}.wav"
            if not p.exists():
                continue
            dur = probe_duration(p)
            for kbps in [128]:
                o = work / f"spd_{name}_ours.m4a"
                f = work / f"spd_{name}_ff.m4a"
                # ABBA: alternate which arm runs first each repetition.
                t_o = t_f = None
                for r in range(args.speed_reps):
                    if r % 2 == 0:
                        a = best_of(lambda: encode_ours(p, o, kbps), 1)
                        b = best_of(lambda: encode_ffmpeg(p, f, kbps), 1)
                    else:
                        b = best_of(lambda: encode_ffmpeg(p, f, kbps), 1)
                        a = best_of(lambda: encode_ours(p, o, kbps), 1)
                    t_o = a if t_o is None else min(t_o, a)
                    t_f = b if t_f is None else min(t_f, b)
                speed.append(dict(clip=name, kbps=kbps, secs=dur,
                                  ours_s=t_o, ff_s=t_f,
                                  ours_x=None if not dur else dur / t_o,
                                  ff_x=None if not dur else dur / t_f))
                print(f"{name:<26} {dur:.1f}s audio  ours {t_o:.3f}s ({dur/t_o:.0f}x RT)  "
                      f"ffmpeg {t_f:.3f}s ({dur/t_f:.0f}x RT)  "
                      f"ratio {t_f/t_o:.2f}x")

    out = work / "results.json"
    out.write_text(json.dumps(dict(null_odg=null_odg, quality=results, speed=speed), indent=2))
    print(f"\nwrote {out}")


if __name__ == "__main__":
    main()
