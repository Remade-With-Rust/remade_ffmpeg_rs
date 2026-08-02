//! PNG still-image codec, backed by our own pure-Rust [`rusty_png`] crate
//! (a performance fork of image-rs/image-png).
//!
//! PNG is self-describing (its `IHDR` carries size + color type), so decode
//! needs no stream parameters: a packet *is* the whole PNG file. We decode to
//! packed [`Rgb24`](PixelFormat::Rgb24)/[`Rgba`](PixelFormat::Rgba) frames and
//! encode those back. To bridge PNG (RGB) and the YUV codecs (AVIF), insert a
//! `-vf format=yuv420p` / `format=rgb24` conversion.

use std::io::Cursor;

use rff_codec::{Codec, CodecRegistry, Decoder, Encoder};
use rff_core::{Dictionary, Error, Frame, MediaType, Packet, PixelFormat, Result, VideoFrame};
use rusty_png::{AdaptiveFilterType, BitDepth, ColorType, Compression, FilterType, Transformations};

/// Register the PNG codec into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Png,
        name: "png",
        long_name: "PNG (Portable Network Graphics) image",
        media_type: MediaType::Video,
        decoder: Some(|| Box::new(PngDecoder::default())),
        encoder: Some(|| Box::new(PngEncoder::default())),
    });
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PngDecoder {
    frame: Option<Frame>,
    eof: bool,
}

impl Decoder for PngDecoder {
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.frame = Some(decode_png(&packet.data)?);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.frame.take() {
            return Ok(frame);
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

fn decode_png(data: &[u8]) -> Result<Frame> {
    let mut decoder = rusty_png::Decoder::new(Cursor::new(data));
    // EXPAND only — deliberately NOT `STRIP_16`.
    //
    // `STRIP_16` reduces 16-bit to 8-bit by keeping the high byte, which is
    // truncation rather than rounding. Measured on a real 16-bit PNG that put
    // **34.0% of bytes off by 1 LSB** from both the source and FFmpeg's decode,
    // where FFmpeg was exact. We take the 16-bit samples and reduce them
    // ourselves with [`sample16_to_8`].
    //
    // The pipeline still speaks 8-bit, so 16-bit input is narrowed rather than
    // carried — but it is now narrowed *correctly*.
    decoder.set_transformations(Transformations::EXPAND);
    let mut reader = decoder
        .read_info()
        .map_err(|e| Error::invalid(format!("png decode: {e}")))?;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| Error::invalid(format!("png decode: {e}")))?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width as usize, info.height as usize);

    // 16-bit sources arrive as big-endian sample pairs. Reduce them ourselves,
    // rounding, rather than letting `STRIP_16` keep the high byte.
    let buf = if reader.output_color_type().1 == BitDepth::Sixteen {
        let mut out = Vec::with_capacity(buf.len() / 2);
        for p in buf.chunks_exact(2) {
            out.push(sample16_to_8(p[0], p[1]));
        }
        out
    } else {
        buf
    };

    // Normalize whatever the PNG decoded to into packed RGB or RGBA.
    let (format, planes, stride) = match reader.output_color_type().0 {
        ColorType::Rgb => (PixelFormat::Rgb24, buf, w * 3),
        ColorType::Rgba => (PixelFormat::Rgba, buf, w * 4),
        ColorType::Grayscale => {
            let mut rgb = vec![0u8; w * h * 3];
            for (i, &g) in buf.iter().enumerate() {
                rgb[i * 3..i * 3 + 3].copy_from_slice(&[g, g, g]);
            }
            (PixelFormat::Rgb24, rgb, w * 3)
        }
        ColorType::GrayscaleAlpha => {
            let mut rgba = vec![0u8; w * h * 4];
            for (i, ga) in buf.chunks_exact(2).enumerate() {
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[ga[0], ga[0], ga[0], ga[1]]);
            }
            (PixelFormat::Rgba, rgba, w * 4)
        }
        other => {
            return Err(Error::unsupported(format!(
                "png decode: unexpected color type {other:?}"
            )))
        }
    };

    Ok(Frame::Video(VideoFrame {
        width: info.width,
        height: info.height,
        format,
        planes: vec![planes],
        strides: vec![stride],
        pts: None,
    }))
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Colour-type analysis
// ---------------------------------------------------------------------------

