import sys, numpy as np, warnings
warnings.filterwarnings('ignore')
from scipy.io import wavfile
PEAQDIR = sys.argv[3] if len(sys.argv) > 3 else 'PEAQ_python'
sys.path.insert(0, PEAQDIR)
import io, contextlib
import numpy_PEAQ

def load(p):
    sr, d = wavfile.read(p)
    if d.ndim > 1: d = d[:, 0]
    d = d.astype(np.float64)
    if np.max(np.abs(d)) <= 1.5:  # float wav in [-1,1]
        d *= 32768.0
    return d, sr

def best_delay(ref, test, maxd=3000):
    n = min(len(ref), len(test))
    if n < maxd + 8000: return 0
    w = min(n - maxd, 80000); start = (n - maxd - w) // 2
    r = ref[start:start+w:32]
    best = (1e30, 0)
    for d in range(maxd):
        e = test[start+d:start+d+w:32] - r
        err = float(e @ e)
        if err < best[0]: best = (err, d)
    return best[1]

ref, sr = load(sys.argv[1]); test, _ = load(sys.argv[2])
d = best_delay(ref, test); test = test[d:]
n = min(len(ref), len(test)); ref, test = ref[:n], test[:n]
peaq = numpy_PEAQ.PEAQ(32768, Fs=sr)
with contextlib.redirect_stdout(io.StringIO()):
    peaq.process(ref, test)
    m = peaq.avg_get()
odg = m['ODG'] if isinstance(m, dict) and 'ODG' in m else m
print('ODG= %.4f delay= %d' % (float(odg), d))
