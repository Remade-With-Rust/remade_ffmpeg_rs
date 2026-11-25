//! Symbol dictionary decoding (6.5, 7.4.2).

use std::sync::Arc;

use crate::arith::{Arith, IntCtx};
use crate::bitmap::{Bitmap, Op};
use crate::budget::Budget;
use crate::generic::{self, Template};
use crate::huffman::{Bits, Table};
use crate::mmr;
use crate::refinement::{self, RefTemplate, Reference};
use crate::text::{self, refinement_data, ArithCtx, Coder, HuffTables, SymCodes};
use crate::{Error, Result};

/// A decoded dictionary: its exported symbols, and the bitmap coding contexts
/// when the segment retains them.
pub(crate) struct SymbolDict {
    pub(crate) symbols: Vec<Arc<Bitmap>>,
    retained: Option<Contexts>,
}

/// Generic and refinement region statistics (7.4.2.2 steps 3 and 7).
#[derive(Clone)]
struct Contexts {
    template: Option<Template>,
    gb: Vec<u8>,
    ref_template: Option<RefTemplate>,
    gr: Vec<u8>,
}

struct Header {
    refagg: bool,
    template: Option<Template>,
    ref_template: Option<RefTemplate>,
    exported: u32,
    new: u32,
}

struct DictTables {
    dh: Arc<Table>,
    dw: Arc<Table>,
    bmsize: Arc<Table>,
    agginst: Arc<Table>,
    text: HuffTables,
}

/// Hands out standard tables and, in order, the referred-to custom ones.
pub(crate) struct TableSource<'t> {
    custom: &'t [Arc<Table>],
    next: usize,
}

impl<'t> TableSource<'t> {
    pub(crate) fn new(custom: &'t [Arc<Table>]) -> Self {
        Self { custom, next: 0 }
    }

    /// `selection` indexes `standard`; the value 3 (1 for a one-bit field)
    /// selects the next referred-to table.
    pub(crate) fn pick(&mut self, selection: u32, standard: &[usize]) -> Result<Arc<Table>> {
        match standard.get(selection as usize) {
            Some(&n) => Ok(standard_table(n)),
            None if selection == 3 || standard.len() == 1 => {
                let t = self.custom.get(self.next).cloned();
                self.next += 1;
                t.ok_or(Error::Invalid("custom Huffman table not referred to"))
            }
            None => Err(Error::Invalid("reserved Huffman table selection")),
        }
    }
}

pub(crate) fn standard_table(n: usize) -> Arc<Table> {
    use std::sync::OnceLock;
    static T: [OnceLock<Arc<Table>>; 15] = [const { OnceLock::new() }; 15];
    T[n - 1]
        .get_or_init(|| Arc::new(Table::standard(n)))
        .clone()
}

