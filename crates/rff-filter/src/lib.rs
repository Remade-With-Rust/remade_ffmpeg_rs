//! `rff-filter` — frame filters, the `libavfilter` equivalent.
//!
//! A [`FilterChain`] is the engine's `-vf` graph: an ordered list of filters
//! that each transform a raw [`VideoFrame`] into another. Built from an
//! FFmpeg-style spec string (`scale=320:240,crop=100:100:0:0`) and applied
//! between decode and encode in the transcode loop.
//!
//! Filters operate on **8-bit planar YUV** (4:2:0 / 4:2:2 / 4:4:4) for now;
//! other layouts return [`Error::Unsupported`]. Each filter is a 1-in/1-out
//! transform.

use rff_core::{Error, PixelFormat, Result, VideoFrame};

/// One frame transform. Implementors map a video frame to a new video frame.
pub trait Filter: Send {
    /// The filter's CLI name (`scale`, `crop`, ...).
    fn name(&self) -> &'static str;

    /// Transform one frame.
    fn filter(&mut self, frame: VideoFrame) -> Result<VideoFrame>;

    /// Report the output dimensions this filter produces for a given input
    /// size, without running it (so the muxer can size the output stream up
    /// front). Defaults to unchanged.
    fn output_dims(&self, in_w: u32, in_h: u32) -> (u32, u32) {
        (in_w, in_h)
    }
}

/// An ordered chain of filters (the `-vf` graph).
#[derive(Default)]
pub struct FilterChain {
    filters: Vec<Box<dyn Filter>>,
}

impl FilterChain {
    /// Parse an FFmpeg-style filter spec: filters separated by `,`, each
    /// `name=a:b:c`. An empty/blank spec yields an empty (pass-through) chain.
    pub fn parse(spec: &str) -> Result<FilterChain> {
        let mut filters: Vec<Box<dyn Filter>> = Vec::new();
        for token in spec.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let (name, args) = token.split_once('=').unwrap_or((token, ""));
            let parts: Vec<&str> = if args.is_empty() {
                Vec::new()
            } else {
                args.split(':').collect()
            };
            match name {
                "scale" => filters.push(Box::new(Scale::parse(&parts)?)),
                "crop" => filters.push(Box::new(Crop::parse(&parts)?)),
                "hflip" => filters.push(Box::new(HFlip)),
                "vflip" => filters.push(Box::new(VFlip)),
                "transpose" => filters.push(Box::new(Transpose::parse(&parts)?)),
                "pad" => filters.push(Box::new(Pad::parse(&parts)?)),
                "format" => filters.push(Box::new(FormatConv::parse(&parts)?)),
                "negate" => filters.push(Box::new(Negate)),
                "gray" | "grayscale" => filters.push(Box::new(Grayscale)),
                other => return Err(Error::unsupported(format!("unknown filter `{other}`"))),
            }
        }
        Ok(FilterChain { filters })
    }

    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    /// Run every filter in order.
    pub fn apply(&mut self, mut frame: VideoFrame) -> Result<VideoFrame> {
        for filter in &mut self.filters {
            frame = filter.filter(frame)?;
        }
        Ok(frame)
    }

    /// The dimensions a frame of `(w, h)` ends up with after the whole chain.
    pub fn output_dims(&self, mut w: u32, mut h: u32) -> (u32, u32) {
        for filter in &self.filters {
            (w, h) = filter.output_dims(w, h);
        }
        (w, h)
    }
}

// ---------------------------------------------------------------------------
// filter_complex (multi-input graphs)
// ---------------------------------------------------------------------------

/// A `-filter_complex` graph. For now it models the most-used multi-input
/// operation: **overlay** — compositing a second input over the first (logo /
/// watermark / picture-in-picture). The labels (`[0:v][1:v]...`) are parsed but
/// the wiring is fixed: input 0 is the base, input 1 the overlay.
pub struct FilterComplex {
    /// `(x, y)` placement of the overlay's top-left on the base, or `None` if
    /// the graph has no overlay.
    pub overlay: Option<(u32, u32)>,
}

impl FilterComplex {
    /// Parse a filter_complex spec. Recognizes `overlay=X:Y` (numeric, plus the
    /// common corner shorthands `W-w`, `H-h` resolved against the base size at
    /// apply time — for now numeric offsets only).
    pub fn parse(spec: &str) -> Result<FilterComplex> {
        if let Some(pos) = spec.find("overlay") {
            let after = spec[pos + "overlay".len()..].trim_start_matches('=');
            // Stop at the next filter (`,`) or output label (`[`).
            let args = after.split([',', '[', ';']).next().unwrap_or("");
            let mut parts = args.split(':');
            let x = parts
                .next()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            let y = parts
                .next()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            return Ok(FilterComplex {
                overlay: Some((x, y)),
            });
        }
        Err(Error::unsupported(format!(
            "filter_complex: unsupported graph `{spec}` (only `overlay` so far)"
        )))
    }
}

/// Composite `over` onto a copy of `base` at `(x, y)`, both 8-bit planar YUV of
/// the same format. Overlay pixels replace the base (YUV carries no alpha);
/// the overlay is clipped to the base bounds and `(x, y)` snaps down to the
/// chroma grid so the planes stay aligned.
pub fn overlay(base: VideoFrame, over: &VideoFrame, x: u32, y: u32) -> Result<VideoFrame> {
    ensure_planar_yuv(&base, "overlay")?;
    ensure_planar_yuv(over, "overlay")?;
    if base.format != over.format {
        return Err(Error::unsupported(format!(
            "overlay: base {} vs overlay {} — convert with the `format` filter first",
            base.format.name(),
            over.format.name()
        )));
    }
    let (sx, sy) = subsampling(base.format)?;
    let (x, y) = (x - x % sx, y - y % sy);
    let mut base = base;
    for i in 0..3 {
        let (div_x, div_y) = if i == 0 { (1, 1) } else { (sx, sy) };
        let (ox, oy) = ((x / div_x) as usize, (y / div_y) as usize);
        let (ow, oh) = plane_dims(over.format, over.width, over.height, i)?;
        let (bw, bh) = plane_dims(base.format, base.width, base.height, i)?;
        let (bw, bh) = (bw as usize, bh as usize);
        let (bstride, ostride) = (base.strides[i], over.strides[i]);
        for row in 0..oh as usize {
            let by = oy + row;
            if by >= bh {
                break;
            }
            let copy_w = (ow as usize).min(bw.saturating_sub(ox));
            if copy_w == 0 {
                break;
            }
            let bstart = by * bstride + ox;
            let ostart = row * ostride;
            base.planes[i][bstart..bstart + copy_w]
                .copy_from_slice(&over.planes[i][ostart..ostart + copy_w]);
        }
    }
    Ok(base)
}

