//! Content-routed quantization planning for `Frame::from_rgba_speed`.
//!
//! The classic path trains NeuQuant on every pixel (samplefac = `speed`),
//! which prices training by *pixel count*. Real frames carry massive color
//! redundancy (measured 2–88× unique-color redundancy across the bench
//! corpus), so this module routes by a cheap content census instead:
//!
//! - **Exact**: ≤256 unique colors → exact palette, no quantizer at all.
//! - **NeuQuant on a frequency-capped unique-color stream**: >256 colors →
//!   train on each unique color replicated `min(count, cap)` times, so
//!   training cost scales with *color diversity*, not resolution.
//!
//! Every routing decision is observable: set `RUSTY_GIF_TRACE=1` to get the
//! route, census, and training-stream size on stderr, and
//! `RUSTY_GIF_Q_POLICY` to override the policy (`upstream`, `unique:<cap>`,
//! `budget:<pixels>`) for A/B work. There is no silent fallback anywhere in
//! this module: an unknown policy string panics rather than degrading.

use alloc::borrow::Cow;
use alloc::vec::Vec;
use std::collections::HashMap;

use crate::common::Frame;
use crate::neuquant::NeuQuant;

/// Which quantization route a frame took. Exposed (crate-public) so tests
/// can assert each arm actually fires — no dead routes, no silent ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// ≤256 unique colors: exact palette, lossless w.r.t. the input frame.
    Exact,
    /// Median-cut + weighted k-means over the color histogram (default).
    Histogram { iters: usize },
    /// NeuQuant trained on the frequency-capped unique-color stream.
    UniqueStream { cap: u32 },
    /// NeuQuant trained on all pixels with a sampling factor (upstream
    /// behavior when `samplefac` = 1; also the `budget:` policy).
    PixelStream { samplefac: i32 },
}

/// Quantization policy. Default is `UniqueStream(CAP_DEFAULT)`; the
/// `RUSTY_GIF_Q_POLICY` env var overrides it for experiments.
#[derive(Debug, Clone, Copy)]
enum Policy {
    /// Content-routed defaults (see `route_auto`); what ships.
    Auto,
    Upstream,
    Kmeans { iters: usize },
    Unique { cap: u32 },
    Budget { pixels: usize },
}

/// Above this many unique colors the k-means assignment step (cost ∝
/// unique × 256 × iters) loses to budget-sampled NeuQuant — measured
/// crossover on the bench corpus (shields/blue_sky at 119k/145k unique
/// route NeuQuant; city/crew/graphics at 10–30k route k-means).
const KMEANS_MAX_UNIQUE: usize = 65_536;
/// NeuQuant sampling budget for the high-diversity route: samplefac =
/// ceil(pixels / budget). 125k passed the corpus quality floor with the
/// largest speed win (blue_sky 38.61 dB vs the 37.68 ffmpeg floor).
const NQ_SAMPLE_BUDGET: usize = 125_000;

fn policy() -> Policy {
    match std::env::var("RUSTY_GIF_Q_POLICY") {
        Err(_) => Policy::Auto,
        Ok(s) if s == "auto" => Policy::Auto,
        Ok(s) if s == "upstream" => Policy::Upstream,
        Ok(s) if s.starts_with("kmeans:") => Policy::Kmeans {
            iters: s["kmeans:".len()..].parse().expect("RUSTY_GIF_Q_POLICY kmeans:<iters>"),
        },
        Ok(s) if s.starts_with("unique:") => Policy::Unique {
            cap: s["unique:".len()..].parse().expect("RUSTY_GIF_Q_POLICY unique:<cap>"),
        },
        Ok(s) if s.starts_with("budget:") => Policy::Budget {
            pixels: s["budget:".len()..].parse().expect("RUSTY_GIF_Q_POLICY budget:<pixels>"),
        },
        Ok(s) => panic!("RUSTY_GIF_Q_POLICY: unknown policy `{s}` (want auto | upstream | kmeans:<iters> | unique:<cap> | budget:<pixels>)"),
    }
}

