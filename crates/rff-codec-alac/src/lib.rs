//! Apple Lossless (ALAC) audio decoder.
//!
//! Decodes ALAC packets (one frame each, as MP4, CAF and Matroska store them)
//! from the 24-byte `ALACSpecificConfig` magic cookie given as extradata. Bit
//! depths 16, 20, 24 and 32, 1 to 8 channels. Output is interleaved: s16 for
//! 16-bit streams, f32 for deeper ones (exact for 20 and 24 bits).
//!
//! The arithmetic follows FFmpeg's `alac.c` (adaptive Golomb-Rice residuals,
//! the adaptive FIR predictor, stereo decorrelation, shifted low bytes), and
//! channels are reordered from ALAC's layout (C L R ...) to the WAV/FFmpeg
//! order (L R C LFE ...) the same way.
//!
//! A malformed frame is an error for that packet only; the decoder keeps no
//! state between frames, so the next packet decodes normally.

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder};
use rff_core::{AudioFrame, CodecId, Error, Frame, MediaType, Packet, Result, SampleFormat};

/// Register the ALAC decoder into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: CodecId::Alac,
        name: "alac",
        long_name: "ALAC (Apple Lossless Audio Codec)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(AlacDecoder::default())),
        encoder: None,
    });
}

/// The fields of `ALACSpecificConfig` the decoder uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlacConfig {
    /// Samples per channel in a full frame.
    pub frame_length: u32,
    pub bit_depth: u8,
    /// Rice history multiplier (`pb`, 40 by default).
    pub pb: u8,
    /// Initial Rice history (`mb`, 10 by default).
    pub mb: u8,
    /// Rice parameter limit (`kb`, 14 by default).
    pub kb: u8,
    pub channels: u8,
    pub sample_rate: u32,
}

impl AlacConfig {
    /// Parse the 24-byte config at `b`.
    fn at(b: &[u8]) -> Option<AlacConfig> {
        if b.len() < 24 {
            return None;
        }
        let c = AlacConfig {
            frame_length: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            bit_depth: b[5],
            pb: b[6],
            mb: b[7],
            kb: b[8],
            channels: b[9],
            sample_rate: u32::from_be_bytes([b[20], b[21], b[22], b[23]]),
        };
        let ok = matches!(c.bit_depth, 16 | 20 | 24 | 32)
            && (1..=8).contains(&c.channels)
            && (1..=1 << 20).contains(&c.frame_length)
            && (1..=32).contains(&c.kb);
        ok.then_some(c)
    }

    /// Find the config in a magic cookie, whichever wrapping it arrives in:
    /// the bare 24 bytes (MP4 `alac` box payload, Matroska CodecPrivate,
    /// many CAF `kuki`s), the full 36-byte `alac` atom (FFmpeg extradata),
    /// or a CAF cookie that opens with a `frma` atom before the `alac` atom.
    pub fn from_cookie(cookie: &[u8]) -> Option<AlacConfig> {
        // An `alac` atom anywhere in the first few atoms: size, 'alac',
        // version/flags, then the config.
        let mut i = 0;
        while i + 12 <= cookie.len() && i < 64 {
            if &cookie[i + 4..i + 8] == b"alac" {
                if let Some(c) = cookie.get(i + 12..).and_then(AlacConfig::at) {
                    return Some(c);
                }
            }
            let size = u32::from_be_bytes([cookie[i], cookie[i + 1], cookie[i + 2], cookie[i + 3]])
                as usize;
            if size < 8 {
                break;
            }
            i += size;
        }
        AlacConfig::at(cookie).or_else(|| cookie.get(4..).and_then(AlacConfig::at))
    }
}

/// ALAC channel order -> output order (FFmpeg's
/// `ff_alac_channel_layout_offsets`): element `ch` of a `channels`-channel
/// stream lands at output index `OFFSETS[channels - 1][ch]`.
const OFFSETS: [[u8; 8]; 8] = [
    [0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 0, 0, 0, 0, 0, 0],
    [2, 0, 1, 0, 0, 0, 0, 0],
    [2, 0, 1, 3, 0, 0, 0, 0],
    [2, 0, 1, 3, 4, 0, 0, 0],
    [2, 0, 1, 4, 5, 3, 0, 0],
    [2, 0, 1, 4, 5, 6, 3, 0],
    [2, 6, 7, 0, 1, 4, 5, 3],
];

