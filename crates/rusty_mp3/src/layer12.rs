//! MPEG-1/2 Audio **Layer I and Layer II** decoding (MP1 / MP2), including the
//! MPEG-2 low sampling rates (16 / 22.05 / 24 kHz).
//!
//! Both layers code 32 polyphase subbands directly -- no Huffman, no IMDCT, no
//! bit reservoir -- so a frame decodes on its own:
//!
//! ```text
//!  header ─▶ [CRC] ─▶ bit allocation ─▶ [scfsi] ─▶ scalefactors ─▶
//!  samples (Layer II: grouped 3/5/9 levels) ─▶ requantize ─▶
//!  polyphase synthesis (the Layer III filterbank) ─▶ PCM
//! ```
//!
//! Layer I carries 12 samples per subband (384 per channel per frame), Layer II
//! three parts of 12 (1152), with its allocation table chosen by bitrate per
//! channel and sample rate (ISO/IEC 11172-3 Tables 3-B.2a-d) or, at the MPEG-2
//! rates, the single LSF table (ISO/IEC 13818-3 Table B.1).
//!
//! **Arithmetic.** Requantization follows FFmpeg's `mpegaudiodec_float`: the
//! subband samples are computed as Q23 fixed-point integers (`l1_unscale`,
//! `l2_unscale_group`, with the float build's multiplier tables), then scaled by
//! exactly 2^-23 into the same synthesis filterbank Layer III uses. Joint-stereo
//! (intensity) bounds, dual channel, CRC-protected frames (the CRC word is
//! skipped, not checked) and free-format frames are supported.
//!
//! The entry points are [`Layer12Header::parse`] and [`Layer12Decode`];
//! [`crate::Mp3Decoder`] frame-syncs a byte stream and dispatches each frame to
//! the layer its header names.

// Parses untrusted bytes: narrowing casts are lint-enforced here (see the crate
// lint policy in Cargo.toml) -- every one is masked, typed, or states its bound.
#![warn(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use crate::bitio::BitReader;
use crate::decode::synthesis;
use crate::frame::{GRANULE_LINES, SUBBANDS, SUBBAND_LINES};
use crate::header::MpegVersion;
use crate::tables::SAMPLE_RATE;
use crate::{Error, Result};

/// Which of the two subband layers a frame uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// Layer I: 384 samples per channel per frame.
    I,
    /// Layer II: 1152 samples per channel per frame (MPEG-1 and MPEG-2 alike).
    II,
}

/// The header `mode` field as Layers I and II read it. In joint stereo the
/// `mode_extension` is the intensity-stereo bound, not Layer III's MS/IS flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer12Mode {
    Stereo,
    /// Intensity stereo: subbands from [`Layer12Header::bound`] up share one
    /// set of samples, scaled by each channel's own scalefactors.
    JointStereo,
    /// Two independent mono programmes.
    DualChannel,
    Mono,
}

/// A parsed Layer I / Layer II frame header.
#[allow(clippy::struct_excessive_bools)] // mirrors the bitstream's one-bit flags
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer12Header {
    pub version: MpegVersion,
    pub layer: Layer,
    pub crc_protected: bool,
    /// The raw 4-bit index; 0 is free format.
    pub bitrate_index: u8,
    /// Bitrate in kbps; 0 for a free-format frame (its size sets the rate).
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    /// The raw 2-bit index (0 = 44.1, 1 = 48, 2 = 32 kHz family).
    pub sample_rate_index: u8,
    pub padding: bool,
    pub mode: Layer12Mode,
    pub mode_extension: u8,
    pub copyright: bool,
    pub original: bool,
    pub emphasis: u8,
}

/// Bitrates (kbps) by index: MPEG-1 Layer I, MPEG-1 Layer II, MPEG-2/2.5
/// Layer I, MPEG-2/2.5 Layer II (ISO/IEC 11172-3 2.4.2.3, 13818-3 2.4.2.3).
const BITRATE_V1_L1: [u32; 16] = [
    0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
];
const BITRATE_V1_L2: [u32; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
];
const BITRATE_V2_L1: [u32; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
];
const BITRATE_V2_L2: [u32; 16] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];

/// The largest free-format frame the stream syncer will search for, in bytes.
/// (Layer I at 448 kbps and 32 kHz is 672 bytes; Layer II at 384 kbps and
/// 16 kHz is 3456.)
pub const MAX_FREE_FORMAT_FRAME: usize = 4096;

