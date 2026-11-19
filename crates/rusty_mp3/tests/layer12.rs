//! MPEG-1/2 Layer I and Layer II decoding.
//!
//! * `iso_layer12_vectors_*`: the ISO 11172-4 Layer I/II conformance streams
//!   (and two of the minimp3 suite's nonstandard ones), scored against their
//!   reference PCM. Not vendored, like the Layer III vectors: set
//!   `MP3_ISO_VECTORS` (see `tests/iso_vectors.rs`); without it they report
//!   SKIPPED and pass.
//! * Everything else is self-contained: streams are built here by a small
//!   Layer I/II encoder (this crate's analysis filterbank, a greedy fixed
//!   allocation, its own transcription of the allocation tables), so joint
//!   stereo, dual channel, CRC, free format, the MPEG-2 rates and hostile input
//!   are all covered without fixtures.

use std::path::Path;

use rusty_mp3::encode::filterbank;
use rusty_mp3::layer12::{Layer, Layer12Header};
use rusty_mp3::{decode_pipelined, Error, Mp3Decoder};

// ---------------------------------------------------------------------------
// Decoding helpers
// ---------------------------------------------------------------------------

struct Decoded {
    samples: Vec<f32>,
    channels: usize,
    rate: u32,
    frames: usize,
}

/// Decode with the streaming decoder, pushing `chunk` bytes at a time.
fn decode_chunked(bytes: &[u8], chunk: usize) -> Decoded {
    let mut dec = Mp3Decoder::new();
    let mut d = Decoded {
        samples: Vec::new(),
        channels: 0,
        rate: 0,
        frames: 0,
    };
    let drain = |dec: &mut Mp3Decoder, d: &mut Decoded| loop {
        match dec.next_frame() {
            Ok(f) => {
                d.channels = usize::from(f.channels);
                d.rate = f.sample_rate;
                d.frames += 1;
                d.samples.extend_from_slice(&f.samples);
            }
            Err(Error::Again | Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    };
    for c in bytes.chunks(chunk.max(1)) {
        dec.push(c);
        drain(&mut dec, &mut d);
    }
    dec.flush();
    drain(&mut dec, &mut d);
    d
}

fn decode(bytes: &[u8]) -> Decoded {
    decode_chunked(bytes, usize::MAX)
}

fn to_s16(x: &[f32]) -> Vec<i32> {
    x.iter()
        .map(|s| (s * 32768.0).round().clamp(-32768.0, 32767.0) as i32)
        .collect()
}

fn bits_eq(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

// ---------------------------------------------------------------------------
// ISO conformance vectors (env-gated)
// ---------------------------------------------------------------------------

/// (stem, what it guards). The reference PCM is the ISO reference decoder's
/// 16-bit output; within 1 LSB is rounding. The `.bit` files are cut short of
/// their references, so every decoded sample is compared, at offset 0.
const ISO_STREAMS: &[(&str, &str)] = &[
    ("l1-fl1", "Layer I, 32 kHz stereo, CRC"),
    ("l1-fl2", "Layer I, 44.1 kHz JOINT stereo, CRC"),
    ("l1-fl3", "Layer I, 48 kHz joint stereo, CRC"),
    ("l1-fl4", "Layer I, 32 kHz mono 32 kbps"),
    ("l1-fl5", "Layer I, 48 kHz DUAL channel, 448 kbps"),
    ("l1-fl6", "Layer I, 44.1 kHz stereo"),
    ("l1-fl7", "Layer I, 44.1 kHz stereo"),
    ("l1-fl8", "Layer I, 44.1 kHz stereo, no CRC"),
    ("l2-fl10", "Layer II, 32 kHz stereo, CRC"),
    ("l2-fl11", "Layer II, 44.1 kHz stereo"),
    ("l2-fl12", "Layer II, 48 kHz stereo"),
    ("l2-fl13", "Layer II, 32 kHz mono 32 kbps (table 3-B.2d)"),
    ("l2-fl14", "Layer II, 48 kHz DUAL channel 384 kbps"),
    ("l2-fl15", "Layer II, 48 kHz stereo 384 kbps"),
    ("l2-fl16", "Layer II, 48 kHz stereo 256 kbps"),
    ("l2-test32", "Layer II, MPEG-2 24 kHz (LSF table)"),
    ("l2-nonstandard-free_format", "Layer II, FREE FORMAT"),
];

fn iso_file(dir: &Path, name: &str) -> Vec<u8> {
    std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn pcm16(bytes: &[u8]) -> Vec<i32> {
    bytes
        .chunks_exact(2)
        .map(|b| i32::from(i16::from_le_bytes([b[0], b[1]])))
        .collect()
}

/// Complete frames in a clean table-bitrate stream (`None` for free format).
fn whole_frames(bit: &[u8]) -> Option<usize> {
    let (mut pos, mut n) = (0, 0);
    while let Some(hb) = bit.get(pos..pos + 4) {
        let h = Layer12Header::parse(hb.try_into().unwrap()).ok()?;
        let size = h.frame_size()?;
        if pos + size > bit.len() {
            break;
        }
        n += 1;
        pos += size;
    }
    Some(n)
}

fn max_abs_diff(a: &[i32], b: &[i32]) -> i32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .max()
        .unwrap_or(0)
}

#[test]
fn iso_layer12_vectors_decode_within_one_lsb() {
    let Some(dir) = std::env::var_os("MP3_ISO_VECTORS") else {
        eprintln!("SKIPPED: set MP3_ISO_VECTORS to the vectors dir (scripts/fetch-mp3-vectors.sh)");
        return;
    };
    let dir = Path::new(&dir);
    let mut failures = Vec::new();
    for &(stem, guards) in ISO_STREAMS {
        let bit = iso_file(dir, &format!("{stem}.bit"));
        let refs = pcm16(&iso_file(dir, &format!("{stem}.pcm")));
        let d = decode(&bit);
        let ours = to_s16(&d.samples);
        let e = max_abs_diff(&ours, &refs);
        eprintln!(
            "{stem:<28} {:>3} frames {:>6} samples (ref {:>6})  max|e| {e} LSB   ({guards})",
            d.frames,
            ours.len(),
            refs.len()
        );
        if ours.len() > refs.len() {
            failures.push(format!(
                "{stem}: {} samples vs reference {}",
                ours.len(),
                refs.len()
            ));
        }
        // Every frame the file holds whole is decoded.
        if let Some(whole) = whole_frames(&bit) {
            if d.frames != whole {
                failures.push(format!("{stem}: {} frames of {whole}", d.frames));
            }
        }
        if e > 1 {
            failures.push(format!("{stem}: max|e| {e} LSB -- guards {guards}"));
        }
    }
    assert!(
        failures.is_empty(),
        "Layer I/II vectors out of tolerance:\n  {}",
        failures.join("\n  ")
    );
}

/// `l2-nonstandard-test32-size` is `l2-test32` with ONE byte of frame 12's
/// allocation changed. The frame decodes to whatever those bits say, the
/// synthesis tail carries it into frame 13, and every other frame must still
/// match the clean stream's reference. (minimp3's own reference for this file
/// drops the frame instead; neither the frame count nor the other frames
/// should change, so the clean reference is the stricter oracle.)
#[test]
fn iso_corrupt_frame_costs_only_itself() {
    let Some(dir) = std::env::var_os("MP3_ISO_VECTORS") else {
        eprintln!("SKIPPED: set MP3_ISO_VECTORS");
        return;
    };
    let dir = Path::new(&dir);
    let bit = iso_file(dir, "l2-nonstandard-test32-size.bit");
    let refs = pcm16(&iso_file(dir, "l2-test32.pcm"));
    let d = decode(&bit);
    let ours = to_s16(&d.samples);
    assert_eq!(ours.len(), refs.len(), "frame count must not change");
    let per_frame = 1152 * 2;
    let hit = 12 * per_frame..14 * per_frame;
    let mut worst = 0;
    for (i, (a, b)) in ours.iter().zip(&refs).enumerate() {
        if !hit.contains(&i) {
            worst = worst.max((a - b).abs());
        }
    }
    eprintln!("outside frames 12-13: max|e| {worst} LSB");
    assert!(worst <= 1, "{worst}");
}

/// `l2-nonstandard-fl1_fl2_ff`: free-format Layer I that switches sample rate
/// (32 kHz to 44.1 kHz) and so changes its free-format frame size mid-stream.
/// Every whole frame decodes: 3 at 32 kHz and 4 at 44.1 kHz (the last is cut).
#[test]
fn iso_free_format_rate_switch() {
    let Some(dir) = std::env::var_os("MP3_ISO_VECTORS") else {
        eprintln!("SKIPPED: set MP3_ISO_VECTORS");
        return;
    };
    let bit = iso_file(Path::new(&dir), "l2-nonstandard-fl1_fl2_ff.bit");
    let mut dec = Mp3Decoder::new();
    dec.push(&bit);
    dec.flush();
    let mut rates = Vec::new();
    while let Ok(f) = dec.next_frame() {
        assert_eq!(f.samples.len(), 384 * 2);
        rates.push(f.sample_rate);
    }
    assert_eq!(rates, [32000, 32000, 32000, 44100, 44100, 44100, 44100]);
}

// ---------------------------------------------------------------------------
// A small Layer I / II encoder (test support)
// ---------------------------------------------------------------------------

/// One subband's allocation row: field width and the steps each nonzero
/// allocation selects (ISO/IEC 11172-3 Tables 3-B.2a-d, 13818-3 B.1),
/// transcribed independently of the decoder's tables.
struct Row {
    nbal: u32,
    steps: &'static [u32],
}
const RA: Row = Row {
    nbal: 4,
    steps: &[
        3, 7, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767, 65535,
    ],
};
const RB: Row = Row {
    nbal: 4,
    steps: &[
        3, 5, 7, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 65535,
    ],
};
const RC: Row = Row {
    nbal: 3,
    steps: &[3, 5, 7, 9, 15, 31, 65535],
};
const RD: Row = Row {
    nbal: 2,
    steps: &[3, 5, 65535],
};
const RE: Row = Row {
    nbal: 4,
    steps: &[
        3, 5, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767,
    ],
};
const RF: Row = Row {
    nbal: 3,
    steps: &[3, 5, 9, 15, 31, 63, 127],
};
const RG: Row = Row {
    nbal: 4,
    steps: &[
        3, 5, 7, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383,
    ],
};
const RH: Row = Row {
    nbal: 2,
    steps: &[3, 5, 9],
};

fn l2_rows(kbps: u32, channels: usize, rate: u32, lsf: bool) -> Vec<&'static Row> {
    let spans: &[(usize, &Row)] = if lsf {
        &[(4, &RG), (7, &RF), (19, &RH)]
    } else {
        let ch = kbps / channels as u32;
        if (rate == 48000 && ch >= 56) || (56..=80).contains(&ch) {
            &[(3, &RA), (8, &RB), (12, &RC), (4, &RD)]
        } else if rate != 48000 && ch >= 96 {
            &[(3, &RA), (8, &RB), (12, &RC), (7, &RD)]
        } else if rate != 32000 && ch <= 48 {
            &[(2, &RE), (6, &RF)]
        } else {
            &[(2, &RE), (10, &RF)]
        }
    };
    spans
        .iter()
        .flat_map(|&(n, r)| std::iter::repeat_n(r, n))
        .collect()
}

#[derive(Clone, Copy)]
struct Cfg {
    layer: Layer,
    /// 3 = MPEG-1, 2 = MPEG-2.
    version: u32,
    rate: u32,
    kbps: u32,
    /// 0 stereo, 1 joint, 2 dual, 3 mono.
    mode: u32,
    /// Joint-stereo bound code (bound = 4 * (ext + 1)).
    ext: u32,
    crc: bool,
    /// Write bitrate index 0 (frames keep the `kbps` size).
    free: bool,
}

impl Cfg {
    fn l2(rate: u32, kbps: u32, mode: u32) -> Self {
        Self {
            layer: Layer::II,
            version: if rate >= 32000 { 3 } else { 2 },
            rate,
            kbps,
            mode,
            ext: 0,
            crc: false,
            free: false,
        }
    }
    fn l1(rate: u32, kbps: u32, mode: u32) -> Self {
        Self {
            layer: Layer::I,
            ..Self::l2(rate, kbps, mode)
        }
    }
    fn channels(&self) -> usize {
        if self.mode == 3 {
            1
        } else {
            2
        }
    }
    fn bound(&self) -> usize {
        if self.mode == 1 {
            4 * (self.ext as usize + 1)
        } else {
            32
        }
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}
impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bits: 0,
        }
    }
    fn put(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            if self.bits % 8 == 0 {
                self.bytes.push(0);
            }
            if (v >> i) & 1 == 1 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }
}