/// `previous` is the last referred-to dictionary, whose contexts may be used.
pub(crate) fn decode(
    data: &[u8],
    input: &[Arc<Bitmap>],
    custom: &[Arc<Table>],
    previous: Option<&SymbolDict>,
    budget: &mut Budget,
) -> Result<SymbolDict> {
    let mut b = Bits::new(data);
    let flags = b.bits(16)?;
    let huff = flags & 1 != 0;
    let refagg = flags & 2 != 0;
    let ctx_used = flags & 0x100 != 0;
    let ctx_retained = flags & 0x200 != 0;
    let mut src = TableSource::new(custom);
    let tables = if huff {
        let t = std_text_tables();
        Some(DictTables {
            dh: src.pick((flags >> 2) & 3, &[4, 5])?,
            dw: src.pick((flags >> 4) & 3, &[2, 3])?,
            bmsize: src.pick((flags >> 6) & 1, &[1])?,
            agginst: src.pick((flags >> 7) & 1, &[1])?,
            text: t,
        })
    } else {
        None
    };
    let template = if huff {
        None
    } else {
        Some(Template::parse(((flags >> 10) & 3) as u8, false, &mut b)?)
    };
    let ref_template = if refagg {
        Some(RefTemplate::parse(flags & 0x1000 != 0, &mut b)?)
    } else {
        None
    };
    let hd = Header {
        refagg,
        template,
        ref_template,
        exported: b.bits(32)?,
        new: b.bits(32)?,
    };
    budget.symbols(input.len() as u64 + u64::from(hd.new))?;
    if u64::from(hd.exported) > input.len() as u64 + u64::from(hd.new) {
        return Err(Error::Invalid("more symbols exported than exist"));
    }

    // 7.4.2.2: start from zero statistics or the previous dictionary's.
    let mut cx = Contexts {
        gb: vec![0; hd.template.as_ref().map_or(0, Template::table)],
        template: hd.template.clone(),
        gr: vec![0; hd.ref_template.map_or(0, RefTemplate::contexts)],
        ref_template: hd.ref_template,
    };
    budget.charge((cx.gb.len() + cx.gr.len()) as u64 / 8)?;
    if ctx_used {
        let old = previous
            .and_then(|p| p.retained.as_ref())
            .ok_or(Error::Invalid("no retained bitmap coding contexts"))?;
        if hd.template.is_some() && old.template.is_some() {
            if hd.template != old.template {
                return Err(Error::Invalid("retained contexts of another template"));
            }
            cx.gb.clone_from(&old.gb);
        }
        if hd.ref_template.is_some() && old.ref_template.is_some() {
            if hd.ref_template != old.ref_template {
                return Err(Error::Invalid("retained contexts of another template"));
            }
            cx.gr.clone_from(&old.gr);
        }
    }
    if ctx_retained {
        budget.charge((cx.gb.len() + cx.gr.len()) as u64 * 8)?;
    }

    let mut all: Vec<Arc<Bitmap>> = input.to_vec();
    let export = match tables {
        Some(t) => decode_huff(&hd, &t, &mut b, &mut all, &mut cx, budget)?,
        None => decode_arith(&hd, b.rest(), &mut all, &mut cx, budget)?,
    };
    let symbols: Vec<_> = all
        .iter()
        .zip(&export)
        .filter(|(_, &e)| e)
        .map(|(s, _)| s.clone())
        .collect();
    if symbols.len() != hd.exported as usize {
        return Err(Error::Invalid("exported symbol count mismatch"));
    }
    Ok(SymbolDict {
        symbols,
        retained: ctx_retained.then_some(cx),
    })
}

/// Table 17's fixed tables.
fn std_text_tables() -> HuffTables {
    HuffTables {
        fs: standard_table(6),
        ds: standard_table(8),
        dt: standard_table(11),
        rdw: standard_table(15),
        rdh: standard_table(15),
        rdx: standard_table(15),
        rdy: standard_table(15),
        rsize: standard_table(1),
    }
}

fn size(v: i64, delta: i64) -> Result<u32> {
    u32::try_from(v + delta).map_err(|_| Error::Invalid("symbol size out of range"))
}

/// Table 17: the aggregate's text region.
fn aggregate_params(hd: &Header, rt: RefTemplate, w: u32, h: u32, instances: u32) -> text::Params {
    debug_assert!(hd.refagg);
    text::Params {
        refine: true,
        w,
        h,
        instances,
        log_strips: 0,
        def_pixel: false,
        op: Op::Or,
        transposed: false,
        corner: 1,
        ds_offset: 0,
        template: rt,
        read_final_oob: true,
    }
}

fn instances(v: i64) -> Result<u32> {
    match u32::try_from(v) {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(Error::Invalid("aggregate instance count")),
    }
}

fn decode_arith(
    hd: &Header,
    data: &[u8],
    all: &mut Vec<Arc<Bitmap>>,
    cx: &mut Contexts,
    budget: &mut Budget,
) -> Result<Vec<bool>> {
    let mut a = Arith::new(data);
    let (mut iadh, mut iadw, mut iaex, mut iaai) =
        (IntCtx::new(), IntCtx::new(), IntCtx::new(), IntCtx::new());
    let total = all.len() + hd.new as usize;
    // Only aggregate coding uses the symbol ID contexts.
    let mut tcx = ArithCtx::new(if hd.refagg { total } else { 0 }, budget)?;
    let template = hd
        .template
        .as_ref()
        .ok_or(Error::Invalid("missing template"))?;
    let mut height = 0u32;
    while all.len() < total {
        budget.charge(16)?;
        height = size(i64::from(height), a.int_some(&mut iadh)?)?;
        let mut width = 0u32;
        while let Some(dw) = a.int(&mut iadw) {
            if all.len() >= total {
                return Err(Error::Invalid("more symbols than declared"));
            }
            width = size(i64::from(width), dw)?;
            budget.bitmap(u64::from(width), u64::from(height))?;
            let bm = if let Some(rt) = hd.ref_template {
                let n = instances(a.int_some(&mut iaai)?)?;
                if n == 1 {
                    let id = a.iaid(&mut tcx.id, tcx.id_len);
                    let dx = a.int_some(&mut tcx.rdx)?;
                    let dy = a.int_some(&mut tcx.rdy)?;
                    let bitmap = all
                        .get(id)
                        .ok_or(Error::Invalid("symbol ID out of range"))?;
                    let r = Reference { bitmap, dx, dy };
                    refinement::decode(&mut a, &mut cx.gr, rt, false, &r, width, height)
                } else {
                    let p = aggregate_params(hd, rt, width, height, n);
                    let mut c = Coder::Arith(&mut a, &mut tcx);
                    text::decode(&p, &mut c, all, &mut cx.gr, budget)?
                }
            } else {
                generic::decode(&mut a, &mut cx.gb, template, false, None, width, height)
            };
            all.push(Arc::new(bm));
        }
    }
    export_flags(total, budget, || a.int_some(&mut iaex))
}

