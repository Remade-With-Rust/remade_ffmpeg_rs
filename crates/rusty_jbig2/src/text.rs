//! Text region decoding (6.4).

use std::sync::Arc;

use crate::arith::{Arith, IntCtx};
use crate::bitmap::{Bitmap, Op};
use crate::budget::Budget;
use crate::huffman::{Bits, Prefix, Table};
use crate::refinement::{self, RefTemplate, Reference};
use crate::{Error, Result};

/// The parameters both coding variants share.
pub(crate) struct Params {
    pub(crate) refine: bool,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) instances: u32,
    pub(crate) log_strips: u32,
    pub(crate) def_pixel: bool,
    pub(crate) op: Op,
    pub(crate) transposed: bool,
    /// 0 bottom left, 1 top left, 2 bottom right, 3 top right.
    pub(crate) corner: u32,
    pub(crate) ds_offset: i64,
    pub(crate) template: RefTemplate,
    /// Whether the last strip's terminating OOB has to be consumed because
    /// more data follows in the same coder (aggregate symbols).
    pub(crate) read_final_oob: bool,
}

/// Integer contexts of the arithmetic variant. In a symbol dictionary they
/// live as long as the dictionary.
pub(crate) struct ArithCtx {
    dt: IntCtx,
    fs: IntCtx,
    ds: IntCtx,
    it: IntCtx,
    ri: IntCtx,
    rdw: IntCtx,
    rdh: IntCtx,
    pub(crate) rdx: IntCtx,
    pub(crate) rdy: IntCtx,
    pub(crate) id: Vec<u8>,
    pub(crate) id_len: u32,
}

impl ArithCtx {
    /// `symbols` is the number of symbol IDs that can be coded.
    pub(crate) fn new(symbols: usize, budget: &mut Budget) -> Result<Self> {
        let id_len = ceil_log2(symbols);
        budget.charge(1 << id_len)?;
        let c = IntCtx::new();
        Ok(Self {
            dt: c.clone(),
            fs: c.clone(),
            ds: c.clone(),
            it: c.clone(),
            ri: c.clone(),
            rdw: c.clone(),
            rdh: c.clone(),
            rdx: c.clone(),
            rdy: c,
            id: vec![0; 1 << id_len],
            id_len,
        })
    }
}

/// Smallest `n` with `2^n >= v`; 0 for 0.
pub(crate) fn ceil_log2(v: usize) -> u32 {
    v.max(1).next_power_of_two().trailing_zeros()
}

/// Tables of the Huffman variant.
pub(crate) struct HuffTables {
    pub(crate) fs: Arc<Table>,
    pub(crate) ds: Arc<Table>,
    pub(crate) dt: Arc<Table>,
    pub(crate) rdw: Arc<Table>,
    pub(crate) rdh: Arc<Table>,
    pub(crate) rdx: Arc<Table>,
    pub(crate) rdy: Arc<Table>,
    pub(crate) rsize: Arc<Table>,
}

/// How symbol IDs are coded in the Huffman variant.
pub(crate) enum SymCodes {
    Prefix(Prefix),
    /// Fixed-length codes (aggregate symbols, 6.5.8.2.3).
    Fixed(u32),
}