/// ISO CRC-16 (x^16 + x^15 + x^2 + 1, initial 0xFFFF) over `n` bits of `b`
/// starting at bit `from`.
fn crc16(b: &[u8], from: usize, n: usize, mut crc: u16) -> u16 {
    for i in from..from + n {
        let bit = (b[i / 8] >> (7 - i % 8)) & 1;
        let top = (crc >> 15) as u8 & 1;
        crc <<= 1;
        if top ^ bit == 1 {
            crc ^= 0x8005;
        }
    }
    crc
}

const BITRATES: [[[u32; 16]; 2]; 2] = [
    // MPEG-1: Layer I, Layer II
    [
        [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
        ],
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
        ],
    ],
    // MPEG-2: Layer I, Layer II
    [
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
        ],
        [
            0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
        ],
    ],
];

/// The scalefactor index (0..=62) whose scale `2^(1 - i/3)` is the smallest
/// one still covering `peak`.
fn scale_index(peak: f32) -> u32 {
    let mut i = 62;
    while i > 0 && 2f32.powf(1.0 - i as f32 / 3.0) < peak {
        i -= 1;
    }
    i
}
fn scale_of(i: u32) -> f32 {
    2f32.powf(1.0 - i as f32 / 3.0)
}
/// The quantizer the decoder inverts: `value = scale * (q - (steps-1)/2) * 2/steps`.
fn quantize(x: f32, steps: u32) -> u32 {
    let q = (x * steps as f32 / 2.0 + (steps as f32 - 1.0) / 2.0).round();
    q.clamp(0.0, (steps - 1) as f32) as u32
}
fn code_bits(steps: u32) -> (bool, u32) {
    match steps {
        3 => (true, 5),
        5 => (true, 7),
        9 => (true, 10),
        s => (false, 32 - s.leading_zeros()),
    }
}