impl Layer12Header {
    /// Parse a 4-byte header.
    ///
    /// # Errors
    /// [`Error::InvalidData`] on a bad sync word or a reserved version, bitrate
    /// or sample-rate index; [`Error::Unsupported`] for a Layer III (or
    /// reserved-layer) header, which [`crate::header::FrameHeader`] handles.
    pub fn parse(bytes: [u8; 4]) -> Result<Self> {
        let h = u32::from_be_bytes(bytes);
        if (h >> 21) & 0x7FF != 0x7FF {
            return Err(Error::invalid("mpa header: bad frame sync"));
        }
        let version = match (h >> 19) & 0x3 {
            0b00 => MpegVersion::V2_5,
            0b10 => MpegVersion::V2,
            0b11 => MpegVersion::V1,
            _ => return Err(Error::invalid("mpa header: reserved MPEG version")),
        };
        let layer = match (h >> 17) & 0x3 {
            0b11 => Layer::I,
            0b10 => Layer::II,
            _ => return Err(Error::unsupported("mpa header: not Layer I or II")),
        };
        let bitrate_index = ((h >> 12) & 0xF) as u8; // 4-bit field
        if bitrate_index == 15 {
            return Err(Error::invalid("mpa header: reserved bitrate index"));
        }
        let sample_rate_index = ((h >> 10) & 0x3) as u8; // 2-bit field
        if sample_rate_index == 3 {
            return Err(Error::invalid("mpa header: reserved sample-rate index"));
        }
        let table = match (version, layer) {
            (MpegVersion::V1, Layer::I) => &BITRATE_V1_L1,
            (MpegVersion::V1, Layer::II) => &BITRATE_V1_L2,
            (_, Layer::I) => &BITRATE_V2_L1,
            (_, Layer::II) => &BITRATE_V2_L2,
        };
        let base = SAMPLE_RATE[usize::from(sample_rate_index)];
        let sample_rate = match version {
            MpegVersion::V1 => base,
            MpegVersion::V2 => base / 2,
            MpegVersion::V2_5 => base / 4,
        };
        let mode = match (h >> 6) & 0x3 {
            0b00 => Layer12Mode::Stereo,
            0b01 => Layer12Mode::JointStereo,
            0b10 => Layer12Mode::DualChannel,
            _ => Layer12Mode::Mono,
        };
        Ok(Self {
            version,
            layer,
            crc_protected: (h >> 16) & 1 == 0,
            bitrate_index,
            bitrate_kbps: table[usize::from(bitrate_index)],
            sample_rate,
            sample_rate_index,
            padding: (h >> 9) & 1 == 1,
            mode,
            mode_extension: ((h >> 4) & 0x3) as u8, // 2-bit field
            copyright: (h >> 3) & 1 == 1,
            original: (h >> 2) & 1 == 1,
            emphasis: (h & 0x3) as u8, // 2-bit field
        })
    }

    /// Coded channels: 1 for mono, 2 otherwise.
    #[must_use]
    pub fn channels(&self) -> usize {
        match self.mode {
            Layer12Mode::Mono => 1,
            _ => 2,
        }
    }

    /// Decoded PCM samples per channel per frame: 384 (Layer I) or 1152.
    #[must_use]
    pub fn samples_per_frame(&self) -> usize {
        match self.layer {
            Layer::I => 384,
            Layer::II => 1152,
        }
    }

    /// Bytes per slot -- the padding unit: 4 for Layer I, 1 for Layer II.
    #[must_use]
    pub fn slot_bytes(&self) -> usize {
        match self.layer {
            Layer::I => 4,
            Layer::II => 1,
        }
    }

    /// Bytes the padding bit adds.
    #[must_use]
    pub fn padding_bytes(&self) -> usize {
        if self.padding {
            self.slot_bytes()
        } else {
            0
        }
    }

    /// Whether this is a free-format frame (bitrate index 0).
    #[must_use]
    pub fn is_free_format(&self) -> bool {
        self.bitrate_index == 0
    }

    /// Frame size in bytes for a bitrate `kbps` (header and CRC included):
    /// Layer I `(12 * bitrate / rate + padding) * 4`, Layer II
    /// `144 * bitrate / rate + padding`.
    fn size_at(&self, kbps: u32) -> usize {
        let rate = (self.sample_rate as usize).max(1);
        let bps = kbps as usize * 1000;
        match self.layer {
            Layer::I => (12 * bps / rate + usize::from(self.padding)) * 4,
            Layer::II => 144 * bps / rate + usize::from(self.padding),
        }
    }

    /// Total frame size in bytes, or `None` for a free-format frame (whose size
    /// is the distance to the next header).
    #[must_use]
    pub fn frame_size(&self) -> Option<usize> {
        (!self.is_free_format()).then(|| self.size_at(self.bitrate_kbps))
    }

    /// The smallest distance the free-format search accepts to the next header:
    /// half the frame at the lowest table bitrate, so a sync pattern early in
    /// the payload is not taken for the next frame.
    #[must_use]
    pub fn min_free_format_size(&self) -> usize {
        let table_min = match (self.version, self.layer) {
            (MpegVersion::V1, _) | (_, Layer::I) => 32,
            _ => 8,
        };
        (self.size_at(table_min) / 2).max(8)
    }

