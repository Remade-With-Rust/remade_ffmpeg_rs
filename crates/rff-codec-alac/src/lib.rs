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

/// MSB-first bit reader over a packet. Reads are served from one unaligned
/// big-endian 64-bit load at the current byte, so any read of up to 32 bits
/// is a load, a shift and a mask rather than a walk over bytes. Past the end
/// of the packet it reads zeros.
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

    /// The next 57 or more bits, MSB-aligned (zeros past the end).
    #[inline(always)]
    fn window(&self) -> u64 {
        let at = self.pos >> 3;
        let word = match self.data.get(at..at + 8) {
            Some(w) => u64::from_be_bytes(w.try_into().unwrap()),
            None => self.window_tail(at),
        };
        word << (self.pos & 7)
    }

    /// `window` within 8 bytes of the end, zero-padded.
    #[cold]
    #[inline(never)]
    fn window_tail(&self, at: usize) -> u64 {
        let mut w = [0u8; 8];
        if let Some(rest) = self.data.get(at..) {
            w[..rest.len()].copy_from_slice(rest);
        }
        u64::from_be_bytes(w)
    }

    /// The next `n` (0..=32) bits without consuming them.
    #[inline(always)]
    fn peek(&self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        (self.window() >> (64 - n)) as u32
    }

    /// Read `n` (0..=32) bits; past the end reads zeros.
    #[inline(always)]
    fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    #[inline(always)]
    fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    fn sbits(&mut self, n: u32) -> i32 {
        sign_extend(self.read(n) as i32, n)
    }
}

/// Sign-extend the low `bits` (0..=32) bits of `v`. Every caller passes a
/// width in that range (a sample size, a coded width, 16). There is no
/// test: at 0 and 32 the shift is 0 and `v` comes back unchanged.
#[inline(always)]
fn sign_extend(v: i32, bits: u32) -> i32 {
    debug_assert!(bits <= 32);
    let s = 32u32.wrapping_sub(bits) & 31;
    (v << s) >> s
}

/// One adaptive Golomb-Rice value (FFmpeg's `decode_scalar`): a unary prefix
/// of at most 9, then either `bps` raw bits (prefix 9) or `k` bits of
/// remainder. `k` is at least 1 for every caller. Everything but the escape
/// comes from one window load. Inlined, since it runs once per residual:
/// as a call it cost 8-11% more instructions over the whole decode.
#[inline(always)]
fn decode_scalar(b: &mut Bits, k: u32, bps: u32) -> u32 {
    let w = b.window();
    let x = (!w).leading_zeros().min(9);
    if x > 8 {
        b.skip(9);
        return b.read(bps);
    }
    if k == 1 {
        b.skip(x + 1);
        return x;
    }
    // The remainder starts after the prefix and its 0; x + 1 + k <= 9 + 32
    // bits, inside the window's 57.
    let extra = ((w << (x + 1)) >> (64 - k)) as u32;
    let base = (x << k).wrapping_sub(x);
    if extra > 1 {
        b.skip(x + 1 + k);
        base.wrapping_add(extra - 1)
    } else {
        b.skip(x + k);
        base
    }
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
    if i >= n {
        return;
    }
    // The common encoder orders get their own copies of the loop, so the
    // per-sample dot product and adaptation run over a known length.
    match order {
        4 => lpc_fixed::<4>(err, out, bps, &mut coefs[..4], quant),
        8 => lpc_fixed::<8>(err, out, bps, &mut coefs[..8], quant),
        _ => lpc_adaptive(err, out, bps, &mut coefs[..order], order, quant),
    }
}

/// `lpc_adaptive` for a fixed order, with the coefficients and the last `N`
/// outputs held in local arrays for the whole frame. Every sample writes
/// both (the new output, the adapted coefficients) and the next sample reads
/// them all back; through memory those reads wait on the stores just made,
/// and in registers they do not. Same arithmetic, same results.
#[inline(always)]
fn lpc_fixed<const N: usize>(
    err: &[i32],
    out: &mut [i32],
    bps: u32,
    coefs: &mut [i16],
    quant: u32,
) {
    let n = err.len().min(out.len());
    if n <= N + 1 {
        return;
    }
    let round = 1i64 << (quant - 1);
    let mut c: [i16; N] = coefs[..N].try_into().unwrap();
    // `d` is out[i - N - 1]; `past[j]` is out[i - N + j].
    let mut d = out[0];
    let mut past: [i32; N] = out[1..=N].try_into().unwrap();
    for (o, &e) in out[N + 1..n].iter_mut().zip(&err[N + 1..n]) {
        let mut val: i32 = 0;
        for j in 0..N {
            val = val.wrapping_add(past[j].wrapping_sub(d).wrapping_mul(i32::from(c[j])));
        }
        let rounded = ((i64::from(val) + round) >> quant) as i32;
        let y = sign_extend(rounded.wrapping_add(d).wrapping_add(e), bps);
        *o = y;
        // The adaptation, split by the residual's sign so each loop's stop
        // test is a plain compare (the shape symphonia uses). The test runs
        // after each step, which is the same as before the next one, since
        // it holds on entry.
        let mut error_val = e;
        if e > 0 {
            for j in 0..N {
                let v = d.wrapping_sub(past[j]);
                let sign = sign_only(v);
                c[j] = c[j].wrapping_sub(sign as i16);
                error_val = error_val
                    .wrapping_sub((v.wrapping_mul(sign) >> quant).wrapping_mul(j as i32 + 1));
                if error_val <= 0 {
                    break;
                }
            }
        } else if e.wrapping_neg() > 0 {
            // (Written as a negation, like the original `error * sign > 0`
            // test, so i32::MIN stops it exactly as there.)
            for j in 0..N {
                let v = d.wrapping_sub(past[j]);
                let sign = sign_only(v);
                c[j] = c[j].wrapping_add(sign as i16);
                error_val = error_val
                    .wrapping_sub((v.wrapping_mul(-sign) >> quant).wrapping_mul(j as i32 + 1));
                if error_val.wrapping_neg() <= 0 {
                    break;
                }
            }
        }
        d = past[0];
        past.copy_within(1.., 0);
        past[N - 1] = y;
    }
    coefs[..N].copy_from_slice(&c);
}