/// Encode interleaved PCM. Returns the stream.
fn encode(pcm: &[f32], cfg: Cfg) -> Vec<u8> {
    let nch = cfg.channels();
    let layer2 = cfg.layer == Layer::II;
    let lsf = cfg.version != 3;
    let spf = if layer2 { 1152 } else { 384 };
    let blocks_per_frame = spf / 32;
    // Analysis, per channel, into blocks of 32 subband samples.
    let frames_n = pcm.len() / nch / spf;
    let granules = (frames_n * spf).div_ceil(576);
    let mut sb: Vec<Vec<[f32; 32]>> = vec![Vec::new(); nch];
    for (ch, out) in sb.iter_mut().enumerate() {
        let mut fifo = [0f32; 512];
        for g in 0..granules {
            let mut x = [0f32; 576];
            for (i, v) in x.iter_mut().enumerate() {
                *v = pcm.get((g * 576 + i) * nch + ch).copied().unwrap_or(0.0);
            }
            let s = filterbank::analyze(&x, &mut fifo);
            for v in 0..18 {
                let mut b = [0f32; 32];
                for k in 0..32 {
                    b[k] = s[k][v];
                }
                out.push(b);
            }
        }
    }
    let vi = usize::from(lsf);
    let li = usize::from(layer2);
    let bri = BITRATES[vi][li]
        .iter()
        .position(|&b| b == cfg.kbps)
        .expect("legal bitrate") as u32;
    let base = match cfg.rate * if lsf { 2 } else { 1 } {
        44100 => 0,
        48000 => 1,
        32000 => 2,
        r => panic!("rate {r}"),
    };
    let unit = if layer2 {
        144 * cfg.kbps * 1000
    } else {
        12 * cfg.kbps * 1000
    };
    let rows: Vec<&Row> = if layer2 {
        l2_rows(cfg.kbps, nch, cfg.rate, lsf)
    } else {
        Vec::new()
    };
    let sblimit = if layer2 { rows.len() } else { 32 };
    let bound = cfg.bound().min(sblimit);
    let parts = if layer2 { 3 } else { 1 };

    let mut stream = Vec::new();
    for f in 0..frames_n {
        let pad = (u64::from(unit) * (f as u64 + 1) / u64::from(cfg.rate)
            - u64::from(unit) * f as u64 / u64::from(cfg.rate))
            > u64::from(unit / cfg.rate);
        let slots = (unit / cfg.rate) as usize + usize::from(pad);
        let size = if layer2 { slots } else { slots * 4 };
        let blk = |ch: usize, j: usize| &sb[ch][f * blocks_per_frame + j];

        // Scalefactors per (ch, sb, part) and scfsi.
        let mut sf = vec![[[0u32; 3]; 32]; 2];
        let mut scfsi = vec![[0u32; 32]; 2];
        for ch in 0..nch {
            for i in 0..sblimit {
                for p in 0..parts {
                    let peak = (0..12)
                        .map(|j| blk(ch, p * 12 + j)[i].abs())
                        .fold(0f32, f32::max);
                    sf[ch][i][p] = scale_index(peak);
                }
                if layer2 {
                    // Share scalefactors that differ by one step (the larger
                    // scale covers both), exercising every scfsi pattern.
                    let s = &mut sf[ch][i];
                    let close = |a: u32, b: u32| a.abs_diff(b) <= 1;
                    scfsi[ch][i] = if close(s[0], s[1]) && close(s[1], s[2]) && close(s[0], s[2]) {
                        let m = s[0].min(s[1]).min(s[2]);
                        *s = [m; 3];
                        2
                    } else if close(s[0], s[1]) {
                        let m = s[0].min(s[1]);
                        s[0] = m;
                        s[1] = m;
                        1
                    } else if close(s[1], s[2]) {
                        let m = s[1].min(s[2]);
                        s[1] = m;
                        s[2] = m;
                        3
                    } else {
                        0
                    };
                }
            }
        }

        // Greedy allocation within the frame budget.
        let budget = size * 8;
        let mut alloc = vec![[0usize; 32]; 2];
        let cost = |alloc: &Vec<[usize; 32]>| -> usize {
            let mut bits = 32 + if cfg.crc { 16 } else { 0 };
            for i in 0..sblimit {
                let nbal = if layer2 { rows[i].nbal as usize } else { 4 };
                let coded = if i < bound { nch } else { 1 };
                bits += nbal * coded;
                for ch in 0..nch {
                    let a = if i < bound { alloc[ch][i] } else { alloc[0][i] };
                    if a == 0 {
                        continue;
                    }
                    if layer2 {
                        bits += 2 + 6 * [3, 2, 1, 2][scfsi[ch][i] as usize];
                    } else {
                        bits += 6;
                    }
                }
                for ch in 0..coded {
                    let a = alloc[ch][i];
                    if a == 0 {
                        continue;
                    }
                    bits += if layer2 {
                        let (grouped, w) = code_bits(rows[i].steps[a - 1]);
                        12 * if grouped { w as usize } else { 3 * w as usize }
                    } else {
                        12 * (a + 1)
                    };
                }
            }
            bits
        };
        // Subbands with nothing in them (filterbank leakage only) get nothing.
        let live = |i: usize| (0..nch).any(|ch| sf[ch][i][..parts].iter().any(|&x| x < 40));
        loop {
            let mut grew = false;
            for i in 0..sblimit {
                if !live(i) {
                    continue;
                }
                let max = if layer2 { rows[i].steps.len() } else { 14 };
                for ch in 0..if i < bound { nch } else { 1 } {
                    if alloc[ch][i] < max {
                        alloc[ch][i] += 1;
                        if i >= bound {
                            alloc[1][i] = alloc[0][i];
                        }
                        if cost(&alloc) <= budget {
                            grew = true;
                        } else {
                            alloc[ch][i] -= 1;
                            if i >= bound {
                                alloc[1][i] = alloc[0][i];
                            }
                        }
                    }
                }
            }
            if !grew {
                break;
            }
        }

        // Bitstream.
        let mut w = BitWriter::new();
        w.put(11, 0x7FF);
        w.put(2, cfg.version);
        w.put(2, if layer2 { 0b10 } else { 0b11 });
        w.put(1, u32::from(!cfg.crc));
        w.put(4, if cfg.free { 0 } else { bri });
        w.put(2, base);
        w.put(1, u32::from(pad));
        w.put(1, 0);
        w.put(2, cfg.mode);
        w.put(2, cfg.ext);
        w.put(4, 0b0100); // not copyright, original, no emphasis
        if cfg.crc {
            w.put(16, 0);
        }
        let side_start = w.bits;
        for i in 0..sblimit {
            let nbal = if layer2 { rows[i].nbal } else { 4 };
            for ch in 0..if i < bound { nch } else { 1 } {
                w.put(nbal, alloc[ch][i] as u32);
            }
        }
        if layer2 {
            for i in 0..sblimit {
                for ch in 0..nch {
                    if alloc[ch][i] != 0 {
                        w.put(2, scfsi[ch][i]);
                    }
                }
            }
        }
        let side_end = w.bits;
        for i in 0..sblimit {
            for ch in 0..nch {
                if alloc[ch][i] == 0 {
                    continue;
                }
                let s = sf[ch][i];
                let sent: &[u32] = if layer2 {
                    match scfsi[ch][i] {
                        0 => &s[..],
                        1 => &[s[0], s[2]],
                        2 => &s[..1],
                        _ => &[s[0], s[1]],
                    }
                } else {
                    &s[..1]
                };
                for &v in sent {
                    w.put(6, v);
                }
            }
        }
        // Samples: Layer II in 3 parts x 4 granules of 3; Layer I 12 singles.
        for gidx in 0..12 {
            let (p, triple) = if layer2 { (gidx / 4, true) } else { (0, false) };
            for i in 0..sblimit {
                let coded = if i < bound { nch } else { 1 };
                for ch in 0..coded {
                    let a = alloc[ch][i];
                    if a == 0 {
                        continue;
                    }
                    let steps = if layer2 {
                        rows[i].steps[a - 1]
                    } else {
                        (1u32 << (a + 1)) - 1
                    };
                    let value = |j: usize| -> f32 {
                        if i < bound {
                            blk(ch, j)[i] / scale_of(sf[ch][i][p])
                        } else {
                            // Intensity: the sum, normalised by the sum of scales.
                            (blk(0, j)[i] + blk(1, j)[i])
                                / (scale_of(sf[0][i][p]) + scale_of(sf[1][i][p]))
                        }
                    };
                    if triple {
                        let j0 = gidx * 3;
                        let q: Vec<u32> = (0..3).map(|m| quantize(value(j0 + m), steps)).collect();
                        let (grouped, wbits) = code_bits(steps);
                        if grouped {
                            w.put(wbits, q[0] + q[1] * steps + q[2] * steps * steps);
                        } else {
                            for &v in &q {
                                w.put(wbits, v);
                            }
                        }
                    } else {
                        w.put(a as u32 + 1, quantize(value(gidx), steps));
                    }
                }
            }
        }
        assert!(w.bits <= budget, "allocation overflowed the frame");
        let mut frame = w.bytes;
        frame.resize(size, 0);
        if cfg.crc {
            let mut c = crc16(&frame, 16, 16, 0xFFFF);
            c = crc16(&frame, side_start, side_end - side_start, c);
            frame[4] = (c >> 8) as u8;
            frame[5] = c as u8;
        }
        stream.extend_from_slice(&frame);
    }
    stream
}

