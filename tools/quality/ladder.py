"""MP3 quality ladder — per-clip, per-bitrate ODG against an external oracle.

The great-gate corpus law: a gate is judged **per content class at >= 4 operating
points**, never on an average, and audio verdicts come from an EXTERNAL oracle
(PEAQ) rather than a self-metric. This runs one arm against another across the
whole corpus and prints the per-class table plus a CSV the gate calculator reads.

    python tools/quality/ladder.py --arms base,slack --rates 96,128,160,192

Arms are named env configurations of the encoder (see ARMS). `base` is the
shipping encoder and is byte-identical to a pre-change build, so it doubles as
the null end of the comparison.

PEAQ is deterministic, so the runs are parallelised across processes; that is
safe for quality numbers and would NOT be for timing ones, which is why no
timing is taken here.
"""
import argparse
import concurrent.futures as cf
import os
import re
import subprocess
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
QDIR = os.path.join(ROOT, "tools", "quality")
PEAQ = os.path.join(QDIR, "PEAQ_python")
ENC = os.path.join(ROOT, "target", "release", "examples", "encprof.exe")
FFMPEG = os.environ.get(
    "FFMPEG",
    r"C:\Users\talmo\AppData\Local\Microsoft\WinGet\Packages"
    r"\Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe"
    r"\ffmpeg-8.1.2-full_build\bin\ffmpeg.exe",
)

# Encoder arms as env deltas. The neutral arm must be byte-identical to the
# shipping encoder -- proven with `cmp`, not assumed (great-gate section 4).
ARMS = {
    "base": {"MP3_SHAPE": "outer", "MP3_PSY_DOMAIN": "0"},
    "outer": {"MP3_SHAPE": "outer"},
    "audible": {"MP3_SHAPE": "audible"},
    "slack": {},
    # Psychoacoustic level sweep. `SMR_OFFSET_DB` was swept 3->18 before and read
    # inert (0.002 ODG) -- but that sweep ran through the broken domain comparison,
    # where a few dB of offset was nothing against a 49 dB scale error. It is a
    # live knob for the first time, so the old result is void, not a refutation.
    "smr9": {"MP3_SMR_DB": "9"},
    "smr15": {"MP3_SMR_DB": "15"},
    "smr21": {"MP3_SMR_DB": "21"},
    "smr27": {"MP3_SMR_DB": "27"},
    # P1 signal audit: does the masking model inform WHERE the slack goes? If
    # ranking by raw noise ties ranking by noise-to-mask, it does not.
    "noise": {"MP3_PICK": "noise"},
    "perline": {"MP3_PICK": "perline"},
    "lowfirst": {"MP3_PICK": "lowfirst"},
    # Threshold SHAPE: loosen the band-energy cap that flattens ~40% of bands.
    "cap4": {"MP3_THR_CAP": "4"},
    "cap16": {"MP3_THR_CAP": "16"},
    "cap1e6": {"MP3_THR_CAP": "1000000"},
    # Reservoir donation strength. Swept before and recorded INERT -- but that
    # sweep ran on three tonal music clips, which have 0% near-empty granules, so
    # it priced the reservoir on content where its mechanism does not operate.
    # Speech leaves 19% of granules unused; that is where it can pay.
    "resv05": {"MP3_RESV_GAIN": "0.5"},
    "resv10": {"MP3_RESV_GAIN": "1.0"},
    "resv20": {"MP3_RESV_GAIN": "2.0"},
    "resv40": {"MP3_RESV_GAIN": "4.0"},
    # Not an env config -- dispatched in encode(). The external oracle arm.
    "lame": {},
}


def run(cmd, env=None):
    e = dict(os.environ)
    if env:
        e.update(env)
    return subprocess.run(cmd, capture_output=True, text=True, env=e)


def encode(src, rate, arm, out):
    # `lame` is the external reference, not an arm of our encoder -- it is what
    # makes the ladder a ranking rather than a self-comparison.
    if arm == "lame":
        return run([FFMPEG, "-y", "-hide_banner", "-loglevel", "error",
                    "-i", src, "-c:a", "libmp3lame", "-b:a", f"{rate}k", out])
    return run([ENC, src, str(rate), out], ARMS[arm])


def decode(mp3, wav):
    return run([FFMPEG, "-y", "-hide_banner", "-loglevel", "error",
                "-i", mp3, "-c:a", "pcm_f32le", wav])


def peaq(ref, test):
    r = run([sys.executable, os.path.join(QDIR, "peaq_run.py"), ref, test, PEAQ])
    m = re.search(r"ODG=\s*(-?[\d.]+)", r.stdout + r.stderr)
    return float(m.group(1)) if m else None


def one(job):
    src, clip, rate, arm, tmp = job
    stem = f"{clip}_{rate}_{arm}"
    mp3 = os.path.join(tmp, stem + ".mp3")
    wav = os.path.join(tmp, stem + ".wav")
    encode(src, rate, arm, mp3)
    if not os.path.exists(mp3):
        return clip, rate, arm, None, 0
    decode(mp3, wav)
    if not os.path.exists(wav):
        return clip, rate, arm, None, 0
    kbps = os.path.getsize(mp3) * 8 / dur(src) / 1000.0
    return clip, rate, arm, peaq(src, wav), kbps


