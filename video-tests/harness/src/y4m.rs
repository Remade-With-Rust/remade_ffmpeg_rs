//! Minimal YUV4MPEG2 reader — the corpus format.
//!
//! y4m rather than raw `.yuv` on purpose: the file carries its own dimensions
//! and frame rate, so our in-process encoder and the external libvpx/ffmpeg
//! binaries (which all read y4m natively) are guaranteed to be fed the identical
//! pixels, with no dimension-mismatch class of bug between the arms.

/// One 4:2:0 8-bit frame, planes contiguous at full width (no padding).
pub struct Yuv {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

pub struct Clip {
    pub width: usize,
    pub height: usize,
    /// Frame rate as (numerator, denominator).
    pub fps: (u32, u32),
    pub frames: Vec<Yuv>,
}

impl Clip {
    pub fn fps_f64(&self) -> f64 {
        self.fps.0 as f64 / self.fps.1.max(1) as f64
    }
    pub fn pixels(&self) -> u64 {
        (self.width * self.height * self.frames.len()) as u64
    }
    /// Duration in seconds at the clip's own frame rate (for kb/s).
    pub fn seconds(&self) -> f64 {
        self.frames.len() as f64 / self.fps_f64().max(1e-9)
    }
}

/// Read a whole y4m file into memory. `limit` caps the frame count (0 = all).
pub fn read(path: &std::path::Path, limit: usize) -> Result<Clip, String> {
    let raw = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let nl = raw
        .iter()
        .position(|&b| b == b'\n')
        .ok_or("no y4m header terminator")?;
    let hdr = std::str::from_utf8(&raw[..nl]).map_err(|_| "non-utf8 y4m header")?;
    if !hdr.starts_with("YUV4MPEG2") {
        return Err(format!("not a y4m file: {hdr:.40}"));
    }

    let (mut w, mut h, mut fps) = (0usize, 0usize, (30u32, 1u32));
    for tag in hdr.split_whitespace().skip(1) {
        let (k, v) = tag.split_at(1);
        match k {
            "W" => w = v.parse().map_err(|_| "bad W")?,
            "H" => h = v.parse().map_err(|_| "bad H")?,
            "F" => {
                let (n, d) = v.split_once(':').ok_or("bad F")?;
                fps = (
                    n.parse().map_err(|_| "bad F num")?,
                    d.parse().map_err(|_| "bad F den")?,
                );
            }
            "C" if !v.starts_with("420") => {
                return Err(format!("chroma {v} unsupported (4:2:0 only)"))
            }
            _ => {}
        }
    }
    if w == 0 || h == 0 {
        return Err("y4m header missing W/H".into());
    }

    let (ys, cs) = (w * h, w.div_ceil(2) * h.div_ceil(2));
    let fsz = ys + 2 * cs;
    let mut frames = Vec::new();
    let mut p = nl + 1;
    while p < raw.len() {
        // Each frame is preceded by a "FRAME[ params]\n" line.
        let fnl = raw[p..]
            .iter()
            .position(|&b| b == b'\n')
            .ok_or("truncated frame header")?
            + p;
        if &raw[p..p + 5.min(raw.len() - p)] != b"FRAME" {
            return Err(format!("expected FRAME at byte {p}"));
        }
        p = fnl + 1;
        if p + fsz > raw.len() {
            break; // trailing partial frame (a range-fetched prefix cut short)
        }
        frames.push(Yuv {
            y: raw[p..p + ys].to_vec(),
            u: raw[p + ys..p + ys + cs].to_vec(),
            v: raw[p + ys + cs..p + fsz].to_vec(),
        });
        p += fsz;
        if limit > 0 && frames.len() >= limit {
            break;
        }
    }
    if frames.is_empty() {
        return Err("no frames decoded".into());
    }
    Ok(Clip {
        width: w,
        height: h,
        fps,
        frames,
    })
}

/// Write a y4m holding the first `limit` frames (0 = all) of `clip`.
///
/// The external arms are handed THIS file rather than the original, so every
/// encoder sees exactly the frame count our in-process encoder saw — a clip
/// truncated by `--frames` on one side and not the other silently compares
/// different work.
pub fn write_prefix(clip: &Clip, path: &std::path::Path, limit: usize) -> Result<(), String> {
    let n = if limit == 0 {
        clip.frames.len()
    } else {
        limit.min(clip.frames.len())
    };
    let mut out = Vec::with_capacity(n * clip.frames[0].y.len() * 3 / 2 + 4096);
    out.extend_from_slice(
        format!(
            "YUV4MPEG2 W{} H{} F{}:{} Ip A1:1 C420jpeg\n",
            clip.width, clip.height, clip.fps.0, clip.fps.1
        )
        .as_bytes(),
    );
    for f in clip.frames.iter().take(n) {
        out.extend_from_slice(b"FRAME\n");
        out.extend_from_slice(&f.y);
        out.extend_from_slice(&f.u);
        out.extend_from_slice(&f.v);
    }
    std::fs::write(path, out).map_err(|e| format!("write {}: {e}", path.display()))
}