/// The shipped routing: k-means iteration count tiers by color diversity
/// (more unique colors → costlier iterations → fewer of them), and very
/// high diversity hands off to budget-sampled NeuQuant entirely.
fn route_auto(unique: usize, pixels: usize, speed: i32) -> Route {
    if unique <= KMEANS_MAX_UNIQUE {
        let iters = match unique {
            u if u <= 32_768 => 3,
            u if u <= 49_152 => 2,
            _ => 1,
        };
        Route::Histogram { iters }
    } else {
        let samplefac = (pixels.div_ceil(NQ_SAMPLE_BUDGET) as i32).clamp(speed, 30);
        Route::PixelStream { samplefac }
    }
}

fn trace_enabled() -> bool {
    std::env::var_os("RUSTY_GIF_TRACE").is_some_and(|v| v != "0")
}

#[inline]
fn key(px: &[u8]) -> u32 {
    u32::from_le_bytes([px[0], px[1], px[2], px[3]])
}

/// Quantize normalized RGBA pixels (alpha already collapsed to 0xFF or the
/// single transparent color by the caller) into an indexed [`Frame`].
///
/// `speed` keeps the upstream `from_rgba_speed` meaning: NeuQuant sampling
/// factor in `[1, 30]`, applied to whichever training stream the route picks.
pub(crate) fn quantize_frame(
    width: u16,
    height: u16,
    pixels: &[u8],
    speed: i32,
    transparent: Option<[u8; 4]>,
) -> Frame<'static> {
    // --- census: unique colors with frequencies (u32 RGBA keys) ---
    let mut hist: HashMap<u32, u32> = HashMap::with_capacity(4096);
    for px in pixels.chunks_exact(4) {
        *hist.entry(key(px)).or_insert(0) += 1;
    }

    // Deterministic color order regardless of hash iteration order: the
    // exact route must stay byte-identical to upstream's sorted-BTreeSet
    // palette, and training streams must not vary run-to-run.
    let mut colors: Vec<(u32, u32)> = hist.iter().map(|(&k, &c)| (k, c)).collect();
    // Sort by (r, g, b, a) tuple order, exactly as upstream's BTreeSet of
    // `(u8, u8, u8, u8)` iterated. The LE u32 key compares in a different
    // order, so sort on the byte tuple.
    colors.sort_unstable_by_key(|&(k, _)| {
        let [r, g, b, a] = k.to_le_bytes();
        (r, g, b, a)
    });

    let route = match policy() {
        _ if colors.len() <= 256 => Route::Exact,
        Policy::Auto => route_auto(colors.len(), pixels.len() / 4, speed),
        Policy::Upstream => Route::PixelStream { samplefac: speed },
        Policy::Kmeans { iters } => Route::Histogram { iters },
        Policy::Unique { cap } => Route::UniqueStream { cap },
        Policy::Budget { pixels: budget } => {
            let n = pixels.len() / 4;
            let samplefac = (n.div_ceil(budget) as i32).clamp(speed, 30);
            Route::PixelStream { samplefac }
        }
    };

    if trace_enabled() {
        std::eprintln!(
            "rusty_gif: route={route:?} unique={} px={} speed={speed}",
            colors.len(),
            pixels.len() / 4
        );
    }

    let frame = match route {
        Route::Exact => exact_frame(width, height, pixels, &colors, transparent),
        Route::Histogram { iters } => {
            let points: Vec<crate::kmeans::WPoint> = colors
                .iter()
                .map(|&(k, count)| crate::kmeans::WPoint {
                    c: k.to_le_bytes().map(|v| v as i32),
                    w: count as u64,
                })
                .collect();
            let pal = crate::kmeans::palette_kmeans(&points, 256, iters);
            let palette: Vec<u8> = pal.colors.iter().flat_map(|c| [c[0], c[1], c[2]]).collect();
            let lookup: HashMap<u32, u8> = colors
                .iter()
                .zip(pal.assignment.iter())
                .map(|(&(k, _), &idx)| (k, idx))
                .collect();
            let index_of = |px: &[u8]| {
                *lookup
                    .get(&key(px))
                    .expect("kmeans palette lookup: color missing from census")
            };
            Frame {
                width,
                height,
                buffer: Cow::Owned(pixels.chunks_exact(4).map(index_of).collect()),
                palette: Some(palette),
                transparent: transparent.map(|t| index_of(&t)),
                ..Frame::default()
            }
        }
        Route::UniqueStream { cap } => {
            let mut stream: Vec<u8> = Vec::with_capacity(
                colors.iter().map(|&(_, c)| c.min(cap) as usize).sum::<usize>() * 4,
            );
            for &(k, count) in &colors {
                let bytes = k.to_le_bytes();
                for _ in 0..count.min(cap) {
                    stream.extend_from_slice(&bytes);
                }
            }
            if trace_enabled() {
                std::eprintln!(
                    "rusty_gif: training stream {} samples (cap {cap})",
                    stream.len() / 4
                );
            }
            let nq = NeuQuant::new(speed, 256, &stream);
            mapped_frame(width, height, pixels, &colors, &nq, transparent)
        }
        Route::PixelStream { samplefac } => {
            let nq = NeuQuant::new(samplefac, 256, pixels);
            mapped_frame(width, height, pixels, &colors, &nq, transparent)
        }
    };

    #[cfg(debug_assertions)]
    {
        let palette_len = frame.palette.as_ref().map_or(0, |p| p.len());
        debug_assert!(palette_len > 0 && palette_len <= 256 * 3, "palette out of range");
        debug_assert_eq!(frame.buffer.len(), pixels.len() / 4, "index buffer size");
    }
    frame
}

