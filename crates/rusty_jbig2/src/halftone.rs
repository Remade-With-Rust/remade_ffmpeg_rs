//! Pattern dictionaries (6.7) and halftone regions (6.6, Annex C).

use crate::arith::Arith;
use crate::bitmap::{Bitmap, Op};
use crate::budget::Budget;
use crate::generic::{self, Template};
use crate::huffman::Bits;
use crate::mmr;
use crate::text::ceil_log2;
use crate::{Error, Result};

pub(crate) struct PatternDict {
    patterns: Vec<Bitmap>,
    pw: u32,
    ph: u32,
}

pub(crate) fn decode_patterns(data: &[u8], budget: &mut Budget) -> Result<PatternDict> {
    let mut b = Bits::new(data);
    let flags = b.bits(8)?;
    let (pw, ph) = (b.bits(8)?, b.bits(8)?);
    let gray_max = b.bits(32)?;
    if pw == 0 || ph == 0 {
        return Err(Error::Invalid("pattern size 0"));
    }
    let n = u64::from(gray_max) + 1;
    budget.symbols(n)?;
    let w = n * u64::from(pw);
    budget.bitmap(w, u64::from(ph))?;
    let all = if flags & 1 != 0 {
        mmr::decode(b.rest(), w as u32, ph, budget.lenient)?.0
    } else {
        // Table 27: A1 is one pattern back, up to 255 pixels.
        let first = (-(pw as i16), 0);
        let t = Template::nominal_but(((flags >> 1) & 3) as u8, Some(first));
        let mut cx = contexts(&t, budget)?;
        generic::decode(
            &mut Arith::new(b.rest()),
            &mut cx,
            &t,
            false,
            None,
            w as u32,
            ph,
        )
    };
    split(&all, pw, ph, n, budget)
}

fn split(all: &Bitmap, pw: u32, ph: u32, n: u64, budget: &mut Budget) -> Result<PatternDict> {
    let mut patterns = Vec::new();
    for i in 0..n as i64 {
        budget.bitmap(u64::from(pw), u64::from(ph))?;
        patterns.push(all.extract(i * i64::from(pw), 0, pw, ph));
    }
    Ok(PatternDict { patterns, pw, ph })
}

fn contexts(t: &Template, budget: &mut Budget) -> Result<Vec<u8>> {
    budget.charge(t.contexts() as u64 / 8)?;
    Ok(vec![0; t.contexts()])
}

/// The halftone region's data after the region segment information field.
pub(crate) fn decode_region(
    data: &[u8],
    w: u32,
    h: u32,
    dict: &PatternDict,
    budget: &mut Budget,
) -> Result<Bitmap> {
    let mut b = Bits::new(data);
    let flags = b.bits(8)?;
    let is_mmr = flags & 1 != 0;
    let template = ((flags >> 1) & 3) as u8;
    let enable_skip = flags & 8 != 0 && !is_mmr;
    let op = Op::from_bits((flags >> 4) & 7)?;
    let gw = b.bits(32)?;
    let gh = b.bits(32)?;
    let gx = i64::from(b.bits(32)? as i32);
    let gy = i64::from(b.bits(32)? as i32);
    let rx = i64::from(b.bits(16)?);
    let ry = i64::from(b.bits(16)?);

    budget.bitmap(u64::from(w), u64::from(h))?;
    let mut reg = Bitmap::new(w, h, flags & 0x80 != 0);
    let (pw, ph) = (i64::from(dict.pw), i64::from(dict.ph));
    // Grid cells are charged whether or not they are coded.
    budget.bitmap(u64::from(gw), u64::from(gh))?;
    let place = |m: u32, n: u32| -> (i64, i64) {
        let (m, n) = (i64::from(m), i64::from(n));
        ((gx + m * ry + n * rx) >> 8, (gy + m * rx - n * ry) >> 8)
    };
    let outside =
        |x: i64, y: i64| x + pw <= 0 || x >= i64::from(w) || y + ph <= 0 || y >= i64::from(h);

    let skip = if enable_skip {
        budget.bitmap(u64::from(gw), u64::from(gh))?;
        let mut s = Bitmap::new(gw, gh, false);
        for m in 0..gh {
            for n in 0..gw {
                let (x, y) = place(m, n);
                if outside(x, y) {
                    s.data[m as usize * s.stride + (n >> 3) as usize] |= 0x80 >> (n & 7);
                }
            }
        }
        Some(s)
    } else {
        None
    };

    // Annex C: Gray-coded bit planes, most significant first.
    let bpp = ceil_log2(dict.patterns.len());
    let mut planes: Vec<Bitmap> = Vec::new();
    let t = Template::nominal_but(template, None);
    let mut cx = if is_mmr {
        Vec::new()
    } else {
        contexts(&t, budget)?
    };
    let mut rest = b.rest();
    let mut a = Arith::new(rest);
    for _ in 0..bpp {
        budget.bitmap(u64::from(gw), u64::from(gh))?;
        let mut p = if is_mmr {
            let (p, used) = mmr::decode(rest, gw, gh, budget.lenient)?;
            rest = rest.get(used..).unwrap_or(&[]);
            p
        } else {
            generic::decode(&mut a, &mut cx, &t, false, skip.as_ref(), gw, gh)
        };
        if let Some(prev) = planes.last() {
            for (d, s) in p.data.iter_mut().zip(&prev.data) {
                *d ^= s;
            }
        }
        planes.push(p);
    }

    let cost = (u64::from(dict.pw) / 8 + 2) * u64::from(dict.ph);
    for m in 0..gh {
        for n in 0..gw {
            let mut v = 0usize;
            for p in &planes {
                v = (v << 1) | p.get(n, m) as usize;
            }
            if v >= dict.patterns.len() {
                if !budget.lenient {
                    return Err(Error::Invalid("gray value has no pattern"));
                }
                v = dict.patterns.len() - 1;
            }
            let (x, y) = place(m, n);
            if outside(x, y) {
                continue;
            }
            budget.charge(cost)?;
            reg.draw(&dict.patterns[v], x, y, op);
        }
    }
    Ok(reg)
}
