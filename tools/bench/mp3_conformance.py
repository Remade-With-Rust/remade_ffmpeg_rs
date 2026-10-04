"""Three-way MP3 decode conformance: ours vs the ISO/minimp3 reference vs FFmpeg.

    python tools/bench/mp3_conformance.py <vectors_dir> <ours_dir> <ffmpeg_dir>

Each dir holds `<stem>.pcm` as interleaved s16le. For every stem the script scores
three pairs, each at its best whole-granule alignment (decoders differ in how many
undecodable leading frames they emit):

    ref~ff   the CONTROL -- two independent references must agree, or the
             reference (or this harness) is the suspect, not our decoder
    ours~ref
    ours~ff

and localises our disagreement: the fraction of granules with any error above
1 LSB, and the index of the first one. A defect that starts at granule 0 and covers
everything is a different bug from one that hits 3% of granules.
"""
import os
import sys

import numpy as np

GRAN = 576


def load(path):
    if not os.path.exists(path) or os.path.getsize(path) == 0:
        return None
    return np.fromfile(path, dtype="<i2").astype(np.int64)


def channels_for(stem, ref):
    # The ISO set is stereo except where the stream is mono; probe from the bit file
    # header instead of guessing: mode bits 0b11 = mono.
    return ref


def align(a, b, ch):
    """Best whole-granule offset of a against b. Returns (snr, k, maxe, a_al, b_al)."""
    g = GRAN * ch
    best = None
    for k in range(-8, 9):
        off = k * g
        a0, b0 = (off, 0) if off >= 0 else (0, -off)
        if a0 >= len(a) or b0 >= len(b):
            continue
        n = min(len(a) - a0, len(b) - b0)
        x, y = a[a0:a0 + n], b[b0:b0 + n]
        e = x - y
        err = float(np.sum(e * e))
        sig = float(np.sum(y * y))
        snr = float("inf") if err == 0 else 10 * np.log10(max(sig, 1.0) / err)
        if best is None or snr > best[0]:
            best = (snr, k, int(np.max(np.abs(e))) if n else 0, x, y)
    return best


def stream_channels(bitpath):
    data = open(bitpath, "rb").read()
    for i in range(len(data) - 3):
        if data[i] == 0xFF and data[i + 1] & 0xE0 == 0xE0 and (data[i + 1] >> 1) & 3 == 1:
            return 1 if (data[i + 3] >> 6) == 3 else 2
    return 2


def describe(a, b, ch, label):
    if a is None or b is None:
        return f"{label}: n/a"
    snr, k, maxe, x, y = align(a, b, ch)
    g = GRAN * ch
    ng = len(x) // g
    bad = [i for i in range(ng) if np.max(np.abs(x[i * g:(i + 1) * g] - y[i * g:(i + 1) * g])) > 1]
    verdict = "ok " if maxe <= 1 else "BAD"
    first = f" first@{bad[0]}" if bad else ""
    return f"{label} {verdict} max {maxe:>5} snr {snr:6.1f} off {k:+d} badgran {len(bad)}/{ng}{first}"


def main():
    vec, ours, ff = sys.argv[1:4]
    stems = sorted({f[:-4] for f in os.listdir(ours) if f.endswith(".pcm")})
    for s in stems:
        bitp = os.path.join(vec, s + ".bit")
        if not os.path.exists(bitp):
            continue
        ch = stream_channels(bitp)
        r, o, f = load(os.path.join(vec, s + ".pcm")), load(os.path.join(ours, s + ".pcm")), load(os.path.join(ff, s + ".pcm"))
        if r is None and f is None:
            continue
        print(f"{s:<40} ch={ch}")
        print("   " + describe(f, r, ch, "CONTROL ff~ref "))
        print("   " + describe(o, r, ch, "ours~ref       "))
        print("   " + describe(o, f, ch, "ours~ff        "))


if __name__ == "__main__":
    main()