_DUR = {}


def dur(path):
    if path not in _DUR:
        r = run([FFMPEG, "-hide_banner", "-i", path, "-f", "null", "-"])
        m = re.search(r"time=(\d+):(\d+):([\d.]+)", r.stderr)
        if m:
            h, mi, s = m.groups()
            _DUR[path] = int(h) * 3600 + int(mi) * 60 + float(s)
        else:
            _DUR[path] = 1.0
    return _DUR[path]


def collect_clips(dirs):
    clips = {}
    for d in dirs:
        if not os.path.isdir(d):
            continue
        for f in sorted(os.listdir(d)):
            if f.endswith(".wav"):
                clips[os.path.splitext(f)[0]] = os.path.join(d, f)
    return clips


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arms", default="base,slack")
    ap.add_argument("--rates", default="96,128,160,192")
    ap.add_argument("--dirs", default=os.path.join(ROOT, "corpus"))
    ap.add_argument("--tmp", default=r"F:\coding\tmp\mp3q\ladder")
    ap.add_argument("--csv", default=r"F:\coding\tmp\mp3q\ladder.csv")
    ap.add_argument("--jobs", type=int, default=10)
    ap.add_argument("--only", default="", help="substring filter on clip name")
    a = ap.parse_args()

    arms = a.arms.split(",")
    rates = [int(r) for r in a.rates.split(",")]
    os.makedirs(a.tmp, exist_ok=True)
    clips = collect_clips(a.dirs.split(","))
    if a.only:
        clips = {k: v for k, v in clips.items() if a.only in k}
    if not clips:
        sys.exit("no clips found")

    jobs = [(src, clip, r, arm, a.tmp)
            for clip, src in clips.items() for r in rates for arm in arms]
    res = {}
    with cf.ProcessPoolExecutor(max_workers=a.jobs) as ex:
        for i, (clip, rate, arm, odg, kbps) in enumerate(ex.map(one, jobs), 1):
            res[(clip, rate, arm)] = (odg, kbps)
            print(f"\r  {i}/{len(jobs)}", end="", file=sys.stderr, flush=True)
    print(file=sys.stderr)

    print(f"method: external PEAQ (PEAQ_python), neutral decoder (ffmpeg), "
          f"per-clip x {len(rates)} rates, arms={arms}; deterministic scorer, "
          f"{a.jobs}-way parallel (no timing taken here)")
    ref = arms[0]
    head = f"{'clip':<24}{'rate':>6}" + "".join(f"{x:>10}" for x in arms)
    if len(arms) > 1:
        head += "".join(f"{'d ' + x:>10}" for x in arms[1:])
    print(head)

    rows = []
    for clip in sorted(clips):
        for r in rates:
            base = res.get((clip, r, ref), (None, 0))[0]
            line = f"{clip:<24}{r:>6}"
            for arm in arms:
                v = res.get((clip, r, arm), (None, 0))[0]
                line += f"{v:>10.4f}" if v is not None else f"{'--':>10}"
            for arm in arms[1:]:
                v = res.get((clip, r, arm), (None, 0))[0]
                d = (v - base) if (v is not None and base is not None) else None
                line += f"{d:>+10.4f}" if d is not None else f"{'--':>10}"
                if d is not None:
                    rows.append((clip, r, arm, base, v, d))
            print(line)

    # Per-class and per-rate aggregates, plus the sign split -- a mean that hides
    # a losing class is exactly what the great-gate law forbids.
    for arm in arms[1:]:
        rs = [x for x in rows if x[2] == arm]
        if not rs:
            continue
        win = sum(1 for x in rs if x[5] > 0)
        loss = sum(1 for x in rs if x[5] < 0)
        print(f"\n{arm} vs {ref}: mean {sum(x[5] for x in rs)/len(rs):+.4f} ODG "
              f"over {len(rs)} points -- {win} better, {loss} worse")
        print(f"  {'per clip':<24}{'mean':>10}{'worst':>10}{'sign':>10}")
        for clip in sorted({x[0] for x in rs}):
            cr = [x[5] for x in rs if x[0] == clip]
            sign = "MIXED" if (max(cr) > 0 > min(cr)) else ("win" if min(cr) > 0 else "LOSS")
            print(f"  {clip:<24}{sum(cr)/len(cr):>+10.4f}{min(cr):>+10.4f}{sign:>10}")
        print(f"  {'per rate':<24}{'mean':>10}{'worst clip':>10}")
        for r in rates:
            cr = [x[5] for x in rs if x[1] == r]
            print(f"  {r:<24}{sum(cr)/len(cr):>+10.4f}{min(cr):>+10.4f}")

    with open(a.csv, "w", encoding="utf-8", newline="") as f:
        f.write("clip,rate,arm,base_odg,arm_odg,gain\n")
        for clip, r, arm, b, v, d in rows:
            f.write(f"{clip},{r},{arm},{b:.6f},{v:.6f},{d:.6f}\n")
    print(f"\nCSV -> {a.csv}")


if __name__ == "__main__":
    main()