/// What a packed RGB(A) frame *actually* contains, as opposed to how it is
/// stored.
///
/// Our decoder normalizes every PNG to packed RGB/RGBA (that is what the rest of
/// the pipeline speaks), which means a grayscale or palette PNG that comes in
/// would go straight back out as truecolour and balloon. Measured before this
/// existed: an 8-bit gray image 259,303 B -> 924,819 B (**+257%**), and a
/// 64-colour indexed graphic 6,522 B -> 73,232 B (**+1023%**). The pixels were
/// right; the file was 3.6x / 11.2x too big.
///
/// So the encoder looks at the pixels and picks the narrowest PNG colour type
/// that represents them exactly. Every branch is lossless by construction.
enum ColourKind {
    /// Every pixel has R == G == B (and, for RGBA, alpha is constant 255).
    Gray,
    /// Gray with a non-constant alpha channel.
    GrayAlpha,
    /// At most 256 distinct colours. Carries the palette and, when any entry is
    /// non-opaque, the `tRNS` alpha table.
    Indexed {
        palette: Vec<[u8; 3]>,
        trns: Option<Vec<u8>>,
    },
    /// Genuinely truecolour — store as-is.
    TrueColour,
}

/// Fraction of horizontally repeated pixels, sampled over ~64 rows.
///
/// This is the dispatch signal for [`auto_config`](PngSettings::auto_config).
/// What decides the right encoder setting is how much extra DEFLATE effort buys
/// — on graphics, `Fast` → `Default`+adaptive is 2.4× smaller; on photographs it
/// is marginal. DEFLATE exploits LZ77 matches, so the cheapest honest proxy is
/// "does this image repeat horizontally".
///
/// Measured on the corpus, and it separates with a wide empty band:
///
/// | class | signal |
/// |---|---|
/// | photographic (9 Derf frames) | 0.0366 – **0.2037** |
/// | real graphics (9 assets) | **0.5312** – 0.9790 |
///
/// Nothing lands between 0.204 and 0.531, so [`GRAPHICS_SIGNAL`] sits in an
/// empty gap rather than on a fitted boundary.
fn content_signal(px: &[u8], w: usize, h: usize, ch: usize) -> f64 {
    if w < 2 || h == 0 {
        return 0.0;
    }
    let row = w * ch;
    let step = (h / 64).max(1);
    let (mut repeats, mut total) = (0usize, 0usize);
    let mut y = 0usize;
    while y < h {
        let line = &px[y * row..(y + 1) * row];
        for x in 1..w {
            if line[(x - 1) * ch..(x - 1) * ch + ch] == line[x * ch..x * ch + ch] {
                repeats += 1;
            }
            total += 1;
        }
        y += step;
    }
    if total == 0 {
        0.0
    } else {
        repeats as f64 / total as f64
    }
}

/// Above this repeated-pixel fraction, treat the image as graphics.
/// Chosen from the empty band between the two measured classes (0.204 / 0.531).
const GRAPHICS_SIGNAL: f64 = 0.35;

/// Resolve `-threads` into a worker count.
///
/// `0` means auto, as it does in FFmpeg. The cap is **measured, not arbitrary**:
/// at 8 workers the speedups were 4.71×/5.40×/6.53×, but at 16 one image
/// (blue_sky) *regressed* to 2.57× while another improved to 8.02×. Since the
/// gain past 8 is inconsistent and the downside is real, auto stops at 8 and
/// anyone who wants more asks for it explicitly.
fn resolve_threads(requested: usize) -> usize {
    if requested != 0 {
        return requested;
    }
    std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(1)
}