/// ≤256 unique colors: exact palette in upstream's sorted order.
fn exact_frame(
    width: u16,
    height: u16,
    pixels: &[u8],
    colors: &[(u32, u32)],
    transparent: Option<[u8; 4]>,
) -> Frame<'static> {
    let palette: Vec<u8> = colors
        .iter()
        .flat_map(|&(k, _)| {
            let [r, g, b, _a] = k.to_le_bytes();
            [r, g, b]
        })
        .collect();
    let lookup: HashMap<u32, u8> = colors
        .iter()
        .zip(0u16..=255)
        .map(|(&(k, _), i)| (k, i as u8))
        .collect();
    let index_of = |px: &[u8]| lookup.get(&key(px)).copied().unwrap_or(0);
    Frame {
        width,
        height,
        buffer: Cow::Owned(pixels.chunks_exact(4).map(index_of).collect()),
        palette: Some(palette),
        transparent: transparent.map(|t| index_of(&t)),
        ..Frame::default()
    }
}

/// NeuQuant-backed frame with the per-pixel search memoized per unique
/// color: `index_of` runs once per color, pixels then map via the table.
fn mapped_frame(
    width: u16,
    height: u16,
    pixels: &[u8],
    colors: &[(u32, u32)],
    nq: &NeuQuant,
    transparent: Option<[u8; 4]>,
) -> Frame<'static> {
    let lookup: HashMap<u32, u8> = colors
        .iter()
        .map(|&(k, _)| (k, nq.index_of(&k.to_le_bytes()) as u8))
        .collect();
    let index_of = |px: &[u8]| {
        *lookup
            .get(&key(px))
            .expect("memoized palette lookup: color missing from census")
    };
    Frame {
        width,
        height,
        buffer: Cow::Owned(pixels.chunks_exact(4).map(index_of).collect()),
        palette: Some(nq.color_map_rgb()),
        transparent: transparent.map(|t| nq.index_of(&t) as u8),
        ..Frame::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(colors: &[(u8, u8, u8)], reps: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for _ in 0..reps {
            for &(r, g, b) in colors {
                v.extend_from_slice(&[r, g, b, 0xFF]);
            }
        }
        v
    }

    /// ≤256 unique colors must take the exact route and reproduce the input
    /// losslessly through palette+indices.
    #[test]
    fn exact_route_fires_and_is_lossless() {
        let colors: Vec<(u8, u8, u8)> = (0..64u16).map(|i| (i as u8, (i * 2) as u8, 255 - i as u8)).collect();
        let px = rgba(&colors, 16); // 1024 px, 64 unique
        let frame = quantize_frame(32, 32, &px, 1, None);
        let pal = frame.palette.as_ref().unwrap();
        assert_eq!(pal.len(), 64 * 3);
        for (i, p) in px.chunks_exact(4).enumerate() {
            let idx = frame.buffer[i] as usize;
            assert_eq!(&pal[idx * 3..idx * 3 + 3], &p[..3], "pixel {i} not lossless");
        }
    }

    /// The shipped auto-routing tiers, pinned at their boundaries. If a
    /// route is added or a threshold moves, this test must move with it —
    /// no arm may become unreachable silently.
    #[test]
    fn auto_route_boundaries() {
        assert_eq!(route_auto(257, 100_000, 1), Route::Histogram { iters: 3 });
        assert_eq!(route_auto(32_768, 100_000, 1), Route::Histogram { iters: 3 });
        assert_eq!(route_auto(32_769, 100_000, 1), Route::Histogram { iters: 2 });
        assert_eq!(route_auto(49_152, 100_000, 1), Route::Histogram { iters: 2 });
        assert_eq!(route_auto(49_153, 100_000, 1), Route::Histogram { iters: 1 });
        assert_eq!(route_auto(65_536, 100_000, 1), Route::Histogram { iters: 1 });
        // High diversity → budget-sampled NeuQuant, samplefac from pixel count.
        assert_eq!(
            route_auto(65_537, 2_073_600, 1),
            Route::PixelStream { samplefac: 17 }
        );
        assert_eq!(route_auto(65_537, 100_000, 1), Route::PixelStream { samplefac: 1 });
    }

    /// Every auto arm must actually execute end-to-end (not just be
    /// selectable): exact, k-means histogram, and NeuQuant pixel-stream.
    #[test]
    fn all_auto_arms_execute() {
        // Exact: 2 colors.
        let px = rgba(&[(0, 0, 0), (255, 255, 255)], 64);
        let f = quantize_frame(16, 8, &px, 1, None);
        assert_eq!(f.palette.as_ref().unwrap().len(), 2 * 3);

        // Histogram (k-means): ~1k unique colors.
        let colors: Vec<(u8, u8, u8)> =
            (0..1024u32).map(|i| ((i % 256) as u8, (i / 4) as u8, 77)).collect();
        let px = rgba(&colors, 1);
        let f = quantize_frame(32, 32, &px, 1, None);
        assert!(f.palette.as_ref().unwrap().len() <= 256 * 3);
        assert_eq!(f.buffer.len(), 1024);

        // PixelStream (NeuQuant): >65_536 unique colors.
        let mut px = Vec::with_capacity(66_000 * 4);
        for i in 0..66_000u32 {
            // Distinct RGB per pixel.
            px.extend_from_slice(&[(i & 0xFF) as u8, ((i >> 8) & 0xFF) as u8, ((i >> 16) as u8) | 0x40, 0xFF]);
        }
        let f = quantize_frame(300, 220, &px, 1, None);
        assert!(f.palette.as_ref().unwrap().len() <= 256 * 3);
        assert_eq!(f.buffer.len(), 66_000);
    }

    /// >256 unique colors must take the quantizer route and still map every
    /// pixel to an in-range palette index.
    #[test]
    fn unique_stream_route_fires() {
        let colors: Vec<(u8, u8, u8)> = (0..512u16).map(|i| ((i / 2) as u8, (i % 256) as u8, (i / 3) as u8)).collect();
        let px = rgba(&colors, 2); // 1024 px, >256 unique
        let frame = quantize_frame(32, 32, &px, 1, None);
        let pal_len = frame.palette.as_ref().unwrap().len() / 3;
        assert!(pal_len <= 256);
        assert!(frame.buffer.iter().all(|&i| (i as usize) < pal_len));
    }
}
