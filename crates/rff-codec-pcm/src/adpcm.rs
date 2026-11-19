//! 4-bit ADPCM decoders: Microsoft ADPCM, IMA ADPCM as WAV stores it, and
//! IMA ADPCM as QuickTime stores it (`ima4`). All decode to interleaved s16.
//!
//! Each follows FFmpeg's `adpcm.c` arithmetic exactly, so output can be
//! compared sample for sample:
//! - MS: `adpcm_ms_expand_nibble`. FFmpeg stores the coefficients divided by
//!   4 and divides the prediction by 64; this keeps the full coefficients (so
//!   a file's own table works) and divides by 256, truncating, which gives the
//!   same value for every standard pair.
//! - IMA WAV: `adpcm_ima_expand_nibble(.., 3)`, the multiplicative form
//!   `((2 * delta + 1) * step) >> 3`.
//! - IMA QT: `adpcm_ima_qt_expand_nibble`, the bitwise form of the IMA spec,
//!   with the per-channel state carried between packets.
//!
//! Damaged input is decoded rather than refused: a short final block yields
//! the samples its bytes hold, and out-of-range header fields (a predictor
//! index past the table, a step index past 88) are clamped. A block too short
//! to hold its own header yields no samples.

/// IMA step sizes (89 entries).
const STEP: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

/// IMA step-index adjustment per nibble (the sign bit is ignored).
const INDEX: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

/// MS ADPCM delta adaptation per nibble.
const MS_ADAPT: [i32; 16] = [
    230, 230, 230, 230, 307, 409, 512, 614, 768, 614, 512, 409, 307, 230, 230, 230,
];

/// The seven standard MS ADPCM predictor coefficient pairs.
pub(crate) const MS_COEFS: [(i32, i32); 7] = [
    (256, 0),
    (512, -256),
    (0, 0),
    (192, 64),
    (240, 0),
    (460, -208),
    (392, -232),
];

fn clip16(v: i32) -> i32 {
    v.clamp(-32768, 32767)
}

fn le16(b: &[u8], at: usize) -> i32 {
    i16::from_le_bytes([b[at], b[at + 1]]) as i32
}

fn push(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&(v as i16).to_le_bytes());
}

/// The coefficient table a WAV `fmt ` extension declares (the bytes after
/// `cbSize`: wSamplesPerBlock, wNumCoef, then wNumCoef pairs), or the
/// standard seven when it declares none or is cut short.
pub(crate) fn ms_coefs(extradata: &[u8]) -> Vec<(i32, i32)> {
    if extradata.len() >= 4 {
        let n = u16::from_le_bytes([extradata[2], extradata[3]]) as usize;
        let pairs = (extradata.len() - 4) / 4;
        if n > 0 && pairs >= n.min(256) {
            return (0..n.min(256))
                .map(|i| (le16(extradata, 4 + 4 * i), le16(extradata, 6 + 4 * i)))
                .collect();
        }
    }
    MS_COEFS.to_vec()
}

#[derive(Clone, Copy, Default)]
struct MsChan {
    coef1: i32,
    coef2: i32,
    delta: i32,
    s1: i32,
    s2: i32,
}

impl MsChan {
    fn expand(&mut self, nibble: u8) -> i32 {
        let signed = if nibble & 8 != 0 {
            nibble as i32 - 16
        } else {
            nibble as i32
        };
        // i64: a file's own coefficients can be any i16, and two full-scale
        // products overflow i32.
        let base = (self.s1 as i64 * self.coef1 as i64 + self.s2 as i64 * self.coef2 as i64) / 256;
        let predictor = (base + (signed * self.delta) as i64).clamp(-32768, 32767) as i32;
        self.s2 = self.s1;
        self.s1 = predictor;
        self.delta = ((MS_ADAPT[nibble as usize] * self.delta) >> 8).clamp(16, i32::MAX / 768);
        self.s1
    }
}

