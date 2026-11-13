"""MP3 spectrum corpus: every rate x channel layout x rate-control mode, from three
independent encoders, then census + decode-vs-FFmpeg on all of it.

    python tools/bench/mp3_spectrum.py            # build corpus + report
    python tools/bench/mp3_spectrum.py --report   # reuse the corpus on disk

Encoders: ours (encprof: CBR low/mid/high + VBR), LAME and shine (both through
FFmpeg). Content: three real CC0/PD music clips, 6 s each, stereo and a mono
downmix, resampled to all nine MPEG Layer III rates.

Two questions, answered separately because they are different questions:

1. COVERAGE -- `examples/coverage` over each encoder's output: which syntax cells
   does each encoder exercise, and which does NO encoder reach (paths our decoder
   is never tested on by this corpus)?
2. DECODE CORRECTNESS -- our decode of every file vs FFmpeg's, aligned at sample
   level (LAME files carry a gapless delay FFmpeg trims and we do not), scored as
   max |error| in s16 LSBs. A file where FFmpeg itself fails is reported, not
   scored.
"""
import os
import subprocess
import sys

import numpy as np

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
OUT = os.environ.get("SPECTRUM_DIR", r"F:\coding\tmp\mp3spectrum")
FFMPEG = os.environ.get(
    "FFMPEG",
    r"C:\Users\talmo\AppData\Local\Microsoft\WinGet\Packages"
    r"\Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe"
    r"\ffmpeg-8.1.2-full_build\bin\ffmpeg.exe",
)
EX = os.path.join(ROOT, "target", "release", "examples")
ENC = os.path.join(EX, "encprof.exe")
COV = os.path.join(EX, "coverage.exe")

CLIPS = ["guitar", "piano", "vocal"]
RATES = [44100, 48000, 32000, 22050, 24000, 16000, 11025, 12000, 8000]


def gen(rate):
    return "V1" if rate >= 32000 else ("V2" if rate >= 16000 else "V25")


# (label, kbps) per generation: low / mid / high CBR points within each table.
CBR = {"V1": [64, 128, 320], "V2": [32, 64, 160], "V25": [8, 32, 64]}


def run(cmd, env=None):
    e = dict(os.environ)
    if env:
        e.update(env)
    return subprocess.run(cmd, capture_output=True, text=True, env=e)


def build():
    srcdir = os.path.join(OUT, "src")
    os.makedirs(srcdir, exist_ok=True)
    for enc in ("ours", "lame", "shine"):
        os.makedirs(os.path.join(OUT, enc), exist_ok=True)
    failures = []
    for clip in CLIPS:
        master = os.path.join(ROOT, "corpus", f"corp_st_mus_{clip}.wav")
        for rate in RATES:
            for ch in (1, 2):
                wav = os.path.join(srcdir, f"{clip}_{rate}_{ch}.wav")
                if not os.path.exists(wav):
                    run([FFMPEG, "-v", "error", "-y", "-i", master, "-t", "6", "-ar", str(rate),
                         "-ac", str(ch), "-c:a", "pcm_s16le", wav])
                g = gen(rate)
                stem = f"{clip}_{rate}_{'st' if ch == 2 else 'mo'}"
                for kbps in CBR[g]:
                    o = os.path.join(OUT, "ours", f"{stem}_cbr{kbps}.mp3")
                    r = run([ENC, wav, str(kbps), o])
                    if r.returncode != 0 or not os.path.exists(o):
                        failures.append(("ours", o, r.stderr.strip()[-120:]))
                    for enc, codec in (("lame", "libmp3lame"), ("shine", "libshine")):
                        o = os.path.join(OUT, enc, f"{stem}_cbr{kbps}.mp3")
                        r = run([FFMPEG, "-v", "error", "-y", "-i", wav, "-c:a", codec, "-b:a", f"{kbps}k", o])
                        if r.returncode != 0:
                            failures.append((enc, o, r.stderr.strip()[-120:]))
                for q in (2, 6):
                    o = os.path.join(OUT, "ours", f"{stem}_vbr{q}.mp3")
                    r = run([ENC, wav, "128", o], env={"VBR_Q": str(q)})
                    if r.returncode != 0 or not os.path.exists(o):
                        failures.append(("ours", o, r.stderr.strip()[-120:]))
                    o = os.path.join(OUT, "lame", f"{stem}_vbr{q}.mp3")
                    r = run([FFMPEG, "-v", "error", "-y", "-i", wav, "-c:a", "libmp3lame", "-q:a", str(q), o])
                    if r.returncode != 0:
                        failures.append(("lame", o, r.stderr.strip()[-120:]))
    return failures