/// MSB-first bit reader over a packet.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize, // in bits
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Bits<'a> {
        Bits { data, pos: 0 }
    }

    fn left(&self) -> isize {
        (self.data.len() * 8) as isize - self.pos as isize
    }

    fn bit(&mut self) -> u32 {
        let byte = self.data.get(self.pos / 8).copied().unwrap_or(0);
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        b as u32
    }

    /// Read `n` (0..=32) bits; past the end reads zeros.
    fn read(&mut self, n: u32) -> u32 {
        let mut v: u64 = 0;
        let mut need = n;
        while need > 0 {
            let byte = self.data.get(self.pos / 8).copied().unwrap_or(0) as u64;
            let off = (self.pos % 8) as u32;
            let take = (8 - off).min(need);
            let bits = (byte >> (8 - off - take)) & ((1 << take) - 1);
            v = (v << take) | bits;
            self.pos += take as usize;
            need -= take;
        }
        v as u32
    }

    fn peek(&self, n: u32) -> u32 {
        let mut c = Bits {
            data: self.data,
            pos: self.pos,
        };
        c.read(n)
    }

    fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    fn sbits(&mut self, n: u32) -> i32 {
        sign_extend(self.read(n) as i32, n)
    }

    /// Count of 1 bits before a 0, at most 9 (FFmpeg's `get_unary_0_9`).
    fn unary_0_9(&mut self) -> u32 {
        let mut n = 0;
        while n < 9 && self.bit() == 1 {
            n += 1;
        }
        n
    }
}

fn sign_extend(v: i32, bits: u32) -> i32 {
    if bits == 0 || bits >= 32 {
        return v;
    }
    let s = 32 - bits;
    (v << s) >> s
}

fn decode_scalar(b: &mut Bits, k: u32, bps: u32) -> u32 {
    let mut x = b.unary_0_9();
    if x > 8 {
        x = b.read(bps);
    } else if k != 1 {
        let extra = b.peek(k);
        x = (x << k).wrapping_sub(x);
        if extra > 1 {
            x = x.wrapping_add(extra - 1);
            b.skip(k);
        } else {
            b.skip(k - 1);
        }
    }
    x
}

fn log2(v: u32) -> u32 {
    if v == 0 {
        0
    } else {
        31 - v.leading_zeros()
    }
}

/// Adaptive Golomb-Rice residuals (FFmpeg's `rice_decompress`).
fn rice_decompress(
    b: &mut Bits,
    out: &mut [i32],
    bps: u32,
    history_mult: u32,
    initial_history: u32,
    limit: u32,
) -> Result<()> {
    let n = out.len();
    let mut history = initial_history;
    let mut sign_modifier = 0u32;
    let mut i = 0;
    while i < n {
        if b.left() <= 0 {
            return Err(Error::invalid("alac: residuals run past the packet"));
        }
        let k = log2((history >> 9) + 3).min(limit);
        let x = decode_scalar(b, k, bps).wrapping_add(sign_modifier);
        sign_modifier = 0;
        out[i] = ((x >> 1) as i32) ^ -((x & 1) as i32);
        if x > 0xffff {
            history = 0xffff;
        } else {
            history = history
                .wrapping_add(x.wrapping_mul(history_mult))
                .wrapping_sub(history.wrapping_mul(history_mult) >> 9);
        }
        if history < 128 && i + 1 < n {
            let k = (7 - log2(history) + ((history + 16) >> 6)).min(limit);
            let block = decode_scalar(b, k, 16) as usize;
            if block > 0 {
                if block >= n - i {
                    return Err(Error::invalid("alac: zero run past the frame"));
                }
                out[i + 1..i + 1 + block].fill(0);
                i += block;
            }
            if block <= 0xffff {
                sign_modifier = 1;
            }
            history = 0;
        }
        i += 1;
    }
    Ok(())
}

fn sign_only(v: i32) -> i32 {
    v.signum()
}

