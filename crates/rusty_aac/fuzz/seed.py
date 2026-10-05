"""Regenerate the fuzz seed corpora from real streams (reproducible; small files).

    python seed.py <work dir>

The work dir holds the raw material, produced once from the ISO/IEC 14496-26
conformance suite and our instruction-count fixtures:

  *.aus   AudioSpecificConfig + raw access units (target-ir/mkaus.py <stream.mp4>)
  *.aac   ADTS streams
  *.latm  LOAS streams (ffmpeg -i <stream> -map 0:a:0 -c copy -f latm)
  eg.f32  interleaved stereo f32le PCM (real music)

Seeds are excerpts -- a few KB -- so libFuzzer starts from real syntax (LC, Main,
LTP, SBR, PS, 5.1 SBR, PCE layouts, ER LD, ELD with and without SBR, 960-sample
frames, LATM) instead of rediscovering it.
"""
import os
import struct
import sys

work = sys.argv[1]
here = os.path.dirname(os.path.abspath(__file__))


def put(target, name, data):
    d = os.path.join(here, "corpus", target)
    os.makedirs(d, exist_ok=True)
    open(os.path.join(d, name), "wb").write(data)


def read_aus(path):
    b = open(path, "rb").read()
    n = struct.unpack_from("<I", b, 0)[0]
    asc, pos, aus = b[4:4 + n], 4 + n, []
    while pos + 4 <= len(b):
        k = struct.unpack_from("<I", b, pos)[0]
        aus.append(b[pos + 4:pos + 4 + k])
        pos += 4 + k
    return asc, aus


for f in sorted(os.listdir(work)):
    p, stem = os.path.join(work, f), os.path.splitext(f)[0]
    if f.endswith(".aus"):
        asc, aus = read_aus(p)
        # decode_config: [len][asc] then [len/4][au, zero-padded to len] (<= 1020 B)
        out = bytes([len(asc)]) + asc
        for au in aus[:6]:
            au = au[:1020]
            q = (len(au) + 3) // 4
            out += bytes([q]) + au + b"\0" * (4 * q - len(au))
        put("decode_config", stem, out)
        put("parsers", stem, asc)
    elif f.endswith(".aac"):
        put("decode_adts", stem, b"\0" + open(p, "rb").read()[:4096])
        put("parsers", stem + "_adts", open(p, "rb").read()[:9])
    elif f.endswith(".latm"):
        put("latm", stem, open(p, "rb").read()[:4096])
    elif f.endswith(".f32"):
        pcm = open(p, "rb").read()
        # encode: [rate idx 4 = 44.1 kHz][2 ch, interleaved][flags][128 kbps]
        put("encode", "music_st_44k", bytes([4, 2, 0, 4]) + pcm[44100 * 8:44100 * 8 + 8192])
        put("encode", "music_mono_48k_tools", bytes([3, 1, 0xfc, 3]) + pcm[:8192])
print("seeded", sorted(os.listdir(os.path.join(here, "corpus"))))