// ---------------------------------------------------------------------------
// Pixel-format helpers
// ---------------------------------------------------------------------------

/// Chroma subsampling factors `(x, y)` for a planar YUV format.
fn subsampling(format: PixelFormat) -> Result<(u32, u32)> {
    match format {
        PixelFormat::Yuv420p => Ok((2, 2)),
        PixelFormat::Yuv422p => Ok((2, 1)),
        PixelFormat::Yuv444p => Ok((1, 1)),
        other => Err(Error::unsupported(format!(
            "filter: pixel format `{}` (only 8-bit planar YUV is supported)",
            other.name()
        ))),
    }
}

/// Width/height of plane `index` for `format` at frame size `(w, h)`.
fn plane_dims(format: PixelFormat, w: u32, h: u32, index: usize) -> Result<(u32, u32)> {
    if index == 0 {
        Ok((w, h))
    } else {
        let (sx, sy) = subsampling(format)?;
        Ok((w.div_ceil(sx), h.div_ceil(sy)))
    }
}

fn ensure_planar_yuv(frame: &VideoFrame, op: &str) -> Result<()> {
    subsampling(frame.format)?; // rejects non-planar / 10-bit
    if frame.planes.len() < 3 || frame.strides.len() < 3 {
        return Err(Error::invalid(format!(
            "{op}: expected 3 planes, got {}",
            frame.planes.len()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// scale (bilinear)
// ---------------------------------------------------------------------------

/// `scale=W:H` — resize to an exact width and height (bilinear). Either
/// dimension may be `-1` to preserve the aspect ratio (rounded to an even size
/// so chroma stays aligned); both `-1` is rejected.
struct Scale {
    width: i32,
    height: i32,
}

impl Scale {
    fn parse(args: &[&str]) -> Result<Scale> {
        let [w, h] = args else {
            return Err(Error::Option("scale: expected scale=W:H".into()));
        };
        let parse = |s: &str| {
            s.trim()
                .parse::<i32>()
                .ok()
                .filter(|&v| v == -1 || v > 0)
                .ok_or_else(|| Error::Option(format!("scale: bad dimension `{s}`")))
        };
        let (width, height) = (parse(w)?, parse(h)?);
        if width == -1 && height == -1 {
            return Err(Error::Option(
                "scale: at least one of W/H must be a fixed size".into(),
            ));
        }
        Ok(Scale { width, height })
    }

    /// Resolve target dimensions, filling a `-1` from the source aspect ratio.
    fn resolve(&self, in_w: u32, in_h: u32) -> (u32, u32) {
        let even = |v: u32| {
            let v = v.max(2);
            if v % 2 == 0 {
                v
            } else {
                v + 1
            }
        };
        match (self.width, self.height) {
            (w, h) if w > 0 && h > 0 => (w as u32, h as u32),
            (-1, h) => {
                let h = h as u32;
                let w = (in_w as u64 * h as u64 / in_h.max(1) as u64) as u32;
                (even(w), h)
            }
            (w, _) => {
                let w = w as u32;
                let h = (in_h as u64 * w as u64 / in_w.max(1) as u64) as u32;
                (w, even(h))
            }
        }
    }
}

impl Filter for Scale {
    fn name(&self) -> &'static str {
        "scale"
    }

    fn output_dims(&self, in_w: u32, in_h: u32) -> (u32, u32) {
        self.resolve(in_w, in_h)
    }

    fn filter(&mut self, src: VideoFrame) -> Result<VideoFrame> {
        ensure_planar_yuv(&src, "scale")?;
        let (out_w, out_h) = self.resolve(src.width, src.height);
        let mut planes = Vec::with_capacity(src.planes.len());
        let mut strides = Vec::with_capacity(src.planes.len());
        for (i, plane) in src.planes.iter().enumerate() {
            let (sw, sh) = plane_dims(src.format, src.width, src.height, i)?;
            let (dw, dh) = plane_dims(src.format, out_w, out_h, i)?;
            planes.push(bilinear(plane, src.strides[i], sw, sh, dw, dh));
            strides.push(dw as usize);
        }
        Ok(VideoFrame {
            width: out_w,
            height: out_h,
            format: src.format,
            planes,
            strides,
            pts: src.pts,
        })
    }
}

/// Bilinear-resample one 8-bit plane from `(sw, sh)` to `(dw, dh)`. Uses
/// half-pixel-centered sampling and edge clamping; output is tightly packed.
fn bilinear(src: &[u8], src_stride: usize, sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = vec![0u8; (dw * dh) as usize];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }
    let x_ratio = sw as f32 / dw as f32;
    let y_ratio = sh as f32 / dh as f32;
    let at = |x: u32, y: u32| src[y as usize * src_stride + x as usize] as f32;

    for dy in 0..dh {
        let fy = (((dy as f32 + 0.5) * y_ratio) - 0.5).max(0.0);
        let y0 = fy.floor() as u32;
        let y1 = (y0 + 1).min(sh - 1);
        let wy = fy - y0 as f32;
        for dx in 0..dw {
            let fx = (((dx as f32 + 0.5) * x_ratio) - 0.5).max(0.0);
            let x0 = fx.floor() as u32;
            let x1 = (x0 + 1).min(sw - 1);
            let wx = fx - x0 as f32;

            let top = at(x0, y0) * (1.0 - wx) + at(x1, y0) * wx;
            let bottom = at(x0, y1) * (1.0 - wx) + at(x1, y1) * wx;
            let value = top * (1.0 - wy) + bottom * wy;
            out[(dy * dw + dx) as usize] = value.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// crop
// ---------------------------------------------------------------------------

/// `crop=W:H:X:Y` — extract a rectangle. Offsets/size must align to the chroma
/// grid (e.g. even for 4:2:0) so the chroma planes stay sample-aligned.
struct Crop {
    w: u32,
    h: u32,
    x: u32,
    y: u32,
}

impl Crop {
    fn parse(args: &[&str]) -> Result<Crop> {
        let [w, h, x, y] = args else {
            return Err(Error::Option("crop: expected crop=W:H:X:Y".into()));
        };
        let num = |s: &str| {
            s.trim()
                .parse::<u32>()
                .map_err(|_| Error::Option(format!("crop: bad value `{s}`")))
        };
        Ok(Crop {
            w: num(w)?,
            h: num(h)?,
            x: num(x)?,
            y: num(y)?,
        })
    }
}

impl Filter for Crop {
    fn name(&self) -> &'static str {
        "crop"
    }

    fn output_dims(&self, _in_w: u32, _in_h: u32) -> (u32, u32) {
        (self.w, self.h)
    }

    fn filter(&mut self, src: VideoFrame) -> Result<VideoFrame> {
        ensure_planar_yuv(&src, "crop")?;
        if self.x + self.w > src.width || self.y + self.h > src.height {
            return Err(Error::Option(format!(
                "crop: rectangle {}x{}+{}+{} exceeds frame {}x{}",
                self.w, self.h, self.x, self.y, src.width, src.height
            )));
        }
        let (sx, sy) = subsampling(src.format)?;
        if self.x % sx != 0 || self.w % sx != 0 || self.y % sy != 0 || self.h % sy != 0 {
            return Err(Error::Option(
                "crop: offset/size must align to the chroma grid".into(),
            ));
        }

        let mut planes = Vec::with_capacity(src.planes.len());
        let mut strides = Vec::with_capacity(src.planes.len());
        for (i, plane) in src.planes.iter().enumerate() {
            // Plane 0 uses full-res geometry; chroma planes scale by subsampling.
            let (div_x, div_y) = if i == 0 { (1, 1) } else { (sx, sy) };
            let (px, py) = (self.x / div_x, self.y / div_y);
            let (pw, ph) = (self.w / div_x, self.h / div_y);

            let mut out = Vec::with_capacity((pw * ph) as usize);
            for row in 0..ph {
                let start = (py + row) as usize * src.strides[i] + px as usize;
                out.extend_from_slice(&plane[start..start + pw as usize]);
            }
            planes.push(out);
            strides.push(pw as usize);
        }
        Ok(VideoFrame {
            width: self.w,
            height: self.h,
            format: src.format,
            planes,
            strides,
            pts: src.pts,
        })
    }
}

// ---------------------------------------------------------------------------
// hflip / vflip
// ---------------------------------------------------------------------------

/// `hflip` — mirror each row left-to-right. Dimensions and format unchanged.
struct HFlip;

impl Filter for HFlip {
    fn name(&self) -> &'static str {
        "hflip"
    }

    fn filter(&mut self, src: VideoFrame) -> Result<VideoFrame> {
        ensure_planar_yuv(&src, "hflip")?;
        let mut planes = Vec::with_capacity(src.planes.len());
        let mut strides = Vec::with_capacity(src.planes.len());
        for (i, plane) in src.planes.iter().enumerate() {
            let (pw, ph) = plane_dims(src.format, src.width, src.height, i)?;
            let (pw, ph, stride) = (pw as usize, ph as usize, src.strides[i]);
            let mut out = vec![0u8; pw * ph];
            for y in 0..ph {
                let row = &plane[y * stride..y * stride + pw];
                let dst = &mut out[y * pw..(y + 1) * pw];
                for x in 0..pw {
                    dst[x] = row[pw - 1 - x];
                }
            }
            planes.push(out);
            strides.push(pw);
        }
        Ok(VideoFrame {
            width: src.width,
            height: src.height,
            format: src.format,
            planes,
            strides,
            pts: src.pts,
        })
    }
}

/// `vflip` — mirror rows top-to-bottom. Dimensions and format unchanged.
struct VFlip;

impl Filter for VFlip {
    fn name(&self) -> &'static str {
        "vflip"
    }

    fn filter(&mut self, src: VideoFrame) -> Result<VideoFrame> {
        ensure_planar_yuv(&src, "vflip")?;
        let mut planes = Vec::with_capacity(src.planes.len());
        let mut strides = Vec::with_capacity(src.planes.len());
        for (i, plane) in src.planes.iter().enumerate() {
            let (pw, ph) = plane_dims(src.format, src.width, src.height, i)?;
            let (pw, ph, stride) = (pw as usize, ph as usize, src.strides[i]);
            let mut out = vec![0u8; pw * ph];
            for y in 0..ph {
                let src_row = &plane[(ph - 1 - y) * stride..(ph - 1 - y) * stride + pw];
                out[y * pw..(y + 1) * pw].copy_from_slice(src_row);
            }
            planes.push(out);
            strides.push(pw);
        }
        Ok(VideoFrame {
            width: src.width,
            height: src.height,
            format: src.format,
            planes,
            strides,
            pts: src.pts,
        })
    }
}

// ---------------------------------------------------------------------------
// transpose (90° rotations)
// ---------------------------------------------------------------------------

/// `transpose=DIR` — a 90° rotation (output dimensions are swapped). `DIR` is
/// `0`/`cclock_flip`, `1`/`clock`, `2`/`cclock`, `3`/`clock_flip` (default
/// `clock`). Requires symmetric chroma (4:2:0 or 4:4:4) so the layout survives.
struct Transpose {
    dir: u8,
}

impl Transpose {
    fn parse(args: &[&str]) -> Result<Transpose> {
        let dir = match args.first().map(|s| s.trim()) {
            None | Some("") => 1, // default: clockwise
            Some("cclock_flip") | Some("0") => 0,
            Some("clock") | Some("1") => 1,
            Some("cclock") | Some("2") => 2,
            Some("clock_flip") | Some("3") => 3,
            Some(other) => {
                return Err(Error::Option(format!(
                    "transpose: unknown direction `{other}`"
                )))
            }
        };
        Ok(Transpose { dir })
    }

    /// Map a destination coordinate back to its source `(x, y)` for plane size
    /// `(w, h)` (the *source* plane dimensions).
    fn source(&self, dx: u32, dy: u32, w: u32, h: u32) -> (u32, u32) {
        match self.dir {
            0 => (dy, dx),                 // transpose (cclock + vflip)
            1 => (dy, h - 1 - dx),         // 90° clockwise
            2 => (w - 1 - dy, dx),         // 90° counter-clockwise
            _ => (w - 1 - dy, h - 1 - dx), // clockwise + vflip
        }
    }
}

impl Filter for Transpose {
    fn name(&self) -> &'static str {
        "transpose"
    }

    fn output_dims(&self, in_w: u32, in_h: u32) -> (u32, u32) {
        (in_h, in_w)
    }

    fn filter(&mut self, src: VideoFrame) -> Result<VideoFrame> {
        ensure_planar_yuv(&src, "transpose")?;
        let (sx, sy) = subsampling(src.format)?;
        if sx != sy {
            return Err(Error::unsupported(
                "transpose: needs symmetric chroma (4:2:0 or 4:4:4)",
            ));
        }
        let mut planes = Vec::with_capacity(src.planes.len());
        let mut strides = Vec::with_capacity(src.planes.len());
        for (i, plane) in src.planes.iter().enumerate() {
            let (pw, ph) = plane_dims(src.format, src.width, src.height, i)?;
            let (dw, dh) = (ph, pw); // dimensions swap
            let stride = src.strides[i];
            let mut out = vec![0u8; (dw * dh) as usize];
            for dy in 0..dh {
                for dx in 0..dw {
                    let (mx, my) = self.source(dx, dy, pw, ph);
                    out[(dy * dw + dx) as usize] = plane[my as usize * stride + mx as usize];
                }
            }
            planes.push(out);
            strides.push(dw as usize);
        }
        Ok(VideoFrame {
            width: src.height,
            height: src.width,
            format: src.format,
            planes,
            strides,
            pts: src.pts,
        })
    }
}

// ---------------------------------------------------------------------------
// pad
// ---------------------------------------------------------------------------

/// `pad=W:H:X:Y` — place the input on a `W×H` black canvas at offset `(X, Y)`.
/// Offsets/size must align to the chroma grid. Fill is full-range black
/// (luma 0, chroma 128).
struct Pad {
    w: u32,
    h: u32,
    x: u32,
    y: u32,
}

impl Pad {
    fn parse(args: &[&str]) -> Result<Pad> {
        let [w, h, x, y] = args else {
            return Err(Error::Option("pad: expected pad=W:H:X:Y".into()));
        };
        let num = |s: &str| {
            s.trim()
                .parse::<u32>()
                .map_err(|_| Error::Option(format!("pad: bad value `{s}`")))
        };
        Ok(Pad {
            w: num(w)?,
            h: num(h)?,
            x: num(x)?,
            y: num(y)?,
        })
    }
}

impl Filter for Pad {
    fn name(&self) -> &'static str {
        "pad"
    }

    fn output_dims(&self, _in_w: u32, _in_h: u32) -> (u32, u32) {
        (self.w, self.h)
    }

    fn filter(&mut self, src: VideoFrame) -> Result<VideoFrame> {
        ensure_planar_yuv(&src, "pad")?;
        if self.x + src.width > self.w || self.y + src.height > self.h {
            return Err(Error::Option(format!(
                "pad: {}x{} input at +{}+{} doesn't fit a {}x{} canvas",
                src.width, src.height, self.x, self.y, self.w, self.h
            )));
        }
        let (sx, sy) = subsampling(src.format)?;
        if self.x % sx != 0 || self.w % sx != 0 || self.y % sy != 0 || self.h % sy != 0 {
            return Err(Error::Option(
                "pad: offset/size must align to the chroma grid".into(),
            ));
        }

        let mut planes = Vec::with_capacity(src.planes.len());
        let mut strides = Vec::with_capacity(src.planes.len());
        for (i, plane) in src.planes.iter().enumerate() {
            let (cw, ch) = plane_dims(src.format, self.w, self.h, i)?; // canvas
            let (sw, sh) = plane_dims(src.format, src.width, src.height, i)?; // source
            let (div_x, div_y) = if i == 0 { (1, 1) } else { (sx, sy) };
            let (px, py) = (self.x / div_x, self.y / div_y);
            let fill = if i == 0 { 0u8 } else { 128u8 }; // black: Y=0, chroma=128

            let (cw, ch) = (cw as usize, ch as usize);
            let mut out = vec![fill; cw * ch];
            for row in 0..sh as usize {
                let dst_start = (py as usize + row) * cw + px as usize;
                let src_start = row * src.strides[i];
                out[dst_start..dst_start + sw as usize]
                    .copy_from_slice(&plane[src_start..src_start + sw as usize]);
            }
            planes.push(out);
            strides.push(cw);
        }
        Ok(VideoFrame {
            width: self.w,
            height: self.h,
            format: src.format,
            planes,
            strides,
            pts: src.pts,
        })
    }
}

// ---------------------------------------------------------------------------
// format (pixel-format / colorspace conversion)
// ---------------------------------------------------------------------------

/// Colour matrix + quantisation range for YUV↔RGB. Coefficients derive from the
/// luma weights `(Kr, Kb)`: BT.601 = (0.299, 0.114), BT.709 = (0.2126, 0.0722).
/// Limited ("TV", Y 16–235 / C 16–240) vs full ("PC", 0–255) range scales Y and
/// chroma accordingly. H.264/AVC video is BT.709 for HD and BT.601 for SD, almost
/// always limited-range — so the BT.601/full default is correct only for full-range
/// sources (PNG/JPEG bridges); pick the right one with `format=rgb24:bt709:limited`.
#[derive(Clone, Copy)]
struct ColorSpec {
    kr: f32,
    kb: f32,
    limited: bool,
}
impl Default for ColorSpec {
    fn default() -> Self {
        ColorSpec {
            kr: 0.299,
            kb: 0.114,
            limited: false,
        } // BT.601 full-range (legacy)
    }
}
impl ColorSpec {
    #[inline]
    fn kg(&self) -> f32 {
        1.0 - self.kr - self.kb
    }
}

/// `format=PIXFMT[:MATRIX][:RANGE]` — convert between `rgb24` and `yuv420p`.
/// MATRIX ∈ {`bt601`, `bt709`}; RANGE ∈ {`full`/`pc`, `limited`/`tv`}. Defaults to
/// BT.601 full range for backward compatibility.
struct FormatConv {
    target: PixelFormat,
    spec: ColorSpec,
}

impl FormatConv {
    fn parse(args: &[&str]) -> Result<FormatConv> {
        let name = args.first().copied().unwrap_or("").trim();
        let target = match name {
            "yuv420p" => PixelFormat::Yuv420p,
            "rgb24" => PixelFormat::Rgb24,
            other => {
                return Err(Error::unsupported(format!(
                    "format: target `{other}` (only yuv420p and rgb24)"
                )))
            }
        };
        let mut spec = ColorSpec::default();
        for opt in args.iter().skip(1).map(|s| s.trim()) {
            match opt {
                "" => {}
                "bt601" | "601" | "smpte170m" => {
                    spec.kr = 0.299;
                    spec.kb = 0.114;
                }
                "bt709" | "709" => {
                    spec.kr = 0.2126;
                    spec.kb = 0.0722;
                }
                "full" | "pc" | "jpeg" => spec.limited = false,
                "limited" | "tv" | "mpeg" => spec.limited = true,
                other => {
                    return Err(Error::unsupported(format!(
                        "format: option `{other}` (matrix bt601/bt709, range full/limited)"
                    )))
                }
            }
        }
        Ok(FormatConv { target, spec })
    }
}

impl Filter for FormatConv {
    fn name(&self) -> &'static str {
        "format"
    }

    fn filter(&mut self, src: VideoFrame) -> Result<VideoFrame> {
        if src.format == self.target {
            return Ok(src);
        }
        match (src.format, self.target) {
            (PixelFormat::Rgb24, PixelFormat::Yuv420p) => rgb_to_yuv420(&src, 3, self.spec),
            (PixelFormat::Rgba, PixelFormat::Yuv420p) => rgb_to_yuv420(&src, 4, self.spec),
            (PixelFormat::Rgba, PixelFormat::Rgb24) => rgba_to_rgb(&src),
            (PixelFormat::Yuv420p, PixelFormat::Rgb24) => yuv420_to_rgb(&src, self.spec),
            (from, to) => Err(Error::unsupported(format!(
                "format: {} → {} not supported",
                from.name(),
                to.name()
            ))),
        }
    }
}