/// Convert one big-endian 16-bit PNG sample to 8-bit, matching FFmpeg exactly.
///
/// The backing crate's `STRIP_16` keeps the high byte (`v >> 8`) — truncation,
/// not reduction — which put **34.0% of bytes off by 1 LSB** against FFmpeg's
/// decode of the same file.
///
/// The formula here was **measured, not derived**. Candidates scored against
/// FFmpeg over 6.2 M samples:
///
/// | formula | agreement |
/// |---|---|
/// | `v >> 8` (what `STRIP_16` does) | 66.01% |
/// | `round(v * 255 / 65535)` | 42.32% |
/// | `floor(v * 255 / 65535)` | 0.09% |
/// | **`(v + 128) >> 8`** | **100.00%** |
///
/// The "mathematically pure" rescale is *worse* here: FFmpeg rounds the high
/// byte rather than rescaling the range, and for a drop-in replacement matching
/// the reference beats matching the textbook. The clamp matters — `0xFFFF`
/// would otherwise overflow to 256.
#[inline]
fn sample16_to_8(hi: u8, lo: u8) -> u8 {
    let v = u16::from_be_bytes([hi, lo]) as u32;
    ((v + 128) >> 8).min(255) as u8
}

/// Fewest bits per index that can address `n` palette entries.
fn indexed_bit_depth(n: usize) -> BitDepth {
    match n {
        0..=2 => BitDepth::One,
        3..=4 => BitDepth::Two,
        5..=16 => BitDepth::Four,
        _ => BitDepth::Eight,
    }
}

/// One pass over the pixels, with early bail-outs so the cost is negligible on
/// the content that cannot benefit.
///
/// The two tests run together and each drops out as soon as it is refuted: the
/// gray test dies on the first pixel with R != G != B, and the palette test dies
/// on the 257th distinct colour. On a photograph both are refuted within the
/// first few hundred pixels, so this is a scan of a tiny prefix, not of the
/// frame — which is why it can be on by default.
fn analyse(px: &[u8], channels: usize) -> ColourKind {
    let mut is_gray = true;
    let mut alpha_constant = true;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut alphas: Vec<u8> = Vec::new();
    let mut index: std::collections::HashMap<[u8; 4], u8> = std::collections::HashMap::new();
    let mut palette_possible = true;

    for p in px.chunks_exact(channels) {
        let (r, g, b) = (p[0], p[1], p[2]);
        let a = if channels == 4 { p[3] } else { 255 };

        if is_gray && (r != g || g != b) {
            is_gray = false;
        }
        if alpha_constant && a != 255 {
            alpha_constant = false;
        }
        if palette_possible {
            let key = [r, g, b, a];
            if !index.contains_key(&key) {
                if index.len() == 256 {
                    palette_possible = false;
                    index.clear();
                    palette.clear();
                    alphas.clear();
                } else {
                    index.insert(key, index.len() as u8);
                    palette.push([r, g, b]);
                    alphas.push(a);
                }
            }
        }
        if !is_gray && !palette_possible {
            return ColourKind::TrueColour;
        }
    }

    // Gray wins over indexed when both apply: a gray8 image needs no PLTE chunk
    // and indexes nothing, so it is never larger.
    if is_gray {
        return if alpha_constant {
            ColourKind::Gray
        } else {
            ColourKind::GrayAlpha
        };
    }
    if palette_possible {
        let trns = if alphas.iter().all(|&a| a == 255) {
            None
        } else {
            Some(alphas)
        };
        return ColourKind::Indexed { palette, trns };
    }
    ColourKind::TrueColour
}

/// Pack 8-bit indices down to 1/2/4 bits per pixel, MSB-first within each byte
/// and re-starting on every row — the layout PNG's `IHDR` bit-depth field means.
fn pack_indices(indices: &[u8], w: usize, h: usize, depth: BitDepth) -> Vec<u8> {
    let bits = match depth {
        BitDepth::One => 1usize,
        BitDepth::Two => 2,
        BitDepth::Four => 4,
        _ => return indices.to_vec(),
    };
    let per_byte = 8 / bits;
    let row_bytes = w.div_ceil(per_byte);
    let mut out = vec![0u8; row_bytes * h];
    for y in 0..h {
        for x in 0..w {
            let v = indices[y * w + x] & ((1 << bits) - 1) as u8;
            let shift = 8 - bits * (x % per_byte + 1);
            out[y * row_bytes + x / per_byte] |= v << shift;
        }
    }
    out
}