    /// The largest free-format frame of this stream, in bytes, padding
    /// included: the frame at the layer's highest table bitrate (Layer I 448,
    /// Layer II 384 kbps; 256 and 160 at the MPEG-2 rates). ISO/IEC 11172-3
    /// does not require a decoder to support free-format bitrates above those,
    /// and FFmpeg supports no free format at all. The rows of an uncompressed
    /// picture confirmed themselves as 706 kbps free-format Layer II (2,307
    /// bytes at 44.1 kHz) and decoded to noise peaking at 7.45.
    #[must_use]
    pub fn max_free_format_size(&self) -> usize {
        let top = match (self.version, self.layer) {
            (MpegVersion::V1, Layer::I) => 448,
            (MpegVersion::V1, Layer::II) => 384,
            (_, Layer::I) => 256,
            (_, Layer::II) => 160,
        };
        self.size_at(top)
    }

    /// The bitrate (kbps) of a frame `frame_len` bytes long: the header's for a
    /// table bitrate, derived from the size for free format.
    ///
    /// The derivation rounds to the nearest kbps: a frame size is the floor of
    /// `144 * bitrate / rate`, so truncating back would read a 192 kbps stream
    /// at 44.1 kHz (626-byte frames) as 191 kbps and pick the wrong Layer II
    /// allocation table.
    #[must_use]
    pub fn effective_bitrate_kbps(&self, frame_len: usize) -> u32 {
        if !self.is_free_format() {
            return self.bitrate_kbps;
        }
        let unpadded = frame_len.saturating_sub(self.padding_bytes());
        let rate = self.sample_rate as usize;
        // bits per frame * rate / samples per frame, in kbps, rounded.
        let (bytes_per_slot_unit, spf) = match self.layer {
            Layer::I => (unpadded / 4 * 4, 384),
            Layer::II => (unpadded, 1152),
        };
        let num = bytes_per_slot_unit * 8 * rate;
        let den = spf * 1000;
        u32::try_from((num + den / 2) / den).unwrap_or(u32::MAX)
    }

    /// First subband of the joint-stereo (intensity) region: `4 * (ext + 1)` in
    /// joint stereo, 32 (no shared subbands) otherwise.
    #[must_use]
    pub fn bound(&self) -> usize {
        match self.mode {
            Layer12Mode::JointStereo => 4 * (usize::from(self.mode_extension) + 1),
            _ => SUBBANDS,
        }
    }

    /// Whether `other` can be the next frame of the same stream: same version,
    /// layer and sample rate, and both free-format or neither.
    #[must_use]
    pub fn same_stream(&self, other: &Self) -> bool {
        self.version == other.version
            && self.layer == other.layer
            && self.sample_rate_index == other.sample_rate_index
            && self.is_free_format() == other.is_free_format()
    }
}

// ---- requantization (FFmpeg `mpegaudiodec_float` arithmetic) -----------------

/// Fractional bits of the fixed-point subband samples.
const FRAC_BITS: u32 = 23;
/// `2^-23`: one fixed-point subband unit in the synthesis filterbank's scale
/// (exact, so the scaling adds no rounding).
const FRAC_SCALE: f32 = 1.0 / (1u32 << FRAC_BITS) as f32;

/// The three scalefactor mantissas `2^(-k/3)`, `k = 0, 1, 2`, as the float build
/// writes them (FFmpeg's literal digits).
const SCALE_MANT: [f64; 3] = [1.0, 0.7937005259, 0.6299605249];

/// Layer I (and Layer II ungrouped) multipliers, `[bits - 2][scale % 3]`:
/// `norm = 2^bits * 2^23 / (2^bits - 1)`, times `2 * 2^(-k/3)` in f32, then
/// truncated to an integer -- FFmpeg's `scale_factor_mult` in its float build
/// (`MULLx(norm, FIXR(m * 2.0), FRAC_BITS)` = `(float)(m * 2.0) * norm`).
// The f64 -> f32 rounding and the f32 -> integer truncation ARE the reference
// arithmetic; the products are below 2^26, far inside i64.
#[allow(clippy::cast_possible_truncation)]
const L1_MULT: [[i64; 3]; 15] = {
    let mut t = [[0i64; 3]; 15];
    let mut i = 0;
    while i < 15 {
        let bits = i + 2;
        let norm = ((1i64 << bits) << FRAC_BITS) / ((1i64 << bits) - 1);
        let mut k = 0;
        while k < 3 {
            t[i][k] = ((SCALE_MANT[k] * 2.0) as f32 * norm as f32) as i64;
            k += 1;
        }
        i += 1;
    }
    t
};