/// The adaptive FIR predictor (FFmpeg's `lpc_prediction`). `coefs` holds the
/// coefficients in stream order reversed (index `order - 1` was read first),
/// and is adapted in place. `order == 31` is the fixed first-order mode.
fn lpc_prediction(
    err: &[i32],
    out: &mut [i32],
    bps: u32,
    coefs: &mut [i16],
    order: usize,
    quant: u32,
) {
    let n = err.len();
    if n == 0 {
        return;
    }
    out[0] = err[0];
    if n <= 1 {
        return;
    }
    if order == 0 {
        out[1..n].copy_from_slice(&err[1..n]);
        return;
    }
    if order == 31 {
        for i in 1..n {
            out[i] = sign_extend(out[i - 1].wrapping_add(err[i]), bps);
        }
        return;
    }
    let mut i = 1;
    while i <= order && i < n {
        out[i] = sign_extend(out[i - 1].wrapping_add(err[i]), bps);
        i += 1;
    }
    while i < n {
        // d = out[i - order - 1]; pred[j] = out[i - order + j].
        let base = i - order - 1;
        let d = out[base];
        let mut val: i32 = 0;
        for j in 0..order {
            val = val.wrapping_add(
                out[base + 1 + j]
                    .wrapping_sub(d)
                    .wrapping_mul(coefs[j] as i32),
            );
        }
        let rounded = ((val as i64 + (1i64 << (quant - 1))) >> quant) as i32;
        let mut error_val = err[i] as u32;
        let v = rounded.wrapping_add(d).wrapping_add(error_val as i32);
        out[i] = sign_extend(v, bps);
        let error_sign = sign_only(error_val as i32);
        if error_sign != 0 {
            let mut j = 0;
            while j < order && (error_val.wrapping_mul(error_sign as u32) as i32) > 0 {
                let mut v = d.wrapping_sub(out[base + 1 + j]);
                let sign = sign_only(v) * error_sign;
                coefs[j] = coefs[j].wrapping_sub(sign as i16);
                v = v.wrapping_mul(sign);
                error_val =
                    error_val.wrapping_sub(((v >> quant) as u32).wrapping_mul(j as u32 + 1));
                j += 1;
            }
        }
        i += 1;
    }
}

/// Decode one ALAC frame into per-channel buffers (output order). Returns
/// the samples per channel.
fn decode_frame(cfg: &AlacConfig, data: &[u8], out: &mut [Vec<i32>]) -> Result<usize> {
    let channels = cfg.channels as usize;
    let mut b = Bits::new(data);
    let mut ch = 0usize;
    let mut nb_samples: Option<usize> = None;
    // Stop once every channel has decoded: bytes after that (damage, or an
    // encoder's padding) are not read as more elements.
    while ch < channels && b.left() >= 3 {
        let element = b.read(3);
        if element == 7 {
            break; // END
        }
        // SCE 0, CPE 1, LFE 3. CCE/DSE/PCE/FIL are not used by ALAC encoders.
        if !matches!(element, 0 | 1 | 3) {
            return Err(Error::unsupported(format!("alac: element type {element}")));
        }
        let elem_ch = if element == 1 { 2 } else { 1 };
        if ch + elem_ch > channels || OFFSETS[channels - 1][ch] as usize + elem_ch > channels {
            return Err(Error::invalid("alac: more channel elements than channels"));
        }
        let at = OFFSETS[channels - 1][ch] as usize;
        let n = decode_element(cfg, &mut b, elem_ch, nb_samples, &mut out[at..at + elem_ch])?;
        nb_samples = Some(n);
        ch += elem_ch;
    }
    if ch < channels {
        return Err(Error::invalid("alac: frame ends before every channel"));
    }
    Ok(nb_samples.unwrap_or(0))
}

