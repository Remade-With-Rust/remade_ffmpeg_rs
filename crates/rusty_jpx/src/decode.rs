//! Codestream to component samples.

use std::borrow::Cow;

use crate::codestream::{self, Codestream, Siz, TileParts};
use crate::dwt::Plane;
use crate::error::{try_vec, JpxError, JpxResult};
use crate::kernels::{self, Level};
use crate::tile::{Budget, PacketData, Scratch, Tile};
use crate::DecodeOptions;

/// Decoded components, before any JP2 level processing.
pub(crate) struct Decoded {
    pub siz: Siz,
    pub samples: Vec<Vec<i32>>,
    pub truncated: bool,
}

pub(crate) fn decode(cs: &[u8], options: &DecodeOptions) -> JpxResult<Decoded> {
    let stream = codestream::parse(cs, &options.limits)?;
    let reduce = u32::from(options.reduce);
    if reduce > 32 {
        return Err(JpxError::Usage("reduce exceeds the decomposition levels"));
    }
    let siz = &stream.siz;
    if siz.comps.iter().any(|c| c.bits > 31) {
        return Err(JpxError::Unsupported("more than 31 bits per sample"));
    }
    let sizes: Vec<(u64, u64)> = (0..siz.comps.len())
        .map(|c| {
            let r = siz.comp_rect(c, reduce);
            (
                u64::from(r[2].saturating_sub(r[0])),
                u64::from(r[3].saturating_sub(r[1])),
            )
        })
        .collect();
    // Products of two u32s; the sum is checked as it goes.
    let mut total = 0u64;
    for &(w, h) in &sizes {
        total = total.saturating_add(w.saturating_mul(h));
    }
    if total > options.limits.max_samples {
        return Err(JpxError::Limit("samples"));
    }
    // A tile without samples in any component need not be present.
    let missing = (0u32..)
        .zip(&stream.tiles)
        .any(|(t, parts)| parts.is_none() && !siz.tile_is_empty(t));
    if missing && !options.allow_truncated {
        return Err(JpxError::Truncated);
    }

    // Allocated on first use, so that a single tile can hand its buffer over.
    let mut out: Vec<Vec<i32>> = vec![Vec::new(); siz.comps.len()];
    let mut truncated = missing;
    let mut budget = Budget {
        structures: options.limits.max_structures,
        steps: (1 << 26) + 2 * cs.len() as u64,
    };
    let mut scratch = Scratch::default();
    for (t, parts) in stream.tiles.iter().enumerate() {
        let Some(parts) = parts else { continue };
        let complete = decode_tile(
            cs,
            &stream,
            t as u32,
            parts,
            options,
            &sizes,
            &mut budget,
            &mut scratch,
            &mut out,
        )?;
        truncated |= !complete;
    }
    for (o, &(w, h)) in out.iter_mut().zip(&sizes) {
        if o.is_empty() {
            *o = try_vec((w * h) as usize)?;
        }
    }
    Ok(Decoded {
        siz: stream.siz,
        samples: out,
        truncated,
    })
}

/// Decode one tile into `out`; `Ok(false)` when its data was incomplete.
#[allow(clippy::too_many_arguments)]
fn decode_tile(
    cs: &[u8],
    stream: &Codestream,
    t: u32,
    parts: &TileParts,
    options: &DecodeOptions,
    sizes: &[(u64, u64)],
    budget: &mut Budget,
    scratch: &mut Scratch,
    out: &mut [Vec<i32>],
) -> JpxResult<bool> {
    let siz = &stream.siz;
    let reduce = u32::from(options.reduce);
    let mut params = Cow::Borrowed(&stream.params);
    for (marker, body) in &parts.markers {
        params.to_mut().apply(*marker, &cs[body.clone()], true)?;
    }
    let mut tile = Tile::new(siz, &params, siz.tile(t), reduce, budget)?;

    let body: Cow<[u8]> = match parts.bodies.as_slice() {
        [] => Cow::Borrowed(&[]),
        [one] => Cow::Borrowed(&cs[one.clone()]),
        many => Cow::Owned(many.iter().flat_map(|r| &cs[r.clone()]).copied().collect()),
    };
    let packed = parts.packed_headers(cs);
    let mut data = PacketData {
        body: &body,
        pos: 0,
        packed: packed.as_deref().map(|h| (h, 0)),
        lenient: options.allow_truncated,
        clean_end: false,
        cut: false,
    };
    let complete = match tile.read_packets(&params, &mut data, budget) {
        Ok(()) => !data.cut,
        // Fewer packets than the progression asks for, in a codestream that
        // is whole: its writer dropped the rest (layers, usually).
        Err(JpxError::Truncated) if data.clean_end && stream.ended && !parts.truncated => true,
        Err(JpxError::Truncated | JpxError::Format(_)) if options.allow_truncated => false,
        Err(e) => return Err(e),
    };

    let use_mct = params.mct && siz.comps.len() >= 3;
    let mut rest = 0..siz.comps.len();
    if use_mct {
        let rects = [0, 1, 2].map(|c| tile.comps[c].rect());
        if rects[0] != rects[1] || rects[0] != rects[2] {
            return Err(JpxError::Format(
                "colour transform over components of different sizes",
            ));
        }
        let mut planes = Vec::with_capacity(3);
        for comp in &tile.comps[..3] {
            planes.push(comp.reconstruct(&body, scratch)?);
        }
        inverse_mct(&mut planes)?;
        for (c, plane) in planes.into_iter().enumerate() {
            store(siz, c, reduce, rects[c], plane, sizes[c], &mut out[c])?;
        }
        rest.start = 3;
    }
    for c in rest {
        let comp = &tile.comps[c];
        let plane = comp.reconstruct(&body, scratch)?;
        store(siz, c, reduce, comp.rect(), plane, sizes[c], &mut out[c])?;
    }
    Ok(complete)
}

/// Inverse RCT or ICT (G.2, G.3) on the first three components.
fn inverse_mct(planes: &mut [Plane]) -> JpxResult<()> {
    let [a, b, c] = planes else { return Ok(()) };
    if a.reals != b.reals || a.reals != c.reals {
        return Err(JpxError::Format("colour transform over different wavelets"));
    }
    kernels::inverse_mct([&mut a.data, &mut b.data, &mut c.data], a.reals);
    Ok(())
}

/// DC level shift, clamp and copy of a tile-component into its component.
fn store(
    siz: &Siz,
    c: usize,
    reduce: u32,
    rect: [u32; 4],
    plane: Plane,
    (w, h): (u64, u64),
    out: &mut Vec<i32>,
) -> JpxResult<()> {
    let k = siz.comps[c];
    let level = Level::new(k.bits, k.signed);
    let origin = siz.comp_rect(c, reduce);
    let (w, h) = (w as usize, h as usize);
    let tw = rect[2].saturating_sub(rect[0]) as usize;
    let th = rect[3].saturating_sub(rect[1]) as usize;
    if tw == 0 || th == 0 {
        return Ok(());
    }
    if !plane.reals && out.is_empty() && (tw, th) == (w, h) {
        *out = plane.data;
        kernels::level_in_place(out, level, false);
        return Ok(());
    }
    if out.is_empty() {
        *out = try_vec(w * h)?;
    }
    let x = (rect[0] - origin[0]) as usize;
    let y = (rect[1] - origin[1]) as usize;
    let rows = out.chunks_exact_mut(w).skip(y).take(th);
    for (dst, src) in rows.zip(plane.data.chunks_exact(tw)) {
        kernels::level_into(&mut dst[x..x + tw], src, level, plane.reals);
    }
    Ok(())
}