/// Layer II grouped multipliers, `[steps >> 2][scale % 3]` for 3, 5 and 9
/// steps: `round(2^(-k/3) * 4 / steps' * 2^23)` with `steps'` = 3, 5, 9 --
/// FFmpeg's `scale_factor_mult2` (`FIXR_OLD`, computed in double).
// Truncation after `+ 0.5` is the reference's rounding; values are below 2^24.
#[allow(clippy::cast_possible_truncation)]
const L2_MULT: [[i32; 3]; 3] = {
    let v = [4.0 / 3.0, 4.0 / 5.0, 4.0 / 9.0];
    let mut t = [[0i32; 3]; 3];
    let mut i = 0;
    while i < 3 {
        let mut k = 0;
        while k < 3 {
            t[i][k] = (SCALE_MANT[k] * v[i] * (1u32 << FRAC_BITS) as f64 + 0.5) as i32;
            k += 1;
        }
        i += 1;
    }
    t
};

/// Requantize an ungrouped sample: `mant` read in `n + 1` bits, scalefactor
/// index `scale` (0..=63). FFmpeg's `l1_unscale`, in Q23.
#[inline]
fn l1_unscale(n: u32, mant: u32, scale: u8) -> i32 {
    let shift = u32::from(scale / 3) + n; // 1..=36: `n` is 1..=15, `scale / 3` 0..=21
    let mult = L1_MULT[(n as usize).saturating_sub(1).min(14)][usize::from(scale % 3)];
    let val = (i64::from(mant) - (1i64 << n) + 1) * mult;
    let r = (val + (1i64 << (shift - 1))) >> shift;
    // |val| < 2^16 * 2^25, and the shift is at least `n`, so |r| < 2^26.
    #[allow(clippy::cast_possible_truncation)]
    let r = r as i32;
    r
}

/// Requantize one sample of a grouped triple: `mant` in `0..steps` (a little
/// above for an out-of-range codeword, as FFmpeg tolerates), `steps` 3, 5 or 9.
/// FFmpeg's `l2_unscale_group`, in Q23.
#[inline]
fn l2_unscale_group(steps: u32, mant: u32, scale: u8) -> i32 {
    let shift = u32::from(scale / 3); // 0..=21
                                      // `mant` < 16 and `steps / 2` <= 4: the difference fits easily.
    #[allow(clippy::cast_possible_wrap)]
    let level = mant as i32 - (steps >> 1) as i32;
    let val = level * L2_MULT[(steps >> 2) as usize][usize::from(scale % 3)];
    if shift > 0 {
        (val + (1 << (shift - 1))) >> shift
    } else {
        val
    }
}

// ---- Layer II allocation tables ----------------------------------------------