fn clamp_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// Packed RGB(A) → planar 4:2:0 (chroma averaged over each 2×2 block).
fn rgb_to_yuv420(src: &VideoFrame, bpp: usize, spec: ColorSpec) -> Result<VideoFrame> {
    let (w, h) = (src.width as usize, src.height as usize);
    let stride = src.strides[0];
    let rgb = &src.planes[0];
    let px = |i: usize, j: usize| {
        let p = j * stride + i * bpp;
        (rgb[p] as f32, rgb[p + 1] as f32, rgb[p + 2] as f32)
    };

    // Forward coeffs from (Kr, Kb): Y = yoff + yscale·(Kr·R+Kg·G+Kb·B),
    // U = 128 + cscale·(ur·R+ug·G+0.5·B), V = 128 + cscale·(0.5·R+vg·G+vb·B).
    let (kr, kb, kg) = (spec.kr, spec.kb, spec.kg());
    let (yoff, yscale, cscale) = if spec.limited {
        (16.0, 219.0 / 255.0, 224.0 / 255.0)
    } else {
        (0.0, 1.0, 1.0)
    };
    let (ur, ug) = (-0.5 * kr / (1.0 - kb), -0.5 * kg / (1.0 - kb));
    let (vg, vb) = (-0.5 * kg / (1.0 - kr), -0.5 * kb / (1.0 - kr));

    let mut y = vec![0u8; w * h];
    for j in 0..h {
        for i in 0..w {
            let (r, g, b) = px(i, j);
            y[j * w + i] = clamp_u8(yoff + yscale * (kr * r + kg * g + kb * b));
        }
    }

    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let (mut u, mut v) = (vec![0u8; cw * ch], vec![0u8; cw * ch]);
    for cj in 0..ch {
        for ci in 0..cw {
            // Average the (up to) 2×2 RGB block, then convert once.
            let (mut sr, mut sg, mut sb, mut n) = (0.0, 0.0, 0.0, 0.0);
            for dy in 0..2 {
                for dx in 0..2 {
                    let (i, j) = (ci * 2 + dx, cj * 2 + dy);
                    if i < w && j < h {
                        let (r, g, b) = px(i, j);
                        sr += r;
                        sg += g;
                        sb += b;
                        n += 1.0;
                    }
                }
            }
            let (r, g, b) = (sr / n, sg / n, sb / n);
            u[cj * cw + ci] = clamp_u8(128.0 + cscale * (ur * r + ug * g + 0.5 * b));
            v[cj * cw + ci] = clamp_u8(128.0 + cscale * (0.5 * r + vg * g + vb * b));
        }
    }

    Ok(VideoFrame {
        width: src.width,
        height: src.height,
        format: PixelFormat::Yuv420p,
        planes: vec![y, u, v],
        strides: vec![w, cw, cw],
        pts: src.pts,
    })
}