// ---------------------------------------------------------------------------
// Signals and scoring
// ---------------------------------------------------------------------------

/// Interleaved test signal: a few tones per channel, the right channel at
/// `right_gain` times a (phase-shifted unless `same`) copy.
fn signal(rate: u32, channels: usize, seconds: f32, right_gain: f32, same: bool) -> Vec<f32> {
    let n = (rate as f32 * seconds) as usize;
    let mut out = Vec::with_capacity(n * channels);
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let l = 0.30 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            + 0.15 * (2.0 * std::f32::consts::PI * 1250.0 * t).sin()
            + 0.05 * (2.0 * std::f32::consts::PI * 3100.0 * t).sin();
        out.push(l);
        if channels == 2 {
            let r = if same {
                l
            } else {
                0.30 * (2.0 * std::f32::consts::PI * 660.0 * t).sin()
                    + 0.10 * (2.0 * std::f32::consts::PI * 2000.0 * t).sin()
            };
            out.push(r * right_gain);
        }
    }
    out
}

/// Best per-channel SNR (dB) of `out` against `input`, over the filterbank
/// delay (481 samples) +- a little.
fn snr(input: &[f32], out: &[f32], channels: usize) -> Vec<f64> {
    (0..channels)
        .map(|ch| {
            let mut best = f64::NEG_INFINITY;
            for delay in 478..=484 {
                let (mut s, mut e) = (0f64, 0f64);
                let n = (out.len() / channels).min(input.len() / channels);
                for i in (delay + 1152)..n {
                    let r = f64::from(input[(i - delay) * channels + ch]);
                    let o = f64::from(out[i * channels + ch]);
                    s += r * r;
                    e += (r - o) * (r - o);
                }
                best = best.max(10.0 * (s / e.max(1e-30)).log10());
            }
            best
        })
        .collect()
}