/// Decode one MS ADPCM block of `channels` interleaved channels. Returns the
/// s16 bytes and the samples per channel.
///
/// Layout: per channel a predictor index (u8); then per channel the initial
/// delta, sample1 and sample2 (le16 each, grouped by field); sample2 and then
/// sample1 are the first two output samples. Each following byte holds two
/// nibbles, high first, cycling through the channels.
pub(crate) fn decode_ms(block: &[u8], channels: usize, coefs: &[(i32, i32)]) -> (Vec<u8>, usize) {
    let header = 7 * channels;
    if channels == 0 || block.len() < header {
        return (Vec::new(), 0);
    }
    let mut ch = vec![MsChan::default(); channels];
    for (c, st) in ch.iter_mut().enumerate() {
        let p = (block[c] as usize).min(coefs.len() - 1);
        (st.coef1, st.coef2) = coefs[p];
        st.delta = le16(block, channels + 2 * c);
        st.s1 = le16(block, 3 * channels + 2 * c);
        st.s2 = le16(block, 5 * channels + 2 * c);
    }
    let body = &block[header..];
    let samples = 2 + body.len() * 2 / channels;
    let mut out = Vec::with_capacity(samples * channels * 2);
    for st in &ch {
        push(&mut out, st.s2);
    }
    for st in &ch {
        push(&mut out, st.s1);
    }
    let nibbles = (samples - 2) * channels;
    let mut c = 0;
    for i in 0..nibbles {
        let byte = body[i / 2];
        let nibble = if i % 2 == 0 { byte >> 4 } else { byte & 0x0F };
        push(&mut out, ch[c].expand(nibble));
        c += 1;
        if c == channels {
            c = 0;
        }
    }
    (out, samples)
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ImaChan {
    predictor: i32,
    index: i32,
}

impl ImaChan {
    /// FFmpeg's `adpcm_ima_expand_nibble(c, nibble, 3)`.
    fn expand_mul(&mut self, nibble: u8) -> i32 {
        let step = STEP[self.index as usize];
        let diff = ((2 * (nibble & 7) as i32 + 1) * step) >> 3;
        let p = if nibble & 8 != 0 {
            self.predictor - diff
        } else {
            self.predictor + diff
        };
        self.predictor = clip16(p);
        self.index = (self.index + INDEX[nibble as usize]).clamp(0, 88);
        self.predictor
    }

    /// FFmpeg's `adpcm_ima_qt_expand_nibble`: the IMA spec's bitwise form.
    fn expand_bits(&mut self, nibble: u8) -> i32 {
        let step = STEP[self.index as usize];
        let mut diff = step >> 3;
        if nibble & 4 != 0 {
            diff += step;
        }
        if nibble & 2 != 0 {
            diff += step >> 1;
        }
        if nibble & 1 != 0 {
            diff += step >> 2;
        }
        let p = if nibble & 8 != 0 {
            self.predictor - diff
        } else {
            self.predictor + diff
        };
        self.predictor = clip16(p);
        self.index = (self.index + INDEX[nibble as usize]).clamp(0, 88);
        self.predictor
    }
}

/// Decode one WAV IMA ADPCM block. Returns s16 bytes and samples per channel.
///
/// Layout: per channel a 4-byte header (predictor le16, step index u8, a
/// reserved byte); the header predictor is the first output sample. Then
/// groups of 8 samples: 4 bytes per channel in channel order, low nibble
/// first.
pub(crate) fn decode_ima_wav(block: &[u8], channels: usize) -> (Vec<u8>, usize) {
    let header = 4 * channels;
    if channels == 0 || block.len() < header {
        return (Vec::new(), 0);
    }
    let mut ch = vec![ImaChan::default(); channels];
    for (c, st) in ch.iter_mut().enumerate() {
        st.predictor = le16(block, 4 * c);
        st.index = (block[4 * c + 2] as i32).min(88);
    }
    let body = &block[header..];
    let groups = body.len() / (4 * channels);
    let samples = 1 + 8 * groups;
    // Decode per channel into a planar scratch, then interleave.
    let mut planar = vec![0i32; samples * channels];
    for (c, st) in ch.iter_mut().enumerate() {
        planar[c * samples] = st.predictor;
        for g in 0..groups {
            let bytes = &body[(g * channels + c) * 4..][..4];
            for (k, &b) in bytes.iter().enumerate() {
                let at = c * samples + 1 + g * 8 + 2 * k;
                planar[at] = st.expand_mul(b & 0x0F);
                planar[at + 1] = st.expand_mul(b >> 4);
            }
        }
    }
    let mut out = Vec::with_capacity(samples * channels * 2);
    for i in 0..samples {
        for c in 0..channels {
            push(&mut out, planar[c * samples + i]);
        }
    }
    (out, samples)
}

/// Bytes of one channel's `ima4` block: a 2-byte header and 32 bytes of
/// nibbles, 64 samples.
pub(crate) const QT_BLOCK: usize = 34;
/// Samples per channel in one `ima4` block.
pub(crate) const QT_SAMPLES: usize = 64;

/// Decode `ima4` data: whole frames of one 34-byte block per channel, in
/// channel order. `state` carries each channel's predictor and step index
/// across packets, as QuickTime's decoder does. Returns s16 bytes and samples
/// per channel; a trailing partial frame is ignored.
pub(crate) fn decode_ima_qt(
    data: &[u8],
    state: &mut Vec<ImaChan>,
    channels: usize,
) -> (Vec<u8>, usize) {
    if channels == 0 {
        return (Vec::new(), 0);
    }
    state.resize(channels, ImaChan::default());
    let frames = data.len() / (QT_BLOCK * channels);
    let samples = frames * QT_SAMPLES;
    let mut planar = vec![0i32; QT_SAMPLES * channels];
    let mut out = Vec::with_capacity(samples * channels * 2);
    for f in 0..frames {
        for (c, st) in state.iter_mut().enumerate() {
            let block = &data[(f * channels + c) * QT_BLOCK..][..QT_BLOCK];
            // Header: the top 9 bits are the predictor, the low 7 the index.
            let head = i16::from_be_bytes([block[0], block[1]]) as i32;
            let index = head & 0x7F;
            let predictor = head & !0x7F;
            // FFmpeg keeps the running predictor when the header's agrees
            // with it to within the 7 bits the header cannot carry.
            if st.index != index || (predictor - st.predictor).abs() > 0x7F {
                st.index = index;
                st.predictor = predictor;
            }
            st.index = st.index.min(88);
            for (k, &b) in block[2..].iter().enumerate() {
                planar[c * QT_SAMPLES + 2 * k] = st.expand_bits(b & 0x0F);
                planar[c * QT_SAMPLES + 2 * k + 1] = st.expand_bits(b >> 4);
            }
        }
        for i in 0..QT_SAMPLES {
            for c in 0..channels {
                push(&mut out, planar[c * QT_SAMPLES + i]);
            }
        }
    }
    (out, samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s16s(b: &[u8]) -> Vec<i16> {
        b.chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    /// A straightforward IMA encoder (the inverse of `expand_mul`), used to
    /// make test blocks whose decode must track the source closely.
    fn ima_encode_nibble(st: &mut ImaChan, x: i32) -> u8 {
        let step = STEP[st.index as usize];
        let d = x - st.predictor;
        let sign = if d < 0 { 8 } else { 0 };
        let mag = ((d.abs() * 4) / step.max(1)).min(7) as u8;
        let nibble = sign | mag;
        st.expand_mul(nibble);
        nibble
    }

    fn sine(n: usize, phase: f64) -> Vec<i32> {
        (0..n)
            .map(|i| (12000.0 * ((i as f64) * 0.05 + phase).sin()) as i32)
            .collect()
    }

    #[test]
    fn ima_wav_block_tracks_its_source() {
        let channels = 2;
        let groups = 127; // 8 header bytes + 127 * 8 = 1024: the usual stereo block, 1017 samples
        let src: Vec<Vec<i32>> = (0..channels)
            .map(|c| sine(1 + 8 * groups, c as f64))
            .collect();
        let mut block = Vec::new();
        let mut enc = vec![ImaChan::default(); channels];
        // What the encoder's own decoder model reconstructs, per channel.
        let mut recon: Vec<Vec<i32>> = vec![Vec::new(); channels];
        for c in 0..channels {
            enc[c].predictor = src[c][0];
            recon[c].push(src[c][0]);
            block.extend_from_slice(&(src[c][0] as i16).to_le_bytes());
            block.extend_from_slice(&[0, 0]);
        }
        for g in 0..groups {
            for c in 0..channels {
                for k in 0..4 {
                    let lo = ima_encode_nibble(&mut enc[c], src[c][1 + g * 8 + 2 * k]);
                    recon[c].push(enc[c].predictor);
                    let hi = ima_encode_nibble(&mut enc[c], src[c][2 + g * 8 + 2 * k]);
                    recon[c].push(enc[c].predictor);
                    block.push(lo | (hi << 4));
                }
            }
        }
        assert_eq!(block.len(), 1024);
        let (out, samples) = decode_ima_wav(&block, channels);
        assert_eq!(samples, 1017);
        let got = s16s(&out);
        // Exact: the decoder reproduces the encoder's reconstruction, which
        // pins the header, the group interleave and the nibble order.
        for i in 0..samples {
            for c in 0..channels {
                assert_eq!(
                    got[i * channels + c] as i32,
                    recon[c][i],
                    "sample {i} ch {c}"
                );
            }
        }
        // And once the step size has adapted, it tracks the source.
        let mut worst = 0;
        for i in 200..samples {
            for c in 0..channels {
                worst = worst.max((got[i * channels + c] as i32 - src[c][i]).abs());
            }
        }
        assert!(worst < 1500, "IMA decode strays {worst} from its source");

        // A truncated block decodes the whole groups it still holds.
        let (_, short) = decode_ima_wav(&block[..8 + 8 * 10 + 3], channels);
        assert_eq!(short, 1 + 8 * 10);
        // A block shorter than its header yields nothing, without panicking.
        assert_eq!(decode_ima_wav(&block[..5], channels).1, 0);
    }

    #[test]
    fn ms_block_header_samples_come_first_and_body_follows_the_spec() {
        // Mono block: predictor 0 (256, 0), delta 16, s1 = 100, s2 = 50.
        let mut block = vec![0u8];
        block.extend_from_slice(&16i16.to_le_bytes());
        block.extend_from_slice(&100i16.to_le_bytes());
        block.extend_from_slice(&50i16.to_le_bytes());
        block.push(0x1F); // nibbles 1 then 0xF (-1)
        let (out, samples) = decode_ms(&block, 1, &MS_COEFS);
        assert_eq!(samples, 4);
        // Hand-computed from adpcm_ms_expand_nibble:
        // n=1: p = 100*256/256 + 1*16 = 116; delta = 230*16>>8 = 14 -> 16.
        // n=-1: p = 116*256/256 - 16 = 100; delta = 230*16>>8 -> 16.
        assert_eq!(s16s(&out), [50, 100, 116, 100]);
    }

    #[test]
    fn ms_stereo_interleaves_nibbles_left_then_right() {
        let mut block = vec![2u8, 2]; // coefficient pair (0, 0): output = nibble * delta
        for v in [32i16, 64, 0, 0, 0, 0] {
            block.extend_from_slice(&v.to_le_bytes());
        }
        block.push(0x21); // left +2, right +1
        let (out, samples) = decode_ms(&block, 2, &MS_COEFS);
        assert_eq!(samples, 3);
        assert_eq!(s16s(&out), [0, 0, 0, 0, 64, 64]);
    }

    #[test]
    fn ms_custom_coefficients_and_clamped_predictor() {
        // An fmt extension declaring a single (128, 0) pair.
        let mut ext = Vec::new();
        ext.extend_from_slice(&500u16.to_le_bytes());
        ext.extend_from_slice(&1u16.to_le_bytes());
        ext.extend_from_slice(&128i16.to_le_bytes());
        ext.extend_from_slice(&0i16.to_le_bytes());
        let coefs = ms_coefs(&ext);
        assert_eq!(coefs, [(128, 0)]);
        // Predictor 9 is past the one-entry table: clamped, not refused.
        let mut block = vec![9u8];
        for v in [16i16, 640, 0] {
            block.extend_from_slice(&v.to_le_bytes());
        }
        block.push(0x00);
        let (out, _) = decode_ms(&block, 1, &coefs);
        assert_eq!(s16s(&out), [0, 640, 320, 160]);
        // A cut-short extension falls back to the standard table.
        assert_eq!(ms_coefs(&ext[..6]), MS_COEFS);
    }

    #[test]
    fn ima_qt_decodes_whole_frames_and_carries_state() {
        // Stereo: two 34-byte blocks per frame, two frames, plus junk.
        let mut data = Vec::new();
        for _ in 0..2 {
            for c in 0..2u8 {
                data.extend_from_slice(&[0x00, 0x10 + c]); // predictor 0, index 16/17
                data.extend((0..32).map(|i| (i as u8).wrapping_mul(37) ^ c));
            }
        }
        data.extend_from_slice(&[1, 2, 3]);
        let mut state = Vec::new();
        let (out, samples) = decode_ima_qt(&data, &mut state, 2);
        assert_eq!(samples, 128);
        assert_eq!(out.len(), 128 * 2 * 2);
        // Decoding frame by frame with carried state gives the same samples.
        let mut state2 = Vec::new();
        let (a, _) = decode_ima_qt(&data[..68], &mut state2, 2);
        let (b, _) = decode_ima_qt(&data[68..136], &mut state2, 2);
        assert_eq!([a, b].concat(), out);
    }

    #[test]
    fn garbage_never_panics() {
        let mut seed = 0x1234_5678u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for len in 0..300 {
            let block: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            for ch in 1..=8 {
                let _ = decode_ms(&block, ch, &MS_COEFS);
                let _ = decode_ima_wav(&block, ch);
                let _ = decode_ima_qt(&block, &mut Vec::new(), ch);
            }
        }
    }
}