/// Planar 4:2:0 → packed RGB (chroma upsampled nearest-neighbour).
fn yuv420_to_rgb(src: &VideoFrame, spec: ColorSpec) -> Result<VideoFrame> {
    let (w, h) = (src.width as usize, src.height as usize);
    let (ys, us, vs) = (src.strides[0], src.strides[1], src.strides[2]);
    let (yp, up, vp) = (&src.planes[0], &src.planes[1], &src.planes[2]);

    // Derive from (Kr, Kb): R = ya + kr_c·cv, G = ya + kgu·cu + kgv·cv,
    // B = ya + kb_c·cu, where ya = yscale·(Y − yoff), cu = Cu−128, cv = Cv−128.
    let kg = spec.kg();
    let (yoff, yscale, cscale) = if spec.limited {
        (16.0, 255.0 / 219.0, 255.0 / 224.0)
    } else {
        (0.0, 1.0, 1.0)
    };
    let kr_c = 2.0 * (1.0 - spec.kr) * cscale;
    let kb_c = 2.0 * (1.0 - spec.kb) * cscale;
    let kgv = -2.0 * spec.kr * (1.0 - spec.kr) / kg * cscale;
    let kgu = -2.0 * spec.kb * (1.0 - spec.kb) / kg * cscale;

    let mut out = vec![0u8; w * h * 3];
    for j in 0..h {
        for i in 0..w {
            let ya = yscale * (yp[j * ys + i] as f32 - yoff);
            let cu = up[(j / 2) * us + i / 2] as f32 - 128.0;
            let cv = vp[(j / 2) * vs + i / 2] as f32 - 128.0;
            let o = (j * w + i) * 3;
            out[o] = clamp_u8(ya + kr_c * cv);
            out[o + 1] = clamp_u8(ya + kgu * cu + kgv * cv);
            out[o + 2] = clamp_u8(ya + kb_c * cu);
        }
    }
    Ok(VideoFrame {
        width: src.width,
        height: src.height,
        format: PixelFormat::Rgb24,
        planes: vec![out],
        strides: vec![w * 3],
        pts: src.pts,
    })
}