fn check(name: &str, cfg: Cfg, input: &[f32], min_snr: f64) -> Decoded {
    let stream = encode(input, cfg);
    let d = decode(&stream);
    let spf = if cfg.layer == Layer::II { 1152 } else { 384 };
    let frames = input.len() / cfg.channels() / spf;
    assert_eq!(d.frames, frames, "{name}: frame count");
    assert_eq!(d.channels, cfg.channels(), "{name}: channels");
    assert_eq!(d.rate, cfg.rate, "{name}: rate");
    assert_eq!(
        d.samples.len(),
        frames * spf * cfg.channels(),
        "{name}: samples"
    );
    let s = snr(input, &d.samples, cfg.channels());
    eprintln!("{name:<36} {} frames  SNR {s:.1?} dB", d.frames);
    for (ch, v) in s.iter().enumerate() {
        assert!(*v > min_snr, "{name}: channel {ch} SNR {v:.1} dB");
    }
    // Every push chunking and the pipelined entry decode identically.
    for chunk in [1, 7, 1000] {
        assert!(
            bits_eq(&decode_chunked(&stream, chunk).samples, &d.samples),
            "{name}: chunk {chunk}"
        );
    }
    let piped: Vec<f32> = decode_pipelined(&stream)
        .into_iter()
        .flat_map(|f| f.samples)
        .collect();
    assert!(bits_eq(&piped, &d.samples), "{name}: pipelined");
    d
}

// ---------------------------------------------------------------------------
// Self-contained decode tests
// ---------------------------------------------------------------------------

#[test]
fn layer2_mpeg1_all_tables_and_modes() {
    // (rate, kbps, mode): every MPEG-1 allocation table, mono and stereo, at
    // low and high bitrates.
    for &(rate, kbps, mode, min) in &[
        (44100, 192, 0, 30.0), // table B.2b (96 kbps/ch)
        (44100, 128, 0, 25.0), // B.2a (64/ch)
        (44100, 64, 0, 10.0),  // B.2c (32/ch, 8 subbands)
        (32000, 64, 0, 10.0),  // B.2d (32/ch at 32 kHz, 12 subbands)
        (48000, 256, 0, 30.0), // B.2a at 48 kHz
        (48000, 384, 2, 30.0), // dual channel
        (44100, 160, 3, 30.0), // mono, B.2b
        (32000, 48, 3, 15.0),  // mono, B.2d
    ] {
        let st = mode != 3;
        let input = signal(rate, if st { 2 } else { 1 }, 0.5, 0.7, false);
        check(
            &format!("L2 {rate} {kbps}k mode {mode}"),
            Cfg::l2(rate, kbps, mode),
            &input,
            min,
        );
    }
}

#[test]
fn layer2_mpeg2_low_rates() {
    for &(rate, kbps, mode) in &[
        (24000, 96, 0),
        (22050, 128, 0),
        (16000, 32, 0),
        (24000, 160, 3),
        (22050, 48, 3),
        (16000, 16, 3),
    ] {
        let st = mode != 3;
        let input = signal(rate, if st { 2 } else { 1 }, 0.6, 0.7, false);
        check(
            &format!("L2 LSF {rate} {kbps}k mode {mode}"),
            Cfg::l2(rate, kbps, mode),
            &input,
            8.0,
        );
    }
}

#[test]
fn layer2_joint_stereo_every_bound() {
    // The right channel is the left at -6 dB (exactly 3 scalefactor steps), so
    // intensity coding above the bound reproduces both channels.
    let input = signal(44100, 2, 0.5, 0.5, true);
    let mut prev: Option<Vec<f32>> = None;
    for ext in 0..4 {
        let cfg = Cfg {
            ext,
            ..Cfg::l2(44100, 192, 1)
        };
        let d = check(
            &format!("L2 joint stereo bound {}", 4 * (ext + 1)),
            cfg,
            &input,
            25.0,
        );
        // Different bounds are different streams: the decode must differ.
        if let Some(p) = &prev {
            assert!(
                !bits_eq(p, &d.samples),
                "bound {ext} decoded like the previous"
            );
        }
        prev = Some(d.samples);
    }
    // And at the MPEG-2 rates.
    let input = signal(24000, 2, 0.6, 0.5, true);
    let cfg = Cfg {
        ext: 1,
        ..Cfg::l2(24000, 96, 1)
    };
    check("L2 LSF joint stereo bound 8", cfg, &input, 20.0);
}