/// Encoder tuning. The defaults reproduce the historical behaviour exactly —
/// `Compression::Fast` + `FilterType::Sub` non-adaptive — because that is what
/// the backing crate defaults to and what every existing output was encoded
/// with. Changing the default is a separate, separately-gated decision.
#[derive(Clone, Copy)]
struct PngSettings {
    compression: Compression,
    filter: FilterType,
    adaptive: AdaptiveFilterType,
    /// Pick compression+filter from the content when the user has not chosen.
    /// See [`content_signal`]. Off automatically the moment `-compression_level`
    /// or `-pred` is given, so an explicit setting is never second-guessed.
    auto_config: bool,
    /// Set once the user names a compression level or filter explicitly.
    explicit_compression: bool,
    explicit_filter: bool,
    /// DEFLATE worker budget. 0 = auto (capped, see `resolve_threads`), 1 =
    /// serial. Parallel output is lossless but not byte-identical to serial,
    /// so `-threads 1` is the way back to reproducible bytes.
    ///
    /// **Applies to `-compression_level 2..9` only.** Levels 0–1 map to
    /// `Compression::Fast`, which is `fdeflate` — a single-stream fast path with
    /// no block splitting — so `-threads` has no effect there. That is the right
    /// trade rather than an oversight: the measured gap against FFmpeg lives at
    /// the quality levels (matched filter and size: 0.69–0.92×), while at `Fast`
    /// we are already ~6× faster than FFmpeg's default.
    threads: usize,
    /// Narrow the output colour type to gray/indexed when the pixels allow it.
    /// On by default: it is lossless, and without it every grayscale or palette
    /// PNG that passes through this codec is re-emitted as truecolour (+257% /
    /// +1023% measured). `-png_auto_type 0` restores the old behaviour.
    auto_type: bool,
}

impl Default for PngSettings {
    fn default() -> Self {
        PngSettings {
            compression: Compression::Fast,
            filter: FilterType::Sub,
            adaptive: AdaptiveFilterType::NonAdaptive,
            threads: 0,
            auto_config: true,
            explicit_compression: false,
            explicit_filter: false,
            auto_type: true,
        }
    }
}

/// Map FFmpeg's `-compression_level 0..9` onto the backing crate's three-level
/// enum. The crate exposes `Fast` (fdeflate), `Default` and `Best` rather than
/// zlib's ten levels, so this is a documented bucketing, not a 1:1 translation:
/// 0–1 stay on the fast path, 2–8 take the balanced path, 9 asks for the
/// smallest file.
fn compression_from_level(level: i32) -> Compression {
    match level {
        i32::MIN..=1 => Compression::Fast,
        2..=8 => Compression::Default,
        _ => Compression::Best,
    }
}

/// FFmpeg's `-pred` names. `mixed` is FFmpeg's per-row adaptive choice, so it
/// maps to the crate's adaptive filter rather than to a fixed filter type.
fn filter_from_pred(pred: &str) -> Option<(FilterType, AdaptiveFilterType)> {
    let f = match pred.trim().to_ascii_lowercase().as_str() {
        "none" => (FilterType::NoFilter, AdaptiveFilterType::NonAdaptive),
        "sub" => (FilterType::Sub, AdaptiveFilterType::NonAdaptive),
        "up" => (FilterType::Up, AdaptiveFilterType::NonAdaptive),
        "avg" | "average" => (FilterType::Avg, AdaptiveFilterType::NonAdaptive),
        "paeth" => (FilterType::Paeth, AdaptiveFilterType::NonAdaptive),
        "mixed" | "adaptive" => (FilterType::Paeth, AdaptiveFilterType::Adaptive),
        _ => return None,
    };
    Some(f)
}

#[derive(Default)]
struct PngEncoder {
    packet: Option<Packet>,
    eof: bool,
    settings: PngSettings,
}