fn decode_huff(
    hd: &Header,
    t: &DictTables,
    b: &mut Bits<'_>,
    all: &mut Vec<Arc<Bitmap>>,
    cx: &mut Contexts,
    budget: &mut Budget,
) -> Result<Vec<bool>> {
    b.align();
    let total = all.len() + hd.new as usize;
    let id_len = text::ceil_log2(total).max(1);
    let codes = SymCodes::Fixed(id_len);
    let mut height = 0u32;
    let mut widths = Vec::new();
    while all.len() < total {
        budget.charge(16)?;
        height = size(i64::from(height), t.dh.read_some(b)?)?;
        let mut width = 0u32;
        let mut total_width = 0u64;
        widths.clear();
        while let Some(dw) = t.dw.read(b)? {
            if all.len() + widths.len() >= total {
                return Err(Error::Invalid("more symbols than declared"));
            }
            width = size(i64::from(width), dw)?;
            let Some(rt) = hd.ref_template else {
                total_width += u64::from(width);
                widths.push(width);
                continue;
            };
            budget.bitmap(u64::from(width), u64::from(height))?;
            let n = instances(t.agginst.read_some(b)?)?;
            let bm = if n == 1 {
                let id = b.bits(id_len)? as usize;
                let dx = t.text.rdx.read_some(b)?;
                let dy = t.text.rdy.read_some(b)?;
                let bitmap = all
                    .get(id)
                    .ok_or(Error::Invalid("symbol ID out of range"))?;
                let r = Reference { bitmap, dx, dy };
                let mut a = Arith::new(refinement_data(b, &t.text.rsize)?);
                refinement::decode(&mut a, &mut cx.gr, rt, false, &r, width, height)
            } else {
                let p = aggregate_params(hd, rt, width, height, n);
                let mut c = Coder::Huff(b, &t.text, &codes);
                text::decode(&p, &mut c, all, &mut cx.gr, budget)?
            };
            all.push(Arc::new(bm));
        }
        if hd.refagg {
            continue;
        }
        // 6.5.9: one collective bitmap, the symbols side by side.
        let n = t.bmsize.read_some(b)?;
        budget.bitmap(total_width, u64::from(height))?;
        let w = total_width as u32;
        let collective = if n == 0 {
            let bytes = w.div_ceil(8) as usize * height as usize;
            Bitmap::from_packed(w, height, b.take(bytes)?.to_vec())
        } else {
            let n = usize::try_from(n).map_err(|_| Error::Invalid("collective bitmap size"))?;
            mmr::decode(b.take(n)?, w, height, budget.lenient)?.0
        };
        let mut x = 0i64;
        for &w in &widths {
            budget.bitmap(u64::from(w), u64::from(height))?;
            all.push(Arc::new(collective.extract(x, 0, w, height)));
            x += i64::from(w);
        }
    }
    let runs = standard_table(1);
    export_flags(total, budget, || runs.read_some(b))
}

/// 6.5.10.
fn export_flags(
    total: usize,
    budget: &mut Budget,
    mut run: impl FnMut() -> Result<i64>,
) -> Result<Vec<bool>> {
    let mut flags = Vec::with_capacity(total);
    let mut on = false;
    let mut empty = 0;
    while flags.len() < total {
        budget.charge(4)?;
        let n = usize::try_from(run()?).map_err(|_| Error::Invalid("export run length"))?;
        if n > total - flags.len() {
            return Err(Error::Invalid("export run length"));
        }
        // Two empty runs in a row say nothing; a stream of them is damage.
        empty = if n == 0 { empty + 1 } else { 0 };
        if empty > 4 {
            return Err(Error::Invalid("empty export runs"));
        }
        flags.resize(flags.len() + n, on);
        on = !on;
    }
    Ok(flags)
}