#[test]
fn layer1_modes_and_rates() {
    for &(rate, kbps, mode, ext) in &[
        (44100, 384, 0, 0),
        (48000, 256, 3, 0),
        (32000, 448, 2, 0),
        (44100, 320, 1, 1),
        (48000, 384, 1, 3),
        (24000, 128, 0, 0),
        (16000, 64, 3, 0),
    ] {
        let st = mode != 3;
        let same = mode == 1;
        let input = signal(rate, if st { 2 } else { 1 }, 0.4, 0.5, same);
        let cfg = Cfg {
            ext,
            ..Cfg::l1(rate, kbps, mode)
        };
        check(
            &format!("L1 {rate} {kbps}k mode {mode} ext {ext}"),
            cfg,
            &input,
            20.0,
        );
    }
}

/// CRC-protected frames carry the same audio as unprotected ones: the decoder
/// skips the CRC word and must produce bit-identical output.
#[test]
fn crc_frames_decode_like_plain_frames() {
    for layer in [Layer::I, Layer::II] {
        let input = signal(48000, 2, 0.3, 0.7, false);
        let mut cfg = Cfg::l2(48000, 384, 0);
        cfg.layer = layer;
        let plain = decode(&encode(&input, cfg));
        cfg.crc = true;
        let protected = encode(&input, cfg);
        assert_eq!(protected[1] & 1, 0, "protection bit clear");
        let crc = decode(&protected);
        assert_eq!(crc.frames, plain.frames);
        // The CRC costs 16 bits of the budget, so the allocation may differ;
        // the decode must still be close, and as good as the plain stream.
        let s = snr(&input, &crc.samples, 2);
        assert!(s.iter().all(|&v| v > 25.0), "{layer:?} CRC SNR {s:?}");
    }
    // Same payload, protection on or off: strip the CRC from a protected
    // stream frame by frame (keeping the frame size) and the decode is equal.
    let input = signal(44100, 2, 0.3, 0.7, false);
    let cfg = Cfg {
        crc: true,
        ..Cfg::l2(44100, 192, 0)
    };
    let protected = encode(&input, cfg);
    let mut stripped = Vec::new();
    let mut pos = 0;
    while pos + 4 <= protected.len() {
        let h = Layer12Header::parse(protected[pos..pos + 4].try_into().unwrap()).unwrap();
        let size = h.frame_size().unwrap();
        let mut f = protected[pos..pos + size].to_vec();
        f[1] |= 1; // protection_bit = 1: no CRC
        f.drain(4..6);
        f.extend_from_slice(&[0, 0]);
        stripped.extend_from_slice(&f);
        pos += size;
    }
    assert!(bits_eq(
        &decode(&protected).samples,
        &decode(&stripped).samples
    ));
}