impl Encoder for PngEncoder {
    /// `-compression_level 0..9` and `-pred none|sub|up|avg|paeth|mixed`, both
    /// spelled the way FFmpeg's PNG encoder spells them so the CLI stays
    /// drop-in. Before this existed neither knob was reachable at all: the
    /// backing crate's defaults were the only operating point this codec could
    /// ever produce.
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        if let Some(v) = options.get("compression_level") {
            match v.trim().parse::<i32>() {
                Ok(level) => {
                    self.settings.compression = compression_from_level(level);
                    self.settings.explicit_compression = true;
                }
                Err(_) => {
                    return Err(Error::invalid(format!(
                        "png encode: -compression_level wants 0..9, got `{v}`"
                    )))
                }
            }
        }
        if let Some(v) = options.get("threads") {
            match v.trim().parse::<usize>() {
                Ok(n) => self.settings.threads = n,
                Err(_) => {
                    return Err(Error::invalid(format!(
                        "png encode: -threads wants a non-negative integer, got `{v}`"
                    )))
                }
            }
        }
        if let Some(v) = options.get("png_auto_config") {
            self.settings.auto_config = !matches!(v.trim(), "0" | "false" | "off" | "no");
        }
        if let Some(v) = options.get("png_auto_type") {
            self.settings.auto_type = !matches!(v.trim(), "0" | "false" | "off" | "no");
        }
        if let Some(v) = options.get("pred") {
            match filter_from_pred(v) {
                Some((f, a)) => {
                    self.settings.filter = f;
                    self.settings.adaptive = a;
                    self.settings.explicit_filter = true;
                }
                None => {
                    return Err(Error::unsupported(format!(
                        "png encode: unknown -pred `{v}` (want none/sub/up/avg/paeth/mixed)"
                    )))
                }
            }
        }
        Ok(())
    }

    /// Packed RGB only — this encoder has no Y'CbCr path, so the pipeline
    /// converts planar input (e.g. straight from the JPEG decoder) for us.
    fn accepted_pixel_formats(&self) -> Option<Vec<PixelFormat>> {
        Some(vec![PixelFormat::Rgb24, PixelFormat::Rgba])
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported(
                    "png encode: audio frame on an image codec",
                ))
            }
        };
        self.packet = Some(Packet::from_data(0, encode_png(vf, self.settings)?));
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(packet) = self.packet.take() {
            return Ok(packet);
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

fn encode_png(vf: &VideoFrame, settings: PngSettings) -> Result<Vec<u8>> {
    let (color, channels) = match vf.format {
        PixelFormat::Rgb24 => (ColorType::Rgb, 3usize),
        PixelFormat::Rgba => (ColorType::Rgba, 4usize),
        other => {
            return Err(Error::unsupported(format!(
                "png encode: needs rgb24/rgba, got `{}` (convert with -vf format=rgb24)",
                other.name()
            )))
        }
    };
    let (w, h) = (vf.width as usize, vf.height as usize);
    let row = w * channels;
    let stride = vf.strides[0];

    // png wants tightly packed rows; repack if the source stride has padding.
    let mut packed: Vec<u8> = if stride == row {
        vf.planes[0].clone()
    } else {
        let mut p = Vec::with_capacity(row * h);
        for j in 0..h {
            p.extend_from_slice(&vf.planes[0][j * stride..j * stride + row]);
        }
        p
    };

    // Pick the operating point from the content, unless the caller named one.
    //
    // The shipped default (`Fast`/`Sub`) is genuinely good on photographs —
    // faster AND smaller than every ffmpeg `-compression_level 1` setting — and
    // catastrophic on graphics: measured over nine real screenshots, charts,
    // diagrams and logos it ran **+130.1%** against ffmpeg's default, up to
    // +1409% on a matplotlib chart. One fixed default cannot serve both, which
    // makes this a dispatch, not a tuning choice.
    //
    // `Compression::Default` + adaptive filtering was chosen by measuring the
    // whole graphics corpus rather than by counting per-image winners:
    //
    //   config              total vs ffmpeg   worst image   encode time
    //   fast/sub (shipped)        +130.1%     +1409.0%          451 ms
    //   default + adaptive          -2.4%        +0.7%          502 ms   <-- chosen
    //   best + adaptive             -6.3%        -3.3%        3,638 ms
    //
    // i.e. +130.1% -> -2.4% for 11% more time. `best` buys 3.9 more points for
    // 8.1x the time, which is a bad default however good the number looks.
    let mut settings = settings;
    if settings.auto_config && !(settings.explicit_compression && settings.explicit_filter) {
        if content_signal(&packed, w, h, channels) >= GRAPHICS_SIGNAL {
            if !settings.explicit_compression {
                settings.compression = Compression::Default;
            }
            if !settings.explicit_filter {
                settings.filter = FilterType::Paeth;
                settings.adaptive = AdaptiveFilterType::Adaptive;
            }
        }
    }

    // Narrow the colour type when the pixels allow it. Lossless in every branch:
    // gray only when R == G == B everywhere, indexed only when the exact colour
    // set fits in 256 entries.
    let kind = if settings.auto_type {
        analyse(&packed, channels)
    } else {
        ColourKind::TrueColour
    };

    let (out_color, out_depth, body, palette, trns) = match kind {
        ColourKind::Gray => {
            let mut g = Vec::with_capacity(w * h);
            for p in packed.chunks_exact(channels) {
                g.push(p[0]);
            }
            (ColorType::Grayscale, BitDepth::Eight, g, None, None)
        }
        ColourKind::GrayAlpha => {
            let mut g = Vec::with_capacity(w * h * 2);
            for p in packed.chunks_exact(channels) {
                g.push(p[0]);
                g.push(if channels == 4 { p[3] } else { 255 });
            }
            (ColorType::GrayscaleAlpha, BitDepth::Eight, g, None, None)
        }
        ColourKind::Indexed { palette, trns } => {
            let depth = indexed_bit_depth(palette.len());
            let mut lut = std::collections::HashMap::with_capacity(palette.len());
            for (i, c) in palette.iter().enumerate() {
                // key on RGB+A so entries that differ only in alpha stay distinct
                let a = trns.as_ref().map_or(255, |t| t[i]);
                lut.insert([c[0], c[1], c[2], a], i as u8);
            }
            let mut idx = Vec::with_capacity(w * h);
            for p in packed.chunks_exact(channels) {
                let a = if channels == 4 { p[3] } else { 255 };
                idx.push(lut[&[p[0], p[1], p[2], a]]);
            }
            let body = pack_indices(&idx, w, h, depth);
            let flat: Vec<u8> = palette.iter().flat_map(|c| c.iter().copied()).collect();
            (ColorType::Indexed, depth, body, Some(flat), trns)
        }
        // `take` rather than clone: on this path `packed` is the body, and the
        // small-image palette re-check below only runs when indexing WAS used,
        // so nothing reads `packed` afterwards here.
        ColourKind::TrueColour => (
            color,
            BitDepth::Eight,
            std::mem::take(&mut packed),
            None,
            None,
        ),
    };

    // Pre-size the output buffer so it never reallocates.
    //
    // The encoder builds the whole IDAT then copies it into `out` through the
    // generic `Write`; starting empty makes `out` double repeatedly on the way
    // to (e.g.) 17 MB, re-copying what it already holds each time. The stage
    // profiler measured that chunk-write at 1.70 GB/s — far below memcpy — and
    // pre-sizing cut the stage from 14.755 ms to 6.247 ms on an 8.3 MPx frame.
    //
    // HONEST CAVEAT: that stage win does NOT show up end-to-end. Paired,
    // ABBA-interleaved, byte-identical-output A/B against a pre-change binary:
    // 1.017x / 0.982x / 0.983x / 0.974x at 0.4-2 MPx and 1.010x at 8.3 MPx —
    // every one inside the 2.0-2.3% null-arm floor. So this is kept because it
    // removes real redundant work and cannot change output, NOT because it
    // makes anything measurably faster. Do not quote it as a speedup.
    //
    // `body.len() + 1024` is an upper bound, not a guess: DEFLATE's stored mode
    // is the worst case and adds well under 1 KB of block headers at these
    // sizes, so this reserves exactly once.
    let indexed_used = matches!(out_color, ColorType::Indexed);
    let out = emit(
        vf, settings, out_color, out_depth, &body, palette, trns,
    )?;

    // A palette is not free: PLTE is 3 bytes per entry of essentially
    // incompressible data. On a real image that is nothing against a multi-KB
    // IDAT, but on a small one it can cost more than indexing saves — measured
    // on a 64x40, 40-colour frame, indexed came out 252 B against 188 B
    // truecolour, because DEFLATE crushes the smooth truecolour data while the
    // palette stays ~120 B.
    //
    // Rather than guess a threshold, encode the alternative too and keep the
    // smaller — but only for inputs small enough that a second encode is free.
    // Above this size the palette is at most 768 B against a megabyte of raw
    // data, so indexing always wins and the check is skipped.
    const DUAL_ENCODE_LIMIT: usize = 1_000_000;
    if indexed_used && packed.len() < DUAL_ENCODE_LIMIT {
        let alt = emit(vf, settings, color, BitDepth::Eight, &packed, None, None)?;
        if alt.len() < out.len() {
            return Ok(alt);
        }
    }
    Ok(out)
}

/// Write one PNG with an explicit colour type / palette. Split out so the
/// small-image palette check can encode a second candidate cheaply.
#[allow(clippy::too_many_arguments)]
fn emit(
    vf: &VideoFrame,
    settings: PngSettings,
    color: ColorType,
    depth: BitDepth,
    body: &[u8],
    palette: Option<Vec<u8>>,
    trns: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(body.len() + 1024);
    {
        let mut encoder = rusty_png::Encoder::new(&mut out, vf.width, vf.height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        if let Some(p) = palette {
            encoder.set_palette(p);
        }
        if let Some(t) = trns {
            encoder.set_trns(t);
        }
        encoder.set_compression(settings.compression);
        encoder.set_filter(settings.filter);
        encoder.set_adaptive_filter(settings.adaptive);
        // Multi-threaded DEFLATE. Lossless, but the compressed bytes differ
        // from serial, so `-threads 1` restores byte-for-byte reproducibility.
        // Images too small to split stay serial automatically and pay nothing.
        encoder.set_parallel(resolve_threads(settings.threads));
        let mut writer = encoder
            .write_header()
            .map_err(|e| Error::invalid(format!("png encode: {e}")))?;
        writer
            .write_image_data(body)
            .map_err(|e| Error::invalid(format!("png encode: {e}")))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_frame(w: u32, h: u32) -> Frame {
        let (wi, hi) = (w as usize, h as usize);
        let mut rgb = vec![0u8; wi * hi * 3];
        for j in 0..hi {
            for i in 0..wi {
                let o = (j * wi + i) * 3;
                rgb[o] = (i * 255 / (wi - 1)) as u8;
                rgb[o + 1] = (j * 255 / (hi - 1)) as u8;
                rgb[o + 2] = 128;
            }
        }
        Frame::Video(VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Rgb24,
            planes: vec![rgb],
            strides: vec![wi * 3],
            pts: None,
        })
    }

    #[test]
    fn png_encode_decode_is_lossless() {
        let original = rgb_frame(40, 24);
        let Frame::Video(src) = &original else {
            unreachable!()
        };

        let bytes = encode_png(src, PngSettings::default()).unwrap();
        assert_eq!(&bytes[1..4], b"PNG"); // PNG signature

        let Frame::Video(decoded) = decode_png(&bytes).unwrap() else {
            unreachable!()
        };
        assert_eq!((decoded.width, decoded.height), (40, 24));
        assert_eq!(decoded.format, PixelFormat::Rgb24);
        // PNG is lossless: pixels must match exactly.
        assert_eq!(decoded.planes[0], src.planes[0]);
    }

    /// Build an RGB24 frame from a closure so each colour-type case is explicit.
    fn frame_from(w: u32, h: u32, ch: usize, f: impl Fn(usize, usize) -> [u8; 4]) -> VideoFrame {
        let (wi, hi) = (w as usize, h as usize);
        let mut data = Vec::with_capacity(wi * hi * ch);
        for j in 0..hi {
            for i in 0..wi {
                let p = f(i, j);
                data.extend_from_slice(&p[..ch]);
            }
        }
        VideoFrame {
            width: w,
            height: h,
            format: if ch == 4 {
                PixelFormat::Rgba
            } else {
                PixelFormat::Rgb24
            },
            planes: vec![data],
            strides: vec![wi * ch],
            pts: None,
        }
    }

    /// Auto-typing must be LOSSLESS in every branch and must actually shrink the
    /// file. Round-trip is the gate: narrowing the colour type is only valid if
    /// the pixels come back bit-for-bit.
    /// 16-bit reduction must match FFmpeg exactly, including the endpoints.
    /// The formula was chosen by measurement (100.00% agreement over 6.2 M
    /// samples, against 66.01% for the truncation it replaced), so this pins it.
    #[test]
    fn sixteen_bit_reduction_matches_ffmpeg() {
        // endpoints
        assert_eq!(sample16_to_8(0x00, 0x00), 0);
        assert_eq!(sample16_to_8(0xFF, 0xFF), 255, "0xFFFF must clamp to 255, not overflow");
        // the rounding boundary: 0x7F80 is exactly .5 at the high byte
        assert_eq!(sample16_to_8(0x7F, 0x7F), 0x7F);
        assert_eq!(sample16_to_8(0x7F, 0x80), 0x80);
        // real samples taken from an FFmpeg-produced rgb48be frame
        for (hi, lo, want) in [
            (0x0Fu8, 0xFEu8, 16u8),
            (0x12, 0x03, 18),
            (0x0F, 0x04, 15),
            (0x10, 0xFF, 17),
            (0x13, 0x03, 19),
        ] {
            assert_eq!(
                sample16_to_8(hi, lo),
                want,
                "0x{hi:02x}{lo:02x} should reduce to {want}"
            );
        }
    }

    #[test]
    fn auto_type_is_lossless_and_smaller() {
        let cases: Vec<(&str, VideoFrame)> = vec![
            // pure gray -> Grayscale
            ("gray", frame_from(64, 40, 3, |i, j| {
                let v = ((i * 4 + j) % 256) as u8;
                [v, v, v, 255]
            })),
            // two colours -> Indexed at 1 bit per pixel
            ("bilevel", frame_from(64, 40, 3, |i, j| {
                if (i / 3 + j / 5) % 2 == 0 { [10, 200, 30, 255] } else { [250, 5, 90, 255] }
            })),
            // ~40 colours, big enough that the 120-byte PLTE is negligible
            // -> Indexed must win outright.
            ("palette", frame_from(400, 300, 3, |i, j| {
                let n = ((i / 50) + (j / 40) * 8) as u8;
                [n * 6, 255 - n * 5, n.wrapping_mul(17), 255]
            })),
            // Same content but TINY: here the incompressible PLTE costs more
            // than indexing saves, so auto-type must fall back rather than
            // inflate the file. Measured before the guard: 252 B vs 188 B.
            ("palette_tiny", frame_from(64, 40, 3, |i, j| {
                let n = ((i / 8) + (j / 8) * 8) as u8;
                [n * 6, 255 - n * 5, n.wrapping_mul(17), 255]
            })),
            // gray with varying alpha -> GrayscaleAlpha
            ("gray_alpha", frame_from(64, 40, 4, |i, j| {
                let v = ((i + j) % 256) as u8;
                [v, v, v, ((i * 3) % 256) as u8]
            })),
            // photographic-ish -> must stay TrueColour
            ("truecolour", frame_from(64, 40, 3, |i, j| {
                [(i * 7 % 256) as u8, (j * 13 % 256) as u8, ((i * j) % 256) as u8, 255]
            })),
        ];

        for (name, vf) in cases {
            let mut on = PngSettings::default();
            on.auto_type = true;
            let mut off = PngSettings::default();
            off.auto_type = false;

            let a = encode_png(&vf, on).unwrap();
            let b = encode_png(&vf, off).unwrap();

            // 1. lossless: decoding the narrowed file reproduces the source pixels
            let Frame::Video(dec) = decode_png(&a).unwrap() else {
                unreachable!()
            };
            assert_eq!(
                dec.planes[0], vf.planes[0],
                "{name}: auto-typed output did not round-trip losslessly"
            );
            assert_eq!(dec.format, vf.format, "{name}: pixel format changed");

            // 2. never larger than leaving it truecolour
            assert!(
                a.len() <= b.len(),
                "{name}: auto-type made the file BIGGER ({} vs {})",
                a.len(),
                b.len()
            );
            // `truecolour` has nothing to narrow to, and `palette_tiny` is the
            // case where narrowing legitimately does not pay — both must simply
            // never come out LARGER, which is asserted above.
            if name != "truecolour" && name != "palette_tiny" {
                assert!(
                    a.len() < b.len(),
                    "{name}: auto-type should have shrunk this ({} vs {})",
                    a.len(),
                    b.len()
                );
            }
        }
    }
}