def coverage(enc):
    r = run([COV, os.path.join(OUT, enc)], env={"COVERAGE_TALLY": "1"})
    unreached, cells = [], {}
    section = None
    for line in r.stdout.splitlines():
        if line.startswith("== cells reached by NO"):
            section = "miss"
            continue
        if line.startswith("== every cell"):
            section = "all"
            continue
        if section == "miss" and line.startswith("  ") and not line.startswith("  --"):
            unreached.append(line.strip())
        if section == "all" and line.startswith("  "):
            parts = line.split()
            cells[parts[0]] = int(parts[1])
    return unreached, cells


def ffdecode(mp3):
    r = subprocess.run([FFMPEG, "-v", "error", "-i", mp3, "-f", "s16le", "-acodec", "pcm_s16le", "-"],
                       capture_output=True)
    if r.returncode != 0 or not r.stdout:
        return None
    return np.frombuffer(r.stdout, dtype="<i2").astype(np.int64)


def our_decodes(enc):
    d = os.path.join(OUT, "dump_" + enc)
    os.makedirs(d, exist_ok=True)
    run([COV, os.path.join(OUT, enc)], env={"COVERAGE_DUMP": d})
    return d


def best_offset(a, b, ch):
    """Sample offset of b inside a (per channel), by FFT cross-correlation on ch 0."""
    x = a[::ch][:400000].astype(np.float64)
    y = b[::ch][:400000].astype(np.float64)
    n = 1 << int(np.ceil(np.log2(len(x) + len(y))))
    c = np.fft.irfft(np.fft.rfft(x, n) * np.conj(np.fft.rfft(y, n)), n)
    lag = int(np.argmax(c))
    if lag > n // 2:
        lag -= n
    return lag  # a[lag + i] ~ b[i]


def score(ours, ref, ch):
    lag = best_offset(ours, ref, ch)
    if lag >= 0:
        a, b = ours[lag * ch:], ref
    else:
        a, b = ours, ref[-lag * ch:]
    n = min(len(a), len(b))
    # Skip the first and last 2 frames: decoders differ at the edges by convention.
    edge = 2304 * ch
    if n <= 2 * edge:
        return None
    e = a[edge:n - edge] - b[edge:n - edge]
    sig = float(np.sum(b[edge:n - edge] ** 2))
    err = float(np.sum(e * e))
    return int(np.max(np.abs(e))), (float("inf") if err == 0 else 10 * np.log10(max(sig, 1) / err)), lag


def main():
    if "--report" not in sys.argv:
        fails = build()
        print(f"encode failures: {len(fails)}")
        for f in fails[:40]:
            print("  ", f)

    print("\n=== COVERAGE: cells each encoder never reaches ===")
    union = None
    per = {}
    for enc in ("ours", "lame", "shine"):
        miss, cells = coverage(enc)
        per[enc] = cells
        print(f"\n[{enc}] {len(miss)} unreached")
        for m in miss:
            print("   ", m)
        union = set(miss) if union is None else union & set(miss)
    print(f"\n[ALL THREE ENCODERS] {len(union)} cells reached by none:")
    for m in sorted(union):
        print("   ", m)

    print("\n=== DECODE: ours vs FFmpeg, every file ===")
    for enc in ("ours", "lame", "shine"):
        dump = our_decodes(enc)
        files = sorted(f for f in os.listdir(os.path.join(OUT, enc)) if f.endswith(".mp3"))
        bad, worst, nofmp = [], (0, ""), []
        for f in files:
            ref = ffdecode(os.path.join(OUT, enc, f))
            if ref is None:
                nofmp.append(f)
                continue
            ours = np.fromfile(os.path.join(dump, f[:-4] + ".pcm"), dtype="<i2").astype(np.int64)
            ch = 2 if "_st_" in f else 1
            s = score(ours, ref, ch)
            if s is None:
                continue
            maxe, snr, lag = s
            if maxe > worst[0]:
                worst = (maxe, f"{f} (snr {snr:.1f} dB, lag {lag})")
            if maxe > 1:
                bad.append((f, maxe, round(snr, 1), lag))
        print(f"[{enc}] {len(files)} files: {len(files) - len(bad) - len(nofmp)} exact (<=1 LSB), "
              f"{len(bad)} differ, {len(nofmp)} FFmpeg could not decode; worst {worst}")
        for b in bad[:25]:
            print("    ", b)


if __name__ == "__main__":
    main()
