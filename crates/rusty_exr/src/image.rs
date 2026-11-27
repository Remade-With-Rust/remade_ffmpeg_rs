//! Decoded images: channels as flat planes.

use crate::half::f16_to_f32;
use crate::header::{AttributeValue, Box2i, ChannelDesc, Header, SampleType};

/// One channel's samples, row-major over its sampled grid.
#[derive(Debug, Clone, PartialEq)]
pub enum Samples {
    /// Half floats as raw bit patterns (exact; see [`crate::f16_to_f32`]).
    F16(Vec<u16>),
    F32(Vec<f32>),
    U32(Vec<u32>),
}

impl Samples {
    /// Number of samples.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::F16(v) => v.len(),
            Self::F32(v) => v.len(),
            Self::U32(v) => v.len(),
        }
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sample type.
    #[must_use]
    pub fn sample_type(&self) -> SampleType {
        match self {
            Self::F16(_) => SampleType::F16,
            Self::F32(_) => SampleType::F32,
            Self::U32(_) => SampleType::U32,
        }
    }

    /// Sample `i` as `f32` (U32 converts numerically).
    #[must_use]
    pub fn get_f32(&self, i: usize) -> f32 {
        match self {
            Self::F16(v) => f16_to_f32(v[i]),
            Self::F32(v) => v[i],
            Self::U32(v) => v[i] as f32,
        }
    }

    /// All samples as `f32`.
    #[must_use]
    pub fn to_f32(&self) -> Vec<f32> {
        match self {
            Self::F16(v) => v.iter().map(|&h| f16_to_f32(h)).collect(),
            Self::F32(v) => v.clone(),
            Self::U32(v) => v.iter().map(|&u| u as f32).collect(),
        }
    }
}

/// A channel with its description and samples.
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub desc: ChannelDesc,
    /// Samples per row (data-window width divided by x sampling).
    pub width: usize,
    /// Sample rows.
    pub height: usize,
    pub samples: Samples,
}

impl Channel {
    /// A full-resolution channel from samples. `samples.len()` must be
    /// `width * height`.
    #[must_use]
    pub fn new(name: impl Into<String>, width: usize, height: usize, samples: Samples) -> Self {
        Self {
            desc: ChannelDesc::new(name, samples.sample_type()),
            width,
            height,
            samples,
        }
    }

    /// The channel's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.desc.name
    }
}

/// One part (or one level of a tiled part), decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    /// The part's header (all attributes, typed where standard).
    pub header: Header,
    /// The data window (for levels > 0: the level's window).
    pub data_window: Box2i,
    /// Which resolution level this is.
    pub level: (usize, usize),
    /// Channels in channel-list (sorted) order.
    pub channels: Vec<Channel>,
}

impl Image {
    /// An empty `width` x `height` image (data and display window at the
    /// origin) to add channels to before writing.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let mut header = Header::default();
        header.set(
            "displayWindow",
            AttributeValue::Box2i(Box2i::from_size(width, height)),
        );
        Self {
            header,
            data_window: Box2i::from_size(width, height),
            level: (0, 0),
            channels: Vec::new(),
        }
    }

    /// Add a full-resolution channel (builder style).
    #[must_use]
    pub fn with_channel(mut self, name: impl Into<String>, samples: Samples) -> Self {
        let (w, h) = (self.width(), self.height());
        self.channels.push(Channel::new(name, w, h, samples));
        self
    }

    /// Data-window width.
    #[must_use]
    pub fn width(&self) -> usize {
        self.data_window.width().max(0) as usize
    }

    /// Data-window height.
    #[must_use]
    pub fn height(&self) -> usize {
        self.data_window.height().max(0) as usize
    }

    /// The channel with this exact name.
    #[must_use]
    pub fn channel(&self, name: &str) -> Option<&Channel> {
        self.channels.iter().find(|c| c.desc.name == name)
    }

    /// Layer names (the part of channel names before the last `.`, empty
    /// for the base layer), in first-seen order.
    #[must_use]
    pub fn layers(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in &self.channels {
            let l = c.desc.name.rfind('.').map_or("", |i| &c.desc.name[..i]);
            if !out.iter().any(|x| x == l) {
                out.push(l.to_string());
            }
        }
        out
    }

    /// Interleaved RGBA `f32` over the data window for a layer (`""` is the
    /// base layer, `"diffuse"` reads `diffuse.R` ...). Missing alpha is 1.
    /// A layer with `Y` but no `R`/`G`/`B` is read as grey (`Y` into all
    /// three). Returns `None` when the layer has neither colour nor `Y`, or
    /// a channel is subsampled.
    #[must_use]
    pub fn to_rgba_f32(&self, layer: &str) -> Option<Vec<f32>> {
        let name = |s: &str| {
            if layer.is_empty() {
                s.to_string()
            } else {
                format!("{layer}.{s}")
            }
        };
        let get = |s: &str| {
            self.channel(&name(s))
                .filter(|c| c.desc.x_sampling == 1 && c.desc.y_sampling == 1)
        };
        let (w, h) = (self.width(), self.height());
        let n = w * h;
        let (r, g, b) = if let (Some(r), Some(g), Some(b)) = (get("R"), get("G"), get("B")) {
            (r, g, b)
        } else {
            let y = get("Y")?;
            (y, y, y)
        };
        let a = get("A");
        // Every plane must hold at least `n` samples (extra ones are
        // ignored), as indexing sample by sample required.
        for c in [Some(r), Some(g), Some(b), a].into_iter().flatten() {
            assert!(c.samples.len() >= n, "channel {} too short", c.desc.name);
        }
        let mut out = vec![0f32; n * 4];
        if let (Samples::F16(rv), Samples::F16(gv), Samples::F16(bv)) =
            (&r.samples, &g.samples, &b.samples)
        {
            let av = match a.map(|a| &a.samples) {
                None => Some(None),
                Some(Samples::F16(v)) => Some(Some(&v[..n])),
                Some(_) => None,
            };
            if let Some(av) = av {
                let isa = crate::simd::isa();
                if crate::simd::rgba_f16(isa, &rv[..n], &gv[..n], &bv[..n], av, &mut out) {
                    return Some(out);
                }
            }
        }
        for (k, c) in [r, g, b].into_iter().enumerate() {
            put_strided(&c.samples, n, k, &mut out);
        }
        match a {
            Some(a) => put_strided(&a.samples, n, 3, &mut out),
            None => out.chunks_exact_mut(4).for_each(|px| px[3] = 1.0),
        }
        Some(out)
    }

    /// The per-sample formulation of [`Self::to_rgba_f32`] (the oracle).
    #[cfg(test)]
    pub(crate) fn to_rgba_f32_reference(&self, layer: &str) -> Option<Vec<f32>> {
        let name = |s: &str| {
            if layer.is_empty() {
                s.to_string()
            } else {
                format!("{layer}.{s}")
            }
        };
        let get = |s: &str| {
            self.channel(&name(s))
                .filter(|c| c.desc.x_sampling == 1 && c.desc.y_sampling == 1)
        };
        let n = self.width() * self.height();
        let (r, g, b) = if let (Some(r), Some(g), Some(b)) = (get("R"), get("G"), get("B")) {
            (r, g, b)
        } else {
            let y = get("Y")?;
            (y, y, y)
        };
        let a = get("A");
        let mut out = vec![0f32; n * 4];
        for i in 0..n {
            out[i * 4] = r.samples.get_f32(i);
            out[i * 4 + 1] = g.samples.get_f32(i);
            out[i * 4 + 2] = b.samples.get_f32(i);
            out[i * 4 + 3] = a.map_or(1.0, |a| a.samples.get_f32(i));
        }
        Some(out)
    }
}