/// Drop the alpha channel: packed RGBA → packed RGB.
fn rgba_to_rgb(src: &VideoFrame) -> Result<VideoFrame> {
    let (w, h) = (src.width as usize, src.height as usize);
    let stride = src.strides[0];
    let rgba = &src.planes[0];
    let mut out = vec![0u8; w * h * 3];
    for j in 0..h {
        for i in 0..w {
            let s = j * stride + i * 4;
            let d = (j * w + i) * 3;
            out[d..d + 3].copy_from_slice(&rgba[s..s + 3]);
        }
    }
    Ok(VideoFrame {
        width: src.width,
        height: src.height,
        format: PixelFormat::Rgb24,
        planes: vec![out],
        strides: vec![w * 3],
        pts: src.pts,
    })
}

// ---------------------------------------------------------------------------
// negate / grayscale
// ---------------------------------------------------------------------------

/// Repack a plane into a tight buffer, applying `f` to every sample.
fn map_plane(plane: &[u8], stride: usize, pw: usize, ph: usize, f: impl Fn(u8) -> u8) -> Vec<u8> {
    let mut out = vec![0u8; pw * ph];
    for y in 0..ph {
        let row = &plane[y * stride..y * stride + pw];
        let dst = &mut out[y * pw..(y + 1) * pw];
        for x in 0..pw {
            dst[x] = f(row[x]);
        }
    }
    out
}