/// Quantization classes, by "qindex": the number of steps and the codeword
/// width. A negative width marks a GROUPED class (three samples in one
/// codeword of that many bits). ISO/IEC 11172-3 Table 3-B.4.
const QUANT_STEPS: [u32; 17] = [
    3, 5, 7, 9, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767, 65535,
];
const QUANT_BITS: [i8; 17] = [-5, -7, 3, -10, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

/// One subband's allocation row: `nbal` (the allocation field width) and the
/// qindex each nonzero allocation value selects (`classes[alloc - 1]`).
struct AllocRow {
    nbal: u32,
    classes: &'static [u8],
}

// The distinct rows of Tables 3-B.2a-d (MPEG-1) and B.1 (MPEG-2 LSF).
/// 3-B.2a/b subbands 0-2.
const ROW_A: AllocRow = AllocRow {
    nbal: 4,
    classes: &[0, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
};
/// 3-B.2a/b subbands 3-10.
const ROW_B: AllocRow = AllocRow {
    nbal: 4,
    classes: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16],
};
/// 3-B.2a/b subbands 11-22.
const ROW_C: AllocRow = AllocRow {
    nbal: 3,
    classes: &[0, 1, 2, 3, 4, 5, 16],
};
/// 3-B.2a/b subbands 23 and up.
const ROW_D: AllocRow = AllocRow {
    nbal: 2,
    classes: &[0, 1, 16],
};
/// 3-B.2c/d subbands 0-1.
const ROW_E: AllocRow = AllocRow {
    nbal: 4,
    classes: &[0, 1, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
};
/// 3-B.2c/d subbands 2 and up; LSF subbands 4-10.
const ROW_F: AllocRow = AllocRow {
    nbal: 3,
    classes: &[0, 1, 3, 4, 5, 6, 7],
};
/// LSF subbands 0-3.
const ROW_G: AllocRow = AllocRow {
    nbal: 4,
    classes: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14],
};
/// LSF subbands 11 and up.
const ROW_H: AllocRow = AllocRow {
    nbal: 2,
    classes: &[0, 1, 3],
};

/// A Layer II allocation table: `sblimit` subbands, each with its row.
struct AllocTable {
    rows: [&'static AllocRow; 30],
    sblimit: usize,
}

const fn table(rows: [&'static AllocRow; 30], sblimit: usize) -> AllocTable {
    AllocTable { rows, sblimit }
}

/// Fill `n` row slots: `[a; i] ++ [b; j] ++ ...` without a const-generic dance.
const fn rows(spans: &[(usize, &'static AllocRow)]) -> [&'static AllocRow; 30] {
    let mut out = [&ROW_D; 30];
    let mut at = 0;
    let mut s = 0;
    while s < spans.len() {
        let (n, row) = spans[s];
        let mut i = 0;
        while i < n && at < 30 {
            out[at] = row;
            at += 1;
            i += 1;
        }
        s += 1;
    }
    out
}

/// Tables 3-B.2a (27 subbands), 3-B.2b (30), 3-B.2c (8), 3-B.2d (12) and the
/// MPEG-2 LSF Table B.1 (30), in FFmpeg's order (`ff_mpa_l2_select_table`).
static ALLOC_TABLES: [AllocTable; 5] = [
    table(
        rows(&[(3, &ROW_A), (8, &ROW_B), (12, &ROW_C), (7, &ROW_D)]),
        27,
    ),
    table(
        rows(&[(3, &ROW_A), (8, &ROW_B), (12, &ROW_C), (7, &ROW_D)]),
        30,
    ),
    table(rows(&[(2, &ROW_E), (28, &ROW_F)]), 8),
    table(rows(&[(2, &ROW_E), (28, &ROW_F)]), 12),
    table(rows(&[(4, &ROW_G), (7, &ROW_F), (19, &ROW_H)]), 30),
];

/// Choose the Layer II allocation table (ISO/IEC 11172-3 2.4.3.3.1, as
/// FFmpeg's `ff_mpa_l2_select_table` encodes it): MPEG-2/2.5 always use the
/// LSF table; MPEG-1 picks by bitrate per channel and sample rate.
fn select_table(bitrate_kbps: u32, channels: usize, sample_rate: u32, lsf: bool) -> usize {
    if lsf {
        return 4;
    }
    let ch_bitrate = bitrate_kbps / u32::try_from(channels.max(1)).unwrap_or(1);
    if (sample_rate == 48_000 && ch_bitrate >= 56) || (56..=80).contains(&ch_bitrate) {
        0
    } else if sample_rate != 48_000 && ch_bitrate >= 96 {
        1
    } else if sample_rate != 32_000 && ch_bitrate <= 48 {
        2
    } else {
        3
    }
}

// ---- the decoder ---------------------------------------------------------------

/// Subband samples of one frame, `[channel][block][subband]` (Q23 integers
/// already scaled by 2^-23): 12 blocks for Layer I, 36 for Layer II.
type SbSamples = [[[f32; SUBBANDS]; 36]; 2];

/// Persistent Layer I/II decoder state: the per-channel synthesis FIFO.
///
/// A frame depends on its predecessors only through this filterbank memory, so
/// a corrupt frame costs that frame and nothing else.
#[derive(Clone)]
pub struct Layer12Decode {
    fifo: [[f32; 1024]; 2],
    /// A frame has decoded (passed [`try_decode_frame`](Self::try_decode_frame)):
    /// this decoder is reading a real stream.
    decoded: bool,
}

impl Default for Layer12Decode {
    fn default() -> Self {
        Self {
            fifo: [[0.0; 1024]; 2],
            decoded: false,
        }
    }
}

impl Layer12Decode {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one whole frame (`frame` starts at the 4 header bytes and runs to
    /// the frame's end) into interleaved PCM: `samples_per_frame() *
    /// channels()` floats. For a free-format frame the bitrate (which picks the
    /// Layer II table) comes from `frame.len()`.
    ///
    /// A frame [`try_decode_frame`](Self::try_decode_frame) refuses (and the
    /// filterbank memory is cleared), so damaged input is never louder than
    /// the stream:
    ///
    /// * once this decoder has decoded a frame, decodes to SILENCE of the
    ///   frame's length, standing in for the damaged frame of a real stream;
    /// * before that, decodes to NOTHING (an empty `Vec`): bytes of which no
    ///   frame has decoded are not a stream, and a "decode" of them that is
    ///   silence from end to end would pass them off as one.
    ///
    /// Call that method to tell a refused frame from real silence and drop it.
    #[must_use]
    pub fn decode_frame(&mut self, header: &Layer12Header, frame: &[u8]) -> Vec<f32> {
        match self.try_decode_frame(header, frame) {
            Ok(pcm) => pcm,
            Err(_) if self.decoded => vec![0f32; header.samples_per_frame() * header.channels()],
            Err(_) => Vec::new(),
        }
    }

    /// [`decode_frame`](Self::decode_frame), refusing a frame that cannot be
    /// what an encoder wrote:
    ///
    /// * its allocation asks for more bits than the frame holds (the check
    ///   minimp3 makes: `bs_frame->pos > bs_frame->limit`). A damaged
    ///   allocation is the usual cause, and so is a frame cut short; the
    ///   missing bits used to read as zeros, which requantize to the most
    ///   negative level of every allocated subband -- full-scale noise;
    /// * a Layer I allocation of 15, which ISO/IEC 11172-3 2.4.2.5 forbids;
    /// * a decoded sample beyond [`MAX_OUTPUT_AMPLITUDE`](crate::MAX_OUTPUT_AMPLITUDE)
    ///   (or not finite): the backstop for damage that is still valid syntax.
    ///
    /// A refused frame also clears the filterbank memory, as minimp3 restarts
    /// its decoder, so nothing of it leaks into the next frame. A frame that
    /// passes decodes exactly as it always did.
    ///
    /// # Errors
    /// [`Error::InvalidData`] for a frame refused as above.
    pub fn try_decode_frame(&mut self, header: &Layer12Header, frame: &[u8]) -> Result<Vec<f32>> {
        let start = 4 + if header.crc_protected { 2 } else { 0 };
        let payload = frame.get(start..).unwrap_or(&[]);
        let mut br = BitReader::new(payload);
        let channels = header.channels();
        let mut sb: SbSamples = [[[0.0; SUBBANDS]; 36]; 2];
        let valid = match header.layer {
            Layer::I => read_layer1(&mut br, header, &mut sb),
            Layer::II => {
                let kbps = header.effective_bitrate_kbps(frame.len());
                read_layer2(&mut br, header, kbps, &mut sb);
                true
            }
        };
        if !valid {
            self.reset();
            return Err(Error::invalid("mpa frame: forbidden Layer I allocation"));
        }
        if br.bit_pos() > payload.len() * 8 {
            self.reset();
            return Err(Error::invalid(
                "mpa frame: allocation asks for more bits than the frame holds",
            ));
        }
        let mut pcm = vec![0f32; header.samples_per_frame() * channels];
        match header.layer {
            Layer::I => {
                for ch in 0..channels {
                    synthesis::polyphase_blocks(
                        &sb[ch][..12],
                        &mut self.fifo[ch],
                        &mut pcm[ch..],
                        channels,
                    );
                }
            }
            Layer::II => {
                // Two 18-pass granules through the Layer III filterbank entries
                // (and their SIMD kernels), subband-major as they expect.
                for g in 0..2 {
                    let mut time = [[0f32; GRANULE_LINES]; 2];
                    for ch in 0..channels {
                        for v in 0..SUBBAND_LINES {
                            let block = &sb[ch][g * SUBBAND_LINES + v];
                            for k in 0..SUBBANDS {
                                time[ch][k * SUBBAND_LINES + v] = block[k];
                            }
                        }
                    }
                    let out = &mut pcm[g * GRANULE_LINES * channels..];
                    if channels == 2 {
                        let [t0, t1] = &time;
                        let [f0, f1] = &mut self.fifo;
                        if let Some(o) = out.first_chunk_mut::<{ 2 * GRANULE_LINES }>() {
                            synthesis::polyphase_stereo([t0, t1], [f0, f1], o);
                        }
                    } else {
                        let p = synthesis::polyphase(&time[0], &mut self.fifo[0]);
                        if let Some(o) = out.first_chunk_mut::<GRANULE_LINES>() {
                            *o = p;
                        }
                    }
                }
            }
        }
        if !crate::within_output_bound(&pcm) {
            self.reset();
            return Err(Error::invalid(
                "mpa frame: decodes beyond MAX_OUTPUT_AMPLITUDE",
            ));
        }
        self.decoded = true;
        Ok(pcm)
    }

    /// Clear the filterbank memory (after a refused frame, or a seek). Whether
    /// a frame has decoded yet ([`decode_frame`](Self::decode_frame)) is kept:
    /// it is still the same stream.
    pub fn reset(&mut self) {
        self.fifo = [[0.0; 1024]; 2];
    }
}

/// Layer I side information and samples (ISO/IEC 11172-3 2.4.1.5 / 2.4.3.2).
/// `false`: the frame carries the forbidden allocation 15 (nothing is decoded).
fn read_layer1(br: &mut BitReader<'_>, header: &Layer12Header, sb: &mut SbSamples) -> bool {
    let channels = header.channels();
    let bound = header.bound().min(SUBBANDS);
    let mut alloc = [[0u8; SUBBANDS]; 2];
    let mut scale = [[0u8; SUBBANDS]; 2];
    for i in 0..SUBBANDS {
        if i < bound {
            for a in alloc.iter_mut().take(channels) {
                a[i] = br.read_u8(4);
            }
        } else {
            let v = br.read_u8(4);
            alloc[0][i] = v;
            alloc[1][i] = v;
        }
    }
    // "1111" is forbidden (2.4.2.5): no encoder writes it, damage does.
    if alloc.iter().flatten().any(|&a| a == 15) {
        return false;
    }
    for i in 0..SUBBANDS {
        for ch in 0..channels {
            if alloc[ch][i] != 0 {
                scale[ch][i] = br.read_u8(6);
            }
        }
    }
    for j in 0..12 {
        for i in 0..SUBBANDS {
            if i < bound {
                for ch in 0..channels {
                    let n = u32::from(alloc[ch][i]);
                    sb[ch][j][i] = if n == 0 {
                        0.0
                    } else {
                        let mant = br.read(n + 1);
                        l1_unscale(n, mant, scale[ch][i]) as f32 * FRAC_SCALE
                    };
                }
            } else {
                // Intensity region: one sample, each channel's own scalefactor.
                let n = u32::from(alloc[0][i]);
                if n == 0 {
                    sb[0][j][i] = 0.0;
                    sb[1][j][i] = 0.0;
                } else {
                    let mant = br.read(n + 1);
                    sb[0][j][i] = l1_unscale(n, mant, scale[0][i]) as f32 * FRAC_SCALE;
                    sb[1][j][i] = l1_unscale(n, mant, scale[1][i]) as f32 * FRAC_SCALE;
                }
            }
        }
    }
    true
}

/// Read and requantize one Layer II sample triple of qindex `q` for the given
/// scalefactors (one per channel that shares the codeword).
fn read_triple(br: &mut BitReader<'_>, q: usize, scales: &[u8], out: &mut [[f32; 3]; 2]) {
    let steps = QUANT_STEPS[q];
    let bits = QUANT_BITS[q];
    if bits < 0 {
        // Grouped: one codeword, three base-`steps` digits, least significant
        // first. An out-of-range codeword leaves a top digit >= steps, which is
        // requantized as is (FFmpeg's division tables do the same).
        let mut v = br.read(u32::from(bits.unsigned_abs()));
        let mut mant = [0u32; 3];
        mant[0] = v % steps;
        v /= steps;
        mant[1] = v % steps;
        mant[2] = v / steps;
        for (o, &s) in out.iter_mut().zip(scales) {
            for m in 0..3 {
                o[m] = l2_unscale_group(steps, mant[m], s) as f32 * FRAC_SCALE;
            }
        }
    } else {
        let bits = u32::from(bits.unsigned_abs());
        for m in 0..3 {
            let mant = br.read(bits);
            for (o, &s) in out.iter_mut().zip(scales) {
                o[m] = l1_unscale(bits - 1, mant, s) as f32 * FRAC_SCALE;
            }
        }
    }
}

/// Layer II side information and samples (ISO/IEC 11172-3 2.4.1.6 / 2.4.3.3).
fn read_layer2(br: &mut BitReader<'_>, header: &Layer12Header, kbps: u32, sb: &mut SbSamples) {
    let channels = header.channels();
    let lsf = header.version != MpegVersion::V1;
    let table = &ALLOC_TABLES[select_table(kbps, channels, header.sample_rate, lsf)];
    let sblimit = table.sblimit;
    let bound = header.bound().min(sblimit);

    // Bit allocation.
    let mut alloc = [[0u8; SUBBANDS]; 2];
    for i in 0..sblimit {
        let nbal = table.rows[i].nbal;
        if i < bound {
            for a in alloc.iter_mut().take(channels) {
                a[i] = br.read_u8(nbal);
            }
        } else {
            let v = br.read_u8(nbal);
            alloc[0][i] = v;
            alloc[1][i] = v;
        }
    }
    // Scalefactor selection information.
    let mut scfsi = [[0u8; SUBBANDS]; 2];
    for i in 0..sblimit {
        for ch in 0..channels {
            if alloc[ch][i] != 0 {
                scfsi[ch][i] = br.read_u8(2);
            }
        }
    }
    // Scalefactors: three per subband (one per 12-sample part), some shared.
    let mut scale = [[[0u8; 3]; SUBBANDS]; 2];
    for i in 0..sblimit {
        for ch in 0..channels {
            if alloc[ch][i] == 0 {
                continue;
            }
            let s = &mut scale[ch][i];
            match scfsi[ch][i] {
                0 => {
                    s[0] = br.read_u8(6);
                    s[1] = br.read_u8(6);
                    s[2] = br.read_u8(6);
                }
                1 => {
                    s[0] = br.read_u8(6);
                    s[1] = s[0];
                    s[2] = br.read_u8(6);
                }
                2 => {
                    s[0] = br.read_u8(6);
                    s[1] = s[0];
                    s[2] = s[0];
                }
                _ => {
                    s[0] = br.read_u8(6);
                    s[2] = br.read_u8(6);
                    s[1] = s[2];
                }
            }
        }
    }
    // Samples: 3 parts x 4 granules x 3 samples; subbands above sblimit stay 0.
    for part in 0..3 {
        for gr in 0..4 {
            let block = part * 12 + gr * 3;
            for i in 0..sblimit {
                let row = table.rows[i];
                let mut tri = [[0f32; 3]; 2];
                if i < bound {
                    for ch in 0..channels {
                        let b = usize::from(alloc[ch][i]);
                        if let Some(&q) = b.checked_sub(1).and_then(|k| row.classes.get(k)) {
                            let mut one = [[0f32; 3]; 2];
                            read_triple(br, usize::from(q), &[scale[ch][i][part]], &mut one);
                            tri[ch] = one[0];
                        }
                    }
                } else {
                    let b = usize::from(alloc[0][i]);
                    if let Some(&q) = b.checked_sub(1).and_then(|k| row.classes.get(k)) {
                        let scales = [scale[0][i][part], scale[1][i][part]];
                        read_triple(br, usize::from(q), &scales, &mut tri);
                    }
                }
                for ch in 0..channels {
                    for m in 0..3 {
                        sb[ch][block + m][i] = tri[ch][m];
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_parses_layer2_and_sizes_frames() {
        // MPEG-1 Layer II, 192 kbps, 44.1 kHz, stereo, no CRC (sox's default).
        let h = Layer12Header::parse([0xFF, 0xFD, 0xA0, 0x04]).unwrap();
        assert_eq!(h.layer, Layer::II);
        assert_eq!(h.version, MpegVersion::V1);
        assert_eq!(h.bitrate_kbps, 192);
        assert_eq!(h.sample_rate, 44100);
        assert_eq!(h.mode, Layer12Mode::Stereo);
        assert!(!h.crc_protected);
        assert_eq!(h.frame_size(), Some(626)); // 144 * 192000 / 44100
        assert_eq!(h.samples_per_frame(), 1152);
        // MPEG-1 Layer I, 384 kbps, 44.1 kHz, padded: (12*384000/44100 + 1) * 4.
        let h = Layer12Header::parse([0xFF, 0xFF, 0xC2, 0x00]).unwrap();
        assert_eq!(h.layer, Layer::I);
        assert_eq!(h.frame_size(), Some((104 + 1) * 4));
        assert_eq!(h.samples_per_frame(), 384);
        // MPEG-2 Layer II, 24 kHz, 96 kbps: 144 * 96000 / 24000 = 576.
        let h = Layer12Header::parse([0xFF, 0xF5, 0xA4, 0xC0]).unwrap();
        assert_eq!((h.version, h.sample_rate), (MpegVersion::V2, 24000));
        assert_eq!(h.frame_size(), Some(576));
        assert_eq!(h.channels(), 1);
        // Layer III and the reserved layer are not ours; reserved fields fail.
        assert!(Layer12Header::parse([0xFF, 0xFB, 0x90, 0x00]).is_err());
        assert!(Layer12Header::parse([0xFF, 0xF9, 0x90, 0x00]).is_err());
        assert!(Layer12Header::parse([0xFF, 0xFD, 0xF0, 0x00]).is_err());
        assert!(Layer12Header::parse([0xFF, 0xFD, 0x0C, 0x00]).is_err());
        // Free format: no table size.
        let h = Layer12Header::parse([0xFF, 0xFD, 0x00, 0x00]).unwrap();
        assert!(h.is_free_format() && h.frame_size().is_none());
    }

    #[test]
    fn table_selection_matches_iso() {
        // (kbps, channels, rate) -> table
        assert_eq!(select_table(192, 2, 44100, false), 1); // 96/ch
        assert_eq!(select_table(128, 2, 44100, false), 0); // 64/ch
        assert_eq!(select_table(64, 2, 44100, false), 2); // 32/ch
        assert_eq!(select_table(64, 2, 32000, false), 3);
        assert_eq!(select_table(48, 1, 32000, false), 3);
        assert_eq!(select_table(256, 2, 48000, false), 0); // 128/ch at 48k
        assert_eq!(select_table(160, 1, 44100, false), 1);
        assert_eq!(select_table(96, 1, 48000, false), 0);
        assert_eq!(select_table(32, 2, 24000, true), 4);
        for (t, want) in ALLOC_TABLES.iter().zip([27, 30, 8, 12, 30]) {
            assert_eq!(t.sblimit, want);
            for r in &t.rows[..t.sblimit] {
                assert_eq!(r.classes.len(), (1 << r.nbal) - 1);
            }
        }
    }

    #[test]
    fn multiplier_tables_match_ffmpeg_float_build() {
        // Spot values: 3-step grouping at k = 0 is round(4/3 * 2^23).
        assert_eq!(L2_MULT[0][0], 11184811);
        assert_eq!(L2_MULT[2][0], 3728270);
        // Layer I, 2-bit samples: norm = 4 * 2^23 / 3 = 11184810, times 2.0.
        assert_eq!(L1_MULT[0][0], 22369620);
        // Full scale: the largest 16-bit mantissa at scalefactor 0 is 2.0
        // (2^24 in Q23), to within the multiplier's rounding.
        let top = l1_unscale(15, 0xFFFF, 0);
        assert!((top - (1 << 24)).abs() < 1024, "{top}");
        // A mid-tread zero stays zero.
        assert_eq!(l1_unscale(3, 7, 10), 0);
        assert_eq!(l2_unscale_group(9, 4, 0), 0);
    }
}