/// Write the first `n` samples of `s`, as `f32`, to every fourth slot of
/// `out` starting at `k`.
fn put_strided(s: &Samples, n: usize, k: usize, out: &mut [f32]) {
    let px = out.chunks_exact_mut(4);
    match s {
        Samples::F16(v) => px.zip(&v[..n]).for_each(|(p, &h)| p[k] = f16_to_f32(h)),
        Samples::F32(v) => px.zip(&v[..n]).for_each(|(p, &f)| p[k] = f),
        Samples::U32(v) => px.zip(&v[..n]).for_each(|(p, &u)| p[k] = u as f32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_matches_reference() {
        let mut seed = 0x0bad_5eedu64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let types = [SampleType::F16, SampleType::F32, SampleType::U32];
        for (w, h) in [
            (1u32, 1u32),
            (3, 1),
            (7, 3),
            (8, 1),
            (9, 2),
            (16, 3),
            (33, 5),
        ] {
            let n = (w * h) as usize;
            for mask in 0..81 {
                let mut img = Image::new(w, h);
                let mut t = mask;
                for name in ["A", "B", "G", "R"] {
                    let ty = types[t % 3];
                    t /= 3;
                    let s = match ty {
                        // Every half pattern class appears, NaNs included.
                        SampleType::F16 => Samples::F16((0..n).map(|_| next() as u16).collect()),
                        SampleType::F32 => {
                            Samples::F32((0..n).map(|_| f32::from_bits(next() as u32)).collect())
                        }
                        SampleType::U32 => Samples::U32((0..n).map(|_| next() as u32).collect()),
                    };
                    img = img.with_channel(name, s);
                }
                let want = img.to_rgba_f32_reference("").unwrap();
                let got = img.to_rgba_f32("").unwrap();
                assert!(
                    want.iter()
                        .zip(&got)
                        .all(|(a, b)| a.to_bits() == b.to_bits()),
                    "{w}x{h} {mask}"
                );
                // Without alpha.
                img.channels.retain(|c| c.desc.name != "A");
                let want = img.to_rgba_f32_reference("").unwrap();
                let got = img.to_rgba_f32("").unwrap();
                assert!(
                    want.iter()
                        .zip(&got)
                        .all(|(a, b)| a.to_bits() == b.to_bits()),
                    "{w}x{h} {mask} no A"
                );
            }
        }
    }

    #[test]
    fn rgba_twins_match_scalar() {
        let mut seed = 0x1234_5678u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u16
        };
        for n in [0usize, 1, 3, 4, 5, 8, 9, 15, 16, 17, 64, 1001] {
            let planes: Vec<Vec<u16>> = (0..4).map(|_| (0..n).map(|_| next()).collect()).collect();
            for alpha in [false, true] {
                let mut want = vec![0f32; 4 * n];
                for k in 0..3 {
                    put_strided(&Samples::F16(planes[k].clone()), n, k, &mut want);
                }
                if alpha {
                    put_strided(&Samples::F16(planes[3].clone()), n, 3, &mut want);
                } else {
                    want.chunks_exact_mut(4).for_each(|p| p[3] = 1.0);
                }
                let a = alpha.then_some(&planes[3][..]);
                for isa in crate::simd::available() {
                    let mut got = vec![0f32; 4 * n];
                    assert!(crate::simd::rgba_f16(
                        isa, &planes[0], &planes[1], &planes[2], a, &mut got
                    ));
                    assert!(
                        want.iter()
                            .zip(&got)
                            .all(|(x, y)| x.to_bits() == y.to_bits()),
                        "{isa:?} n {n}"
                    );
                }
            }
        }
    }
}