/// The adaptive part of `lpc_prediction`, from sample `order + 1` on.
#[inline(always)]
fn lpc_adaptive(
    err: &[i32],
    out: &mut [i32],
    bps: u32,
    coefs: &mut [i16],
    order: usize,
    quant: u32,
) {
    let n = err.len().min(out.len());
    let round = 1i64 << (quant - 1);
    for i in order + 1..n {
        // `win[0]` is d = out[i - order - 1]; `win[1 + j]` pairs with coefs[j].
        let (head, tail) = out.split_at_mut(i);
        let win = &head[i - order - 1..];
        let d = win[0];
        let past = &win[1..=order];
        let val = past.iter().zip(coefs.iter()).fold(0i32, |acc, (&o, &c)| {
            acc.wrapping_add(o.wrapping_sub(d).wrapping_mul(i32::from(c)))
        });
        let rounded = ((i64::from(val) + round) >> quant) as i32;
        let mut error_val = err[i] as u32;
        tail[0] = sign_extend(rounded.wrapping_add(d).wrapping_add(error_val as i32), bps);
        let error_sign = sign_only(error_val as i32);
        if error_sign != 0 {
            for (j, (&o, c)) in past.iter().zip(coefs.iter_mut()).enumerate() {
                if (error_val.wrapping_mul(error_sign as u32) as i32) <= 0 {
                    break;
                }
                let v = d.wrapping_sub(o);
                let sign = sign_only(v) * error_sign;
                *c = c.wrapping_sub(sign as i16);
                error_val = error_val.wrapping_sub(
                    ((v.wrapping_mul(sign) >> quant) as u32).wrapping_mul(j as u32 + 1),
                );
            }
        }
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

/// Interleave the first `n` samples of each channel into little-endian bytes,
/// `W` bytes a sample. Mono and stereo get loops of their own, so the common
/// cases write whole frames without a per-sample channel loop.
fn interleave<const W: usize>(
    bufs: &[Vec<i32>],
    n: usize,
    conv: impl Fn(i32) -> [u8; W],
) -> Vec<u8> {
    let ch = bufs.len();
    let mut out = vec![0u8; n * ch * W];
    match bufs {
        [m] => {
            for (d, &s) in out.chunks_exact_mut(W).zip(&m[..n]) {
                d.copy_from_slice(&conv(s));
            }
        }
        [l, r] => {
            for (d, (&a, &b)) in out.chunks_exact_mut(2 * W).zip(l[..n].iter().zip(&r[..n])) {
                d[..W].copy_from_slice(&conv(a));
                d[W..].copy_from_slice(&conv(b));
            }
        }
        _ => {
            for (i, d) in out.chunks_exact_mut(ch * W).enumerate() {
                for (b, o) in bufs.iter().zip(d.chunks_exact_mut(W)) {
                    o.copy_from_slice(&conv(b[i]));
                }
            }
        }
    }
    out
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
        let (format, data) = if cfg.bit_depth == 16 {
            let data = interleave(&self.buffers, n, |s| (s as i16).to_le_bytes());
            (SampleFormat::S16, data)
        } else {
            // Left-justify to 32 bits (as FFmpeg's s32 output), then scale.
            // `v as f32` rounds once, and scaling by a power of two is exact,
            // so this is the same value as rounding `v / 2^31` from f64.
            let shift = 32 - cfg.bit_depth as u32;
            let data = interleave(&self.buffers, n, |s| {
                let v = (s as u32).wrapping_shl(shift) as i32;
                (v as f32 * (1.0 / 2_147_483_648.0)).to_le_bytes()
            });
            (SampleFormat::F32, data)
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