/// `negate` — invert every sample (`255 - v`), luma and chroma.
struct Negate;

impl Filter for Negate {
    fn name(&self) -> &'static str {
        "negate"
    }

    fn filter(&mut self, src: VideoFrame) -> Result<VideoFrame> {
        ensure_planar_yuv(&src, "negate")?;
        let mut planes = Vec::with_capacity(src.planes.len());
        let mut strides = Vec::with_capacity(src.planes.len());
        for (i, plane) in src.planes.iter().enumerate() {
            let (pw, ph) = plane_dims(src.format, src.width, src.height, i)?;
            planes.push(map_plane(
                plane,
                src.strides[i],
                pw as usize,
                ph as usize,
                |v| 255 - v,
            ));
            strides.push(pw as usize);
        }
        Ok(VideoFrame {
            planes,
            strides,
            ..src
        })
    }
}

/// `grayscale` — keep luma, neutralize chroma (set both chroma planes to 128).
struct Grayscale;

impl Filter for Grayscale {
    fn name(&self) -> &'static str {
        "grayscale"
    }

    fn filter(&mut self, src: VideoFrame) -> Result<VideoFrame> {
        ensure_planar_yuv(&src, "grayscale")?;
        let mut planes = Vec::with_capacity(src.planes.len());
        let mut strides = Vec::with_capacity(src.planes.len());
        for (i, plane) in src.planes.iter().enumerate() {
            let (pw, ph) = plane_dims(src.format, src.width, src.height, i)?;
            let (pw, ph) = (pw as usize, ph as usize);
            if i == 0 {
                planes.push(map_plane(plane, src.strides[i], pw, ph, |v| v));
            } else {
                planes.push(vec![128u8; pw * ph]);
            }
            strides.push(pw);
        }
        Ok(VideoFrame {
            planes,
            strides,
            ..src
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A W×H 4:2:0 frame with a per-pixel luma function and flat chroma.
    fn frame(w: u32, h: u32, luma: impl Fn(u32, u32) -> u8) -> VideoFrame {
        let (wi, hi) = (w as usize, h as usize);
        let mut y = vec![0u8; wi * hi];
        for row in 0..h {
            for col in 0..w {
                y[row as usize * wi + col as usize] = luma(col, row);
            }
        }
        let chroma = vec![128u8; (wi / 2) * (hi / 2)];
        VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Yuv420p,
            planes: vec![y, chroma.clone(), chroma],
            strides: vec![wi, wi / 2, wi / 2],
            pts: Some(0),
        }
    }

    #[test]
    fn scale_changes_dimensions_and_keeps_planes_consistent() {
        let mut chain = FilterChain::parse("scale=32:16").unwrap();
        assert_eq!(chain.output_dims(64, 64), (32, 16));
        let out = chain.apply(frame(64, 64, |x, _| x as u8)).unwrap();
        assert_eq!((out.width, out.height), (32, 16));
        // Y plane is dw*dh; chroma is half each way.
        assert_eq!(out.planes[0].len(), 32 * 16);
        assert_eq!(out.planes[1].len(), 16 * 8);
        assert_eq!(out.strides, vec![32, 16, 16]);
    }

    #[test]
    fn scale_preserves_a_flat_field_exactly() {
        // A constant plane must resample to the same constant (no ringing).
        let mut chain = FilterChain::parse("scale=20:30").unwrap();
        let out = chain.apply(frame(64, 48, |_, _| 200)).unwrap();
        assert!(out.planes[0].iter().all(|&v| v == 200));
    }

    #[test]
    fn crop_extracts_the_right_rectangle() {
        // Luma encodes the column index, so a crop at x=8 should start at 8.
        let mut chain = FilterChain::parse("crop=16:16:8:4").unwrap();
        assert_eq!(chain.output_dims(64, 64), (16, 16));
        let out = chain.apply(frame(64, 64, |x, _| x as u8)).unwrap();
        assert_eq!((out.width, out.height), (16, 16));
        assert_eq!(out.planes[0][0], 8); // top-left came from source column 8
        assert_eq!(out.planes[0][15], 23); // 8 + 15
    }

    #[test]
    fn crop_rejects_out_of_bounds_and_misalignment() {
        let mut oob = FilterChain::parse("crop=64:64:8:8").unwrap();
        assert!(oob.apply(frame(64, 64, |_, _| 0)).is_err());
        let mut odd = FilterChain::parse("crop=15:15:1:1").unwrap();
        assert!(odd.apply(frame(64, 64, |_, _| 0)).is_err());
    }

    #[test]
    fn rejects_unknown_filter() {
        assert!(FilterChain::parse("frobnicate=1").is_err());
    }

    #[test]
    fn negate_inverts_samples() {
        let mut chain = FilterChain::parse("negate").unwrap();
        let out = chain.apply(frame(4, 4, |x, _| x as u8)).unwrap();
        assert_eq!(out.planes[0][0], 255); // 255 - 0
        assert_eq!(out.planes[0][3], 252); // 255 - 3
        assert_eq!(out.planes[1][0], 127); // chroma 128 → 127
    }

    #[test]
    fn grayscale_neutralizes_chroma() {
        // Frame with saturated chroma; grayscale must flatten it to 128.
        let mut f = frame(8, 8, |x, _| x as u8);
        for c in [1, 2] {
            f.planes[c].iter_mut().for_each(|p| *p = 200);
        }
        let luma_before = f.planes[0].clone();
        let mut chain = FilterChain::parse("grayscale").unwrap();
        let out = chain.apply(f).unwrap();
        assert_eq!(out.planes[0], luma_before); // luma untouched
        assert!(out.planes[1].iter().all(|&v| v == 128));
        assert!(out.planes[2].iter().all(|&v| v == 128));
    }

    #[test]
    fn scale_minus_one_preserves_aspect() {
        let chain = FilterChain::parse("scale=-1:32").unwrap();
        // 64×48 → width = 64*32/48 = 42 (rounded even), height = 32.
        assert_eq!(chain.output_dims(64, 48), (42, 32));
        assert!(FilterChain::parse("scale=-1:-1").is_err());
    }

    #[test]
    fn hflip_mirrors_columns() {
        let mut chain = FilterChain::parse("hflip").unwrap();
        let out = chain.apply(frame(8, 4, |x, _| x as u8)).unwrap();
        assert_eq!((out.width, out.height), (8, 4));
        assert_eq!(out.planes[0][0], 7); // first column came from the last
        assert_eq!(out.planes[0][7], 0);
    }

    #[test]
    fn vflip_mirrors_rows() {
        let mut chain = FilterChain::parse("vflip").unwrap();
        let out = chain.apply(frame(4, 8, |_, y| y as u8)).unwrap();
        assert_eq!(out.planes[0][0], 7); // top row came from the bottom
        assert_eq!(out.planes[0][7 * 4], 0);
    }

    #[test]
    fn transpose_clock_then_cclock_is_identity() {
        // A unique value per pixel; clockwise then counter-clockwise must
        // restore the original exactly (no interpolation involved).
        let original = frame(8, 4, |x, y| (x * 4 + y) as u8);
        let expected = original.planes[0].clone();
        let mut chain = FilterChain::parse("transpose=clock,transpose=cclock").unwrap();
        let out = chain.apply(original).unwrap();
        assert_eq!((out.width, out.height), (8, 4));
        assert_eq!(out.planes[0], expected);
    }

    #[test]
    fn transpose_swaps_dimensions() {
        let chain = FilterChain::parse("transpose=1").unwrap();
        assert_eq!(chain.output_dims(96, 64), (64, 96));
    }

    #[test]
    fn format_rgb_yuv_roundtrip_is_close() {
        // A smooth RGB gradient survives RGB→YUV420→RGB within tolerance.
        let (w, h) = (32usize, 32usize);
        let mut rgb = vec![0u8; w * h * 3];
        for j in 0..h {
            for i in 0..w {
                let o = (j * w + i) * 3;
                rgb[o] = (i * 255 / (w - 1)) as u8; // R ramps across
                rgb[o + 1] = (j * 255 / (h - 1)) as u8; // G ramps down
                rgb[o + 2] = 96;
            }
        }
        let src = VideoFrame {
            width: w as u32,
            height: h as u32,
            format: PixelFormat::Rgb24,
            planes: vec![rgb.clone()],
            strides: vec![w * 3],
            pts: Some(0),
        };

        let mut to_yuv = FilterChain::parse("format=yuv420p").unwrap();
        let yuv = to_yuv.apply(src).unwrap();
        assert_eq!(yuv.format, PixelFormat::Yuv420p);
        assert_eq!(yuv.planes.len(), 3);

        let mut to_rgb = FilterChain::parse("format=rgb24").unwrap();
        let back = to_rgb.apply(yuv).unwrap();
        assert_eq!(back.format, PixelFormat::Rgb24);

        let total: u64 = rgb
            .iter()
            .zip(&back.planes[0])
            .map(|(a, b)| (*a as i16 - *b as i16).unsigned_abs() as u64)
            .sum();
        let mean = total as f64 / (w * h * 3) as f64;
        assert!(mean < 8.0, "rgb↔yuv round-trip drifted too far: {mean:.2}");
    }

    #[test]
    fn colorspace_matrices_and_range() {
        // A 2×2 solid RGB frame (chroma-subsample averages to the same value → exact).
        let solid = |r: u8, g: u8, b: u8| VideoFrame {
            width: 2,
            height: 2,
            format: PixelFormat::Rgb24,
            planes: vec![[r, g, b].repeat(4)],
            strides: vec![6],
            pts: Some(0),
        };
        let to_yuv = |opts: &str, f: VideoFrame| {
            let mut c = FilterChain::parse(&format!("format=yuv420p{opts}")).unwrap();
            let y = c.apply(f).unwrap();
            (y.planes[0][0], y.planes[1][0], y.planes[2][0])
        };
        // Range anchors (matrix-independent): limited white→235/black→16; full→255/0.
        assert_eq!(to_yuv(":limited", solid(255, 255, 255)).0, 235);
        assert_eq!(to_yuv(":limited", solid(0, 0, 0)).0, 16);
        assert_eq!(to_yuv(":full", solid(255, 255, 255)).0, 255);
        assert_eq!(to_yuv(":full", solid(0, 0, 0)).0, 0);
        // Matrix matters: pure-red luma is higher in BT.601 (0.299) than BT.709 (0.2126).
        let y601 = to_yuv(":bt601:full", solid(255, 0, 0)).0;
        let y709 = to_yuv(":bt709:full", solid(255, 0, 0)).0;
        assert!(y601 > y709, "601 red-luma {y601} should exceed 709 {y709}");
        // Greyscale is chroma-neutral (U=V=128) in every mode (luma scales with range).
        for m in [":full", ":limited", ":bt709:limited"] {
            let (_, u, v) = to_yuv(m, solid(128, 128, 128));
            assert_eq!((u, v), (128, 128), "grey chroma ({m})");
        }
        // Each colorspace round-trips tightly (the decode-display path).
        for m in [":bt709:limited", ":bt601:limited", ":bt709:full"] {
            let mut to_y = FilterChain::parse(&format!("format=yuv420p{m}")).unwrap();
            let mut to_r = FilterChain::parse(&format!("format=rgb24{m}")).unwrap();
            let src = solid(200, 100, 50);
            let back = to_r.apply(to_y.apply(src.clone()).unwrap()).unwrap();
            let drift: u32 = src.planes[0]
                .iter()
                .zip(&back.planes[0])
                .map(|(a, b)| (*a as i16 - *b as i16).unsigned_abs() as u32)
                .sum::<u32>()
                / 12;
            assert!(drift < 6, "round-trip drift for {m}: {drift}");
        }
    }

    #[test]
    fn pad_centers_input_on_black_canvas() {
        let mut chain = FilterChain::parse("pad=16:16:4:4").unwrap();
        assert_eq!(chain.output_dims(8, 8), (16, 16));
        let out = chain.apply(frame(8, 8, |_, _| 200)).unwrap();
        assert_eq!((out.width, out.height), (16, 16));
        assert_eq!(out.planes[0][0], 0); // border luma is black
        assert_eq!(out.planes[0][4 * 16 + 4], 200); // input top-left at (4,4)
        assert_eq!(out.planes[1][0], 128); // border chroma is neutral
        assert!(chain.apply(frame(64, 64, |_, _| 0)).is_err()); // doesn't fit
    }

    #[test]
    fn filter_complex_parses_overlay_offset() {
        let fc = FilterComplex::parse("[0:v][1:v]overlay=16:8[out]").unwrap();
        assert_eq!(fc.overlay, Some((16, 8)));
        // Bare overlay (no offset) defaults to the top-left corner.
        assert_eq!(
            FilterComplex::parse("overlay").unwrap().overlay,
            Some((0, 0))
        );
        assert!(FilterComplex::parse("[0][1]hstack").is_err());
    }

    #[test]
    fn overlay_composites_at_offset() {
        // Base luma = 10 everywhere; overlay luma = 200 everywhere. After an
        // overlay at (8, 4), the 8×8 patch there must read 200, elsewhere 10.
        let base = frame(32, 32, |_, _| 10);
        let over = frame(8, 8, |_, _| 200);
        let out = overlay(base, &over, 8, 4).unwrap();
        assert_eq!((out.width, out.height), (32, 32));
        assert_eq!(out.planes[0][4 * 32 + 8], 200); // top-left of the patch
        assert_eq!(out.planes[0][11 * 32 + 15], 200); // bottom-right of the patch
        assert_eq!(out.planes[0][0], 10); // outside, untouched
        assert_eq!(out.planes[0][12 * 32 + 8], 10); // one row below the patch
    }

    #[test]
    fn overlay_clips_to_base_bounds() {
        // An overlay placed near the corner spills past the edge; the copy must
        // clip rather than panic, and the in-bounds corner still lands.
        let base = frame(16, 16, |_, _| 0);
        let over = frame(8, 8, |_, _| 99);
        let out = overlay(base, &over, 12, 12).unwrap();
        assert_eq!(out.planes[0][12 * 16 + 12], 99); // the 4×4 visible corner
        assert_eq!(out.planes[0][15 * 16 + 15], 99);
    }
}