fn decode_element(
    cfg: &AlacConfig,
    b: &mut Bits,
    channels: usize,
    expected: Option<usize>,
    out: &mut [Vec<i32>],
) -> Result<usize> {
    b.skip(4); // element instance tag
    b.skip(12); // unused
    let has_size = b.read(1) == 1;
    let extra_bits = b.read(2) * 8;
    let sample_size = cfg.bit_depth as u32;
    let bps = (sample_size + channels as u32 - 1)
        .checked_sub(extra_bits)
        .filter(|&v| (1..=32).contains(&v))
        .ok_or_else(|| Error::invalid("alac: bad shifted-byte count"))?;
    let is_compressed = b.read(1) == 0;
    let n = if has_size {
        b.read(32) as usize
    } else {
        cfg.frame_length as usize
    };
    if n == 0 || n > cfg.frame_length as usize {
        return Err(Error::invalid(format!("alac: {n} samples in a frame")));
    }
    if expected.is_some_and(|e| e != n) {
        return Err(Error::invalid("alac: elements disagree on the frame size"));
    }
    for o in out.iter_mut() {
        o.clear();
        o.resize(n, 0);
    }
    let mut decorr_shift = 0u32;
    let mut decorr_left_weight = 0i32;
    let mut extra = 0u32;
    let mut extra_buf: Vec<Vec<u32>> = Vec::new();
    if is_compressed {
        decorr_shift = b.read(8);
        decorr_left_weight = b.read(8) as i32;
        if channels == 2 && decorr_left_weight != 0 && decorr_shift > 31 {
            return Err(Error::invalid("alac: decorrelation shift past 31"));
        }
        let mut pred_type = [0u32; 2];
        let mut quant = [0u32; 2];
        let mut hist_mult = [0u32; 2];
        let mut order = [0usize; 2];
        let mut coefs = [[0i16; 32]; 2];
        for c in 0..channels {
            pred_type[c] = b.read(4);
            quant[c] = b.read(4);
            hist_mult[c] = b.read(3);
            order[c] = b.read(5) as usize;
            if order[c] >= n || quant[c] == 0 {
                return Err(Error::invalid("alac: bad predictor"));
            }
            for i in (0..order[c]).rev() {
                coefs[c][i] = b.sbits(16) as i16;
            }
        }
        extra = extra_bits;
        if extra > 0 {
            if b.left() < (n * channels * extra as usize) as isize {
                return Err(Error::invalid("alac: shifted bytes run past the packet"));
            }
            extra_buf = vec![vec![0u32; n]; channels];
            for i in 0..n {
                for e in extra_buf.iter_mut() {
                    e[i] = b.read(extra);
                }
            }
        }
        let mut residual = vec![0i32; n];
        for c in 0..channels {
            rice_decompress(
                b,
                &mut residual,
                bps,
                hist_mult[c] * cfg.pb as u32 / 4,
                cfg.mb as u32,
                cfg.kb as u32,
            )?;
            if pred_type[c] == 15 {
                let copy = residual.clone();
                lpc_prediction(&copy, &mut residual, bps, &mut [], 31, 0);
            }
            lpc_prediction(
                &residual,
                &mut out[c],
                bps,
                &mut coefs[c],
                order[c],
                quant[c],
            );
        }
    } else {
        for i in 0..n {
            for o in out.iter_mut() {
                o[i] = b.sbits(sample_size);
            }
        }
    }
    if channels == 2 && decorr_left_weight != 0 {
        let (l, r) = out.split_at_mut(1);
        for i in 0..n {
            let mut a = l[0][i];
            let mut bb = r[0][i];
            a = a.wrapping_sub(bb.wrapping_mul(decorr_left_weight) >> decorr_shift);
            bb = bb.wrapping_add(a);
            l[0][i] = bb;
            r[0][i] = a;
        }
    }
    if extra > 0 {
        for (o, e) in out.iter_mut().zip(&extra_buf) {
            for i in 0..n {
                o[i] = ((o[i] as u32) << extra | e[i]) as i32;
            }
        }
    }
    Ok(n)
}

#[derive(Default)]
struct AlacDecoder {
    config: Option<AlacConfig>,
    sample_rate: u32,
    buffers: Vec<Vec<i32>>,
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for AlacDecoder {
    fn configure(&mut self, params: &CodecParams) -> Result<()> {
        let cfg = AlacConfig::from_cookie(&params.extradata)
            .ok_or_else(|| Error::invalid("alac: no usable magic cookie in extradata"))?;
        self.sample_rate = if params.sample_rate > 0 {
            params.sample_rate
        } else {
            cfg.sample_rate
        };
        self.buffers = vec![Vec::new(); cfg.channels as usize];
        self.config = Some(cfg);
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let cfg = self
            .config
            .ok_or_else(|| Error::invalid("alac: not configured"))?;
        let n = decode_frame(&cfg, &packet.data, &mut self.buffers)?;
        let channels = cfg.channels as usize;
        let (format, data) = if cfg.bit_depth == 16 {
            let mut out = Vec::with_capacity(n * channels * 2);
            for i in 0..n {
                for b in &self.buffers {
                    out.extend_from_slice(&(b[i] as i16).to_le_bytes());
                }
            }
            (SampleFormat::S16, out)
        } else {
            // Left-justify to 32 bits (as FFmpeg's s32 output), then scale.
            let shift = 32 - cfg.bit_depth as u32;
            let mut out = Vec::with_capacity(n * channels * 4);
            for i in 0..n {
                for b in &self.buffers {
                    let v = (b[i] as u32).wrapping_shl(shift) as i32;
                    out.extend_from_slice(&((v as f64 / 2_147_483_648.0) as f32).to_le_bytes());
                }
            }
            (SampleFormat::F32, out)
        };
        self.frame = Some(Frame::Audio(AudioFrame {
            sample_rate: self.sample_rate,
            channels: cfg.channels as u16,
            format,
            planes: vec![data],
            samples: n,
            pts: packet.pts,
        }));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(f) = self.frame.take() {
            return Ok(f);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.eof = true;
    }
}

#[cfg(test)]
mod tests;