/// The entropy coder a text region is read from.
pub(crate) enum Coder<'c, 'd> {
    Arith(&'c mut Arith<'d>, &'c mut ArithCtx),
    Huff(&'c mut Bits<'d>, &'c HuffTables, &'c SymCodes),
}

/// Positions are kept to 32 bits, as the specification's variables are.
fn pos(v: i64) -> Result<i64> {
    i32::try_from(v)
        .map(i64::from)
        .map_err(|_| Error::Invalid("symbol position out of range"))
}

/// A refined size: the reference's plus a delta, at least 0.
pub(crate) fn refined(size: u32, delta: i64) -> Result<u32> {
    u32::try_from(i64::from(size) + delta).map_err(|_| Error::Invalid("refined symbol size"))
}

pub(crate) fn decode(
    p: &Params,
    coder: &mut Coder<'_, '_>,
    symbols: &[Arc<Bitmap>],
    gr: &mut [u8],
    budget: &mut Budget,
) -> Result<Bitmap> {
    budget.bitmap(u64::from(p.w), u64::from(p.h))?;
    let mut reg = Bitmap::new(p.w, p.h, p.def_pixel);
    let strips = 1i64 << p.log_strips;

    let delta_t = |c: &mut Coder<'_, '_>| -> Result<i64> {
        let v = match c {
            Coder::Arith(a, cx) => a.int_some(&mut cx.dt)?,
            Coder::Huff(b, t, _) => t.dt.read_some(b)?,
        };
        Ok(v * strips)
    };
    let mut strip_t = pos(-delta_t(coder)?)?;
    let mut first_s = 0i64;
    let mut done = 0u32;
    while done < p.instances {
        // An arithmetic-coded strip can hold as little as one instance.
        budget.charge(16)?;
        strip_t = pos(strip_t + delta_t(coder)?)?;
        let dfs = match coder {
            Coder::Arith(a, cx) => a.int_some(&mut cx.fs)?,
            Coder::Huff(b, t, _) => t.fs.read_some(b)?,
        };
        first_s = pos(first_s + dfs)?;
        let mut cur_s = first_s;
        let mut first = true;
        loop {
            if !first {
                if done == p.instances && !p.read_final_oob {
                    break;
                }
                let ds = match coder {
                    Coder::Arith(a, cx) => a.int(&mut cx.ds),
                    Coder::Huff(b, t, _) => t.ds.read(b)?,
                };
                let Some(ds) = ds else { break };
                if done == p.instances {
                    return Err(Error::Invalid("more symbol instances than declared"));
                }
                cur_s = pos(cur_s + ds + p.ds_offset)?;
            }
            first = false;
            budget.charge(16)?;
            let cur_t = if p.log_strips == 0 {
                0
            } else {
                match coder {
                    Coder::Arith(a, cx) => a.int_some(&mut cx.it)?,
                    Coder::Huff(b, _, _) => i64::from(b.bits(p.log_strips)?),
                }
            };
            let t = pos(strip_t + cur_t)?;
            let id = match coder {
                Coder::Arith(a, cx) => {
                    let len = cx.id_len;
                    a.iaid(&mut cx.id, len)
                }
                Coder::Huff(b, _, SymCodes::Prefix(t)) => t.read(b)?,
                Coder::Huff(b, _, SymCodes::Fixed(n)) => b.bits(*n)? as usize,
            };
            let Some(symbol) = symbols.get(id) else {
                // Without refinement nothing else is coded for it.
                if budget.lenient && !p.refine {
                    done += 1;
                    continue;
                }
                return Err(Error::Invalid("symbol ID out of range"));
            };
            let ri = p.refine
                && match coder {
                    Coder::Arith(a, cx) => a.int_some(&mut cx.ri)? != 0,
                    Coder::Huff(b, _, _) => b.bit()? != 0,
                };
            let refined;
            let bitmap = if ri {
                refined = refine_instance(p, coder, symbol, gr, budget)?;
                &refined
            } else {
                &**symbol
            };
            let (w, h) = (i64::from(bitmap.w), i64::from(bitmap.h));
            // The advance, before or after drawing depending on the corner.
            let advance = if p.transposed { h - 1 } else { w - 1 };
            let before = if p.transposed {
                p.corner == 0 || p.corner == 2
            } else {
                p.corner >= 2
            };
            if before {
                cur_s = pos(cur_s + advance)?;
            }
            let (mut x, mut y) = if p.transposed { (t, cur_s) } else { (cur_s, t) };
            if p.corner >= 2 {
                x -= w - 1;
            }
            if p.corner == 0 || p.corner == 2 {
                y -= h - 1;
            }
            budget.charge(bitmap.data.len() as u64)?;
            reg.draw(bitmap, x, y, p.op);
            if !before {
                cur_s = pos(cur_s + advance)?;
            }
            done += 1;
        }
    }
    Ok(reg)
}

/// A symbol instance bitmap with refinement (6.4.11, RI = 1).
fn refine_instance(
    p: &Params,
    coder: &mut Coder<'_, '_>,
    symbol: &Bitmap,
    gr: &mut [u8],
    budget: &mut Budget,
) -> Result<Bitmap> {
    let (dw, dh, dx, dy) = match coder {
        Coder::Arith(a, cx) => (
            a.int_some(&mut cx.rdw)?,
            a.int_some(&mut cx.rdh)?,
            a.int_some(&mut cx.rdx)?,
            a.int_some(&mut cx.rdy)?,
        ),
        Coder::Huff(b, t, _) => (
            t.rdw.read_some(b)?,
            t.rdh.read_some(b)?,
            t.rdx.read_some(b)?,
            t.rdy.read_some(b)?,
        ),
    };
    let w = refined(symbol.w, dw)?;
    let h = refined(symbol.h, dh)?;
    budget.bitmap(u64::from(w), u64::from(h))?;
    let r = Reference {
        bitmap: symbol,
        dx: (dw >> 1) + dx,
        dy: (dh >> 1) + dy,
    };
    match coder {
        Coder::Arith(a, _) => Ok(refinement::decode(a, gr, p.template, false, &r, w, h)),
        Coder::Huff(b, t, _) => {
            let data = refinement_data(b, &t.rsize)?;
            let mut a = Arith::new(data);
            Ok(refinement::decode(&mut a, gr, p.template, false, &r, w, h))
        }
    }
}

/// The byte-aligned arithmetic data embedded in a Huffman-coded region.
pub(crate) fn refinement_data<'d>(b: &mut Bits<'d>, rsize: &Table) -> Result<&'d [u8]> {
    let n = rsize.read_some(b)?;
    b.take(usize::try_from(n).map_err(|_| Error::Invalid("refinement data size"))?)
}
