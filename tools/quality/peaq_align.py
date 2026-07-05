"""PEAQ with sample-accurate alignment — for encoders whose output isn't end-trimmed.

Like `peaq_run.py`, but refines the delay to the exact sample (a coarse strided search
then a full-resolution local search) before scoring. Necessary for our Vorbis encoder,
whose stream carries a non-zero decoder delay + priming packet that ffmpeg does not trim
(unlike libvorbis, which lands delay=0); PEAQ collapses to ~-4 on a few samples of
misalignment, so the coarse alignment in peaq_run.py reads garbage on our output.

    python tools/quality/peaq_align.py reference.wav test.wav [PEAQ_dir]
"""
import sys, io, contextlib, numpy as np, warnings
warnings.filterwarnings('ignore')
from scipy.io import wavfile
PEAQDIR = sys.argv[3] if len(sys.argv) > 3 else 'PEAQ_python'
sys.path.insert(0, PEAQDIR)
import numpy_PEAQ


def load(p):
    sr, d = wavfile.read(p)
    if d.ndim > 1:
        d = d[:, 0]
    d = d.astype(np.float64)
    if np.max(np.abs(d)) <= 1.5:  # float wav in [-1, 1]
        d *= 32768.0
    return d, sr


def shift_err(ref, test, s, start, w, step=1):
    """SSD of ref vs test at signed shift s (s>0: test lags; s<0: test leads)."""
    r = ref[start:start + w:step]
    t = test[start + s:start + s + w:step]
    m = min(len(r), len(t))
    e = t[:m] - r[:m]
    return float(e @ e)


def best_shift(ref, test, maxd=5000):
    """Signed integer shift of `test` relative to `ref` minimizing SSD — searched in BOTH
    directions (our decoder drops leading samples, so the decode *leads* the reference).
    Coarse strided pass, then a full-resolution local refine."""
    n = min(len(ref), len(test))
    w = min(n - 2 * maxd, 100000)
    if w <= 0:
        return 0
    start = (n - w) // 2  # centered, so start+s stays in-bounds for |s|<=maxd
    lo, hi = -maxd, maxd
    best = (1e30, 0)
    for s in range(lo, hi, 8):
        err = shift_err(ref, test, s, start, w, step=8)
        if err < best[0]:
            best = (err, s)
    s0 = best[1]
    best = (1e30, s0)
    for s in range(s0 - 12, s0 + 13):
        err = shift_err(ref, test, s, start, w, step=1)
        if err < best[0]:
            best = (err, s)
    return best[1]


ref, sr = load(sys.argv[1]); test, _ = load(sys.argv[2])
s = best_shift(ref, test)
if s >= 0:
    test = test[s:]
else:
    ref = ref[-s:]
n = min(len(ref), len(test)); ref, test = ref[:n], test[:n]
d = s
# normalized correlation, a cheap alignment sanity check
corr = float(ref @ test) / (np.linalg.norm(ref) * np.linalg.norm(test) + 1e-12)
peaq = numpy_PEAQ.PEAQ(32768, Fs=sr)
with contextlib.redirect_stdout(io.StringIO()):
    peaq.process(ref, test)
    m = peaq.avg_get()
odg = m['ODG'] if isinstance(m, dict) and 'ODG' in m else m
print('ODG= %.4f delay= %d corr= %.4f' % (float(odg), d, corr))