/// Free format: bitrate index 0, frame size found by the distance to the next
/// header. Same frames as a table-bitrate stream, so the decode is identical
/// (the derived bitrate picks the same Layer II table).
#[test]
fn free_format_decodes_like_table_bitrate() {
    for (layer, rate, kbps, mode) in [
        (Layer::II, 44100, 192, 0), // padded frames, table B.2b
        (Layer::II, 32000, 64, 3),
        (Layer::II, 22050, 64, 0), // LSF
        (Layer::I, 44100, 384, 0),
    ] {
        let mut cfg = Cfg::l2(rate, kbps, mode);
        cfg.layer = layer;
        let input = signal(rate, cfg.channels(), 0.5, 0.7, false);
        let table = decode(&encode(&input, cfg));
        cfg.free = true;
        let stream = encode(&input, cfg);
        let free = decode(&stream);
        assert_eq!(free.frames, table.frames, "{layer:?} {rate} {kbps}");
        assert!(
            bits_eq(&free.samples, &table.samples),
            "{layer:?} {rate} {kbps}"
        );
        for chunk in [1, 333] {
            assert!(bits_eq(
                &decode_chunked(&stream, chunk).samples,
                &table.samples
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

fn xorshift(seed: u32) -> impl FnMut() -> u32 {
    let mut x = seed;
    move || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    }
}

fn stream_l2() -> (Vec<u8>, usize) {
    let input = signal(48000, 2, 0.5, 0.7, false);
    let s = encode(&input, Cfg::l2(48000, 256, 0)); // 768-byte frames
    (s, 768)
}

/// Garbage between and after frames: the garbage is skipped, every real
/// frame is decoded, and the decode is bit-identical to the clean stream's.
/// Garbage BEFORE the first frame: Layer III-shaped false syncs in it decode as
/// Layer III used to (that path is unchanged), but every real frame after it
/// still decodes bit-identically.
#[test]
fn garbage_is_skipped_and_decoding_resumes() {
    let (clean, size) = stream_l2();
    let frames = clean.len() / size;
    let reference = decode(&clean);
    let per_frame = 1152 * 2;
    let mut rnd = xorshift(0x1234_5678);
    // Sync-heavy garbage: a third of the bytes 0xFF.
    let mut garbage = |n: usize| -> Vec<u8> {
        (0..n)
            .map(|_| {
                let v = rnd();
                if v % 3 == 0 {
                    0xFF
                } else {
                    (v >> 8) as u8
                }
            })
            .collect()
    };
    let lead = garbage(5000);
    let mid = garbage(3000);
    let tail = garbage(2000);
    let k = 10;
    let mut file = clean[..k * size].to_vec();
    file.extend_from_slice(&mid);
    file.extend_from_slice(&clean[k * size..]);
    file.extend_from_slice(&tail);
    for chunk in [usize::MAX, 4096, 77] {
        let d = decode_chunked(&file, chunk);
        assert_eq!(
            d.frames, frames,
            "chunk {chunk}: every real frame, no junk frames"
        );
        assert!(
            bits_eq(&d.samples, &reference.samples),
            "chunk {chunk}: decode equals clean"
        );
        let mut led = lead.clone();
        led.extend_from_slice(&file);
        let d = decode_chunked(&led, chunk);
        let tail = &d.samples[d.samples.len().saturating_sub(reference.samples.len())..];
        assert!(
            bits_eq(tail, &reference.samples),
            "chunk {chunk}: lead garbage"
        );
    }
    // Damage inside a frame: overwrite frame k's payload with garbage.
    let mut hurt = clean.clone();
    let junk = garbage(size - 8);
    hurt[k * size + 8..(k + 1) * size].copy_from_slice(&junk);
    let d = decode(&hurt);
    assert_eq!(d.frames, frames);
    let hit = k * per_frame..(k + 2) * per_frame;
    for (i, (a, b)) in d.samples.iter().zip(&reference.samples).enumerate() {
        if !hit.contains(&i) {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "sample {i} outside the damaged frame"
            );
        }
    }
    // A damaged HEADER: that frame is lost, the rest resync.
    let mut hurt = clean.clone();
    hurt[k * size + 2] = 0xFF; // bitrate index 15: reserved
    let d = decode(&hurt);
    assert_eq!(d.frames, frames - 1);
}

/// Truncation at every byte of a short stream, chunked and whole: never a
/// panic, never more frames than fit, and the frames before the cut decode as
/// in the full stream.
#[test]
fn truncation_never_panics() {
    let input = signal(32000, 1, 0.2, 1.0, false);
    for cfg in [Cfg::l2(32000, 64, 3), Cfg::l1(32000, 128, 3), {
        let mut c = Cfg::l2(32000, 64, 3);
        c.free = true;
        c
    }] {
        let s = encode(&input, cfg);
        let full = decode(&s);
        let size = s.len() / full.frames;
        for cut in 0..s.len() {
            let d = decode(&s[..cut]);
            assert!(d.frames <= cut / size + 1, "cut {cut}");
            let n = d.samples.len().min(full.samples.len());
            let whole = cut / size;
            let spf = if cfg.layer == Layer::II { 1152 } else { 384 };
            let check = n.min(whole.saturating_sub(1) * spf);
            assert!(
                bits_eq(&d.samples[..check], &full.samples[..check]),
                "cut {cut}"
            );
        }
        let _ = decode_chunked(&s[..s.len() - 3], 5);
    }
}

/// Random bit flips and random bytes: no panic, output always finite.
#[test]
fn hostile_bytes_never_panic() {
    let (clean, _) = stream_l2();
    let mut rnd = xorshift(0x0BAD_F00D);
    for trial in 0..200 {
        let mut s = clean.clone();
        for _ in 0..=(trial % 40) {
            let at = rnd() as usize % s.len();
            s[at] ^= 1 << (rnd() % 8);
        }
        let d = decode_chunked(
            &s,
            if trial % 2 == 0 {
                usize::MAX
            } else {
                1 + trial
            },
        );
        assert!(d.samples.iter().all(|v| v.is_finite()), "trial {trial}");
        let _ = decode_pipelined(&s);
    }
    // Pure noise with sync bytes and every Layer I/II header shape.
    for trial in 0..64u32 {
        let mut s: Vec<u8> = (0..20_000)
            .map(|_| {
                let v = rnd();
                if v % 5 == 0 {
                    0xFF
                } else {
                    (v >> 13) as u8
                }
            })
            .collect();
        for k in 0..200 {
            let at = (rnd() as usize) % (s.len() - 4);
            s[at] = 0xFF;
            s[at + 1] = 0xF0 | ((k as u8) & 0x0F); // any version/layer/CRC
        }
        let d = decode_chunked(&s, 1 + trial as usize * 97);
        assert!(
            d.samples.iter().all(|v| v.is_finite()),
            "noise trial {trial}"
        );
        let _ = decode_pipelined(&s);
    }
}

/// A Layer III stream with Layer I/II-shaped garbage in it: the garbage does
/// not decode as frames once Layer III frames have been seen.
#[test]
fn layer12_garbage_in_layer3_stream_is_not_decoded() {
    let pcm: Vec<f32> = (0..44100).map(|i| 0.3 * (i as f32 * 0.05).sin()).collect();
    let mut enc = rusty_mp3::Mp3Encoder::new(rusty_mp3::Mp3EncoderConfig {
        bitrate_kbps: 128,
        vbr_quality: None,
    });
    enc.push_pcm_f32(&pcm, 1, 44100).unwrap();
    enc.finish();
    let mut mp3 = Vec::new();
    while let Ok(p) = enc.next_packet() {
        mp3.extend_from_slice(&p);
    }
    let clean = decode(&mp3);
    // A Layer II header + filler at a frame boundary in the middle.
    let h = Layer12Header::parse([0xFF, 0xFD, 0xA0, 0x04]).unwrap();
    let mut fake = vec![0xFF, 0xFD, 0xA0, 0x04];
    fake.resize(h.frame_size().unwrap() / 3, 0x55);
    // Walk the Layer III frames to the one nearest the middle.
    let mut at = 0;
    while at < mp3.len() / 2 {
        let hb: [u8; 4] = mp3[at..at + 4].try_into().unwrap();
        at += rusty_mp3::header::FrameHeader::parse(hb)
            .unwrap()
            .frame_size();
    }
    let mut file = mp3[..at].to_vec();
    file.extend_from_slice(&fake);
    file.extend_from_slice(&mp3[at..]);
    let d = decode(&file);
    assert!(d.samples.iter().all(|v| v.is_finite()));
    assert_eq!(d.frames, clean.frames, "no Layer II frame invented");
}

// ---------------------------------------------------------------------------
// Corpus writer (opt-in): streams the sox corpus cannot provide
// ---------------------------------------------------------------------------

/// Read a 16-bit PCM WAV (any channel count) as interleaved f32.
fn read_wav(path: &Path) -> Option<(Vec<f32>, usize, u32)> {
    let b = std::fs::read(path).ok()?;
    let (mut pos, mut ch, mut rate) = (12, 0usize, 0u32);
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let len = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().ok()?) as usize;
        let body = b.get(pos + 8..pos + 8 + len)?;
        if id == b"fmt " {
            ch = usize::from(u16::from_le_bytes([body[2], body[3]]));
            rate = u32::from_le_bytes(body[4..8].try_into().ok()?);
        } else if id == b"data" {
            let pcm = body
                .chunks_exact(2)
                .map(|s| f32::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0)
                .collect();
            return Some((pcm, ch, rate));
        }
        pos += 8 + len + (len & 1);
    }
    None
}

/// `MP12_CORPUS_OUT=dir [MP12_SOURCE_WAV=src.wav]`: write Layer I, joint
/// stereo, CRC and free-format streams for oracle comparisons (sox writes
/// neither Layer I nor joint stereo). 44.1 kHz streams use the source WAV
/// when given; the rest use the synthetic test signal.
#[test]
fn write_corpus_files() {
    let Some(dir) = std::env::var_os("MP12_CORPUS_OUT") else {
        return;
    };
    let dir = Path::new(&dir);
    let wav = std::env::var_os("MP12_SOURCE_WAV").and_then(|p| read_wav(Path::new(&p)));
    let source = |rate: u32, channels: usize| -> Vec<f32> {
        match &wav {
            Some((pcm, 2, 44100)) if rate == 44100 => {
                if channels == 2 {
                    pcm.clone()
                } else {
                    pcm.chunks_exact(2).map(|s| 0.5 * (s[0] + s[1])).collect()
                }
            }
            _ => signal(rate, channels, 3.0, 0.7, false),
        }
    };
    let joint = |cfg: Cfg, ext: u32| Cfg { ext, ..cfg };
    let crc = |cfg: Cfg| Cfg { crc: true, ..cfg };
    let free = |cfg: Cfg| Cfg { free: true, ..cfg };
    let files: Vec<(&str, Cfg)> = vec![
        ("l1_44k_st_384.mp1", Cfg::l1(44100, 384, 0)),
        ("l1_44k_mono_192.mp1", Cfg::l1(44100, 192, 3)),
        ("l1_48k_mono_256.mp1", Cfg::l1(48000, 256, 3)),
        ("l1_32k_dual_448.mp1", Cfg::l1(32000, 448, 2)),
        ("l1_44k_js_b8_320.mp1", joint(Cfg::l1(44100, 320, 1), 1)),
        (
            "l1_44k_js_b16_crc_256.mp1",
            crc(joint(Cfg::l1(44100, 256, 1), 3)),
        ),
        ("l1_24k_st_128.mp1", Cfg::l1(24000, 128, 0)),
        ("l1_16k_mono_64.mp1", Cfg::l1(16000, 64, 3)),
        ("l2_44k_js_b4_192.mp2", joint(Cfg::l2(44100, 192, 1), 0)),
        ("l2_44k_js_b8_128.mp2", joint(Cfg::l2(44100, 128, 1), 1)),
        (
            "l2_44k_js_b12_crc_160.mp2",
            crc(joint(Cfg::l2(44100, 160, 1), 2)),
        ),
        ("l2_48k_js_b16_256.mp2", joint(Cfg::l2(48000, 256, 1), 3)),
        ("l2_32k_js_b4_64.mp2", joint(Cfg::l2(32000, 64, 1), 0)),
        ("l2_24k_js_b8_96.mp2", joint(Cfg::l2(24000, 96, 1), 1)),
        (
            "l2_22k_js_b4_crc_64.mp2",
            crc(joint(Cfg::l2(22050, 64, 1), 0)),
        ),
        ("l2_16k_js_b8_48.mp2", joint(Cfg::l2(16000, 48, 1), 1)),
        ("l2_48k_dual_crc_384.mp2", crc(Cfg::l2(48000, 384, 2))),
        ("l2_44k_free_192.mp2", free(Cfg::l2(44100, 192, 0))),
        ("l1_44k_free_384.mp1", free(Cfg::l1(44100, 384, 0))),
    ];
    for (name, cfg) in files {
        let stream = encode(&source(cfg.rate, cfg.channels()), cfg);
        std::fs::write(dir.join(name), &stream).unwrap();
        eprintln!("wrote {name}: {} bytes", stream.len());
    }
}

/// `decode_pipelined` must equal the serial decoder on any input (the
/// `pipelined` fuzz target's contract). `pipelined_crash_1.bin` is the
/// fuzzer's find: a Layer III stream with Layer I false syncs near its end,
/// which the serial decoder once took (at `flush`) and the Layer III pipeline
/// never does. Also replays the fuzz seed corpus and mutations of it and of a
/// Layer II stream.
#[test]
fn pipelined_equals_serial_on_damaged_and_mixed_input() {
    fn same(data: &[u8]) -> bool {
        let piped = decode_pipelined(data);
        let mut dec = Mp3Decoder::new();
        dec.push(data);
        dec.flush();
        let mut serial = Vec::new();
        while let Ok(f) = dec.next_frame() {
            serial.push(f);
        }
        piped.len() == serial.len()
            && piped.iter().zip(&serial).all(|(a, b)| {
                a.channels == b.channels
                    && a.sample_rate == b.sample_rate
                    && bits_eq(&a.samples, &b.samples)
            })
    }
    let mut inputs: Vec<Vec<u8>> = vec![
        include_bytes!("data/pipelined_crash_1.bin").to_vec(),
        stream_l2().0,
    ];
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/pipelined");
    if let Ok(dir) = std::fs::read_dir(&corpus) {
        for e in dir.flatten() {
            if let Ok(b) = std::fs::read(e.path()) {
                inputs.push(b);
            }
        }
    }
    let mut rnd = xorshift(0x5EED_1234);
    let seeds = inputs.clone();
    for seed in &seeds {
        if seed.is_empty() {
            continue;
        }
        for _ in 0..60 {
            let mut v = seed.clone();
            for _ in 0..=(rnd() % 16) {
                let at = rnd() as usize % v.len();
                v[at] = match rnd() % 3 {
                    0 => 0xFF,
                    1 => v[at] ^ (1 << (rnd() % 8)),
                    _ => rnd() as u8,
                };
            }
            inputs.push(v);
        }
        // A Layer III stream with a Layer II stream glued on, and the reverse.
        let mut glued = seed.clone();
        glued.extend_from_slice(&stream_l2().0);
        inputs.push(glued);
        let mut glued = stream_l2().0;
        glued.extend_from_slice(seed);
        inputs.push(glued);
    }
    for (i, data) in inputs.iter().enumerate() {
        assert!(
            same(data),
            "input {i} ({} bytes): pipelined differs from serial",
            data.len()
        );
    }
}
