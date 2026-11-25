//! Segment dispatch and page composition (7.4, 8.2).

use std::collections::HashMap;
use std::sync::Arc;

use crate::arith::Arith;
use crate::bitmap::{Bitmap, Bitmap1, Op};
use crate::budget::Budget;
use crate::generic::{self, Template};
use crate::halftone::{self, PatternDict};
use crate::huffman::{Bits, Prefix, Table};
use crate::refinement::{self, RefTemplate, Reference};
use crate::segment::{kind, RegionInfo, Segment};
use crate::symbol::{self, SymbolDict, TableSource};
use crate::text::{self, ArithCtx, Coder, HuffTables, SymCodes};
use crate::{mmr, DecodeOptions, Error, Result};

/// What a decoded segment leaves for later ones to refer to.
enum Resource {
    Symbols(SymbolDict),
    Patterns(PatternDict),
    Table(Arc<Table>),
    /// An intermediate region's auxiliary buffer.
    Region(Bitmap),
}

struct Page {
    bitmap: Bitmap,
    default_pixel: bool,
    /// The height was not given: it follows from the stripes.
    unknown_height: bool,
    max_stripe: u32,
    last_stripe_end: Option<u32>,
}

struct Decoder<'o> {
    opts: &'o DecodeOptions,
    budget: Budget,
    resources: HashMap<u32, Resource>,
    page: Option<Page>,
}

/// Decode the page numbered `page` (`None`: the first one there is) from
/// `segments`. `parse_error` is what cut the segment list short.
pub(crate) fn decode<'a, 's: 'a>(
    segments: impl Iterator<Item = &'a Segment<'s>> + Clone,
    page: Option<u32>,
    parse_error: Option<Error>,
    needs_end_of_page: bool,
    opts: &DecodeOptions,
) -> Result<Bitmap1> {
    let lenient = opts.lenient;
    if let (Some(e), false) = (parse_error, lenient) {
        return Err(e);
    }
    let no_page = parse_error.unwrap_or(Error::Invalid("no page information segment"));
    let number = match page {
        Some(p) => p,
        None => {
            segments
                .clone()
                .find(|s| s.kind == kind::PAGE_INFO)
                .ok_or(no_page)?
                .page
        }
    };
    let mut d = Decoder {
        opts,
        budget: Budget::new(opts),
        resources: HashMap::new(),
        page: None,
    };
    let mut ended = false;
    for s in segments {
        if s.page != 0 && s.page != number {
            continue;
        }
        if s.kind == kind::END_OF_PAGE && s.page == number && d.page.is_some() {
            ended = true;
            break;
        }
        match d.segment(s) {
            Err(Error::LimitExceeded(l)) => return Err(Error::LimitExceeded(l)),
            Err(e) if !lenient => return Err(e),
            _ => {}
        }
    }
    let p = d.page.ok_or(no_page)?;
    if needs_end_of_page && !ended && !lenient {
        return Err(Error::Truncated);
    }
    let mut bm = p.bitmap;
    if p.unknown_height {
        match p.last_stripe_end {
            Some(y) => bm.set_height(y + 1, p.default_pixel),
            None if lenient => {}
            None => return Err(Error::Invalid("page height never given")),
        }
    }
    Ok(Bitmap1 {
        width: bm.w,
        height: bm.h,
        stride: bm.stride,
        data: bm.data,
    })
}

impl Decoder<'_> {
    fn segment(&mut self, s: &Segment<'_>) -> Result<()> {
        // Each segment costs something, whatever it declares.
        self.budget.charge(256 + s.refs.len() as u64 * 16)?;
        match s.kind {
            kind::SYMBOL_DICT => {
                let input = self.symbols(&s.refs)?;
                let tables = self.tables(&s.refs);
                let previous = s
                    .refs
                    .iter()
                    .rev()
                    .find_map(|r| match self.resources.get(r) {
                        Some(Resource::Symbols(d)) => Some(d),
                        _ => None,
                    });
                let dict = symbol::decode(s.data, &input, &tables, previous, &mut self.budget)?;
                self.resources.insert(s.number, Resource::Symbols(dict));
            }
            kind::PATTERN_DICT => {
                let dict = halftone::decode_patterns(s.data, &mut self.budget)?;
                self.resources.insert(s.number, Resource::Patterns(dict));
            }
            kind::TABLES => {
                let t = Arc::new(Table::parse(s.data)?);
                self.resources.insert(s.number, Resource::Table(t));
            }
            k if kind::TEXT.contains(&k) => self.region(s, Self::text)?,
            k if kind::HALFTONE.contains(&k) => self.region(s, Self::halftone)?,
            k if kind::GENERIC.contains(&k) => self.region(s, Self::generic)?,
            k if kind::REFINEMENT.contains(&k) => self.region(s, Self::refinement)?,
            kind::PAGE_INFO => self.page_info(s.data)?,
            kind::END_OF_STRIPE => self.end_of_stripe(s.data)?,
            kind::EXTENSION => {
                let t = Bits::new(s.data).bits(32)?;
                if t & 0x8000_0000 != 0 {
                    return Err(Error::Unsupported("necessary extension segment"));
                }
            }
            // A page this call does not decode, or nothing left to do.
            kind::END_OF_PAGE | kind::END_OF_FILE | kind::PROFILES => {}
            _ => return Err(Error::Unsupported("reserved segment type")),
        }
        Ok(())
    }

    fn page_info(&mut self, data: &[u8]) -> Result<()> {
        if self.page.is_some() {
            return Err(Error::Invalid("second page information segment"));
        }
        let mut b = Bits::new(data);
        let (w, h) = (b.bits(32)?, b.bits(32)?);
        // Resolution, not needed to decode.
        b.bits(32)?;
        b.bits(32)?;
        let flags = b.bits(8)?;
        let striping = b.bits(16)?;
        let default_pixel = flags & 4 != 0;
        let unknown_height = h == u32::MAX;
        let max_stripe = striping & 0x7FFF;
        if w == 0 || h == 0 {
            return Err(Error::Invalid("empty page"));
        }
        if unknown_height && striping & 0x8000 == 0 {
            return Err(Error::Invalid("unknown height on a page without stripes"));
        }
        let h = if unknown_height { max_stripe } else { h };
        self.budget.bitmap(u64::from(w), u64::from(h))?;
        self.page = Some(Page {
            bitmap: Bitmap::new(w, h, default_pixel),
            default_pixel,
            unknown_height,
            max_stripe,
            last_stripe_end: None,
        });
        Ok(())
    }

    /// Make the page at least `h` rows high, when its height is open.
    fn grow(&mut self, h: u64) -> Result<()> {
        let Some(p) = self.page.as_mut() else {
            return Ok(());
        };
        let old = u64::from(p.bitmap.h);
        if p.unknown_height && h > old {
            let w = u64::from(p.bitmap.w);
            self.budget.size(w, h)?;
            self.budget.bitmap(w, h - old)?;
            p.bitmap.set_height(h as u32, p.default_pixel);
        }
        Ok(())
    }

    fn end_of_stripe(&mut self, data: &[u8]) -> Result<()> {
        let y = Bits::new(data).bits(32)?;
        let p = self
            .page
            .as_mut()
            .ok_or(Error::Invalid("segment before page information"))?;
        if p.last_stripe_end.is_some_and(|l| y <= l) {
            return Err(Error::Invalid("stripes out of order"));
        }
        if !p.unknown_height {
            if y >= p.bitmap.h {
                return Err(Error::Invalid("stripe ends below the page"));
            }
            p.last_stripe_end = Some(y);
            return Ok(());
        }
        let next = u64::from(y) + 1 + u64::from(p.max_stripe);
        let w = u64::from(p.bitmap.w);
        // Room for the next stripe, if there can be one; the page is cropped
        // to its last stripe at the end.
        self.budget.size(w, u64::from(y) + 1)?;
        let h = if self.budget.size(w, next).is_ok() {
            next
        } else {
            u64::from(y) + 1
        };
        p.last_stripe_end = Some(y);
        self.grow(h)
    }

    /// The symbols of every dictionary referred to, in order.
    fn symbols(&mut self, refs: &[u32]) -> Result<Vec<Arc<Bitmap>>> {
        let mut dicts = Vec::new();
        for r in refs {
            match self.resources.get(r) {
                Some(Resource::Symbols(d)) => dicts.push(d),
                Some(_) => {}
                None if self.opts.lenient => {}
                None => return Err(Error::Invalid("referred-to segment not found")),
            }
        }
        let n: usize = dicts.iter().map(|d| d.symbols.len()).sum();
        self.budget.symbols(n as u64)?;
        let mut out = Vec::with_capacity(n);
        for d in dicts {
            out.extend_from_slice(&d.symbols);
        }
        Ok(out)
    }

    fn tables(&self, refs: &[u32]) -> Vec<Arc<Table>> {
        refs.iter()
            .filter_map(|r| match self.resources.get(r) {
                Some(Resource::Table(t)) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    /// Decode a region segment and draw it or keep it.
    fn region(
        &mut self,
        s: &Segment<'_>,
        decode: fn(&mut Self, &Segment<'_>, &RegionInfo, &[u8]) -> Result<Bitmap>,
    ) -> Result<()> {
        let mut info = RegionInfo::parse(&mut Bits::new(s.data))?;
        if let Some(rows) = s.rows {
            if rows > info.h {
                return Err(Error::Invalid("row count above the region height"));
            }
            info.h = rows;
        }
        if self.page.is_none() {
            return Err(Error::Invalid("segment before page information"));
        }
        let bm = decode(self, s, &info, &s.data[RegionInfo::LEN..])?;
        // Immediate segment types have bit 1 set.
        if s.kind & 2 == 0 {
            self.resources.insert(s.number, Resource::Region(bm));
            return Ok(());
        }
        self.grow(u64::from(info.y) + u64::from(info.h))?;
        if let Some(p) = self.page.as_mut() {
            self.budget.charge(bm.data.len() as u64)?;
            p.bitmap
                .draw(&bm, i64::from(info.x), i64::from(info.y), info.op);
        }
        Ok(())
    }

    fn generic(&mut self, _: &Segment<'_>, info: &RegionInfo, data: &[u8]) -> Result<Bitmap> {
        let mut b = Bits::new(data);
        let flags = b.bits(8)?;
        self.budget.bitmap(u64::from(info.w), u64::from(info.h))?;
        if flags & 1 != 0 {
            return Ok(mmr::decode(b.rest(), info.w, info.h, self.budget.lenient)?.0);
        }
        let id = ((flags >> 1) & 3) as u8;
        let t = Template::parse(id, id == 0 && flags & 0x10 != 0, &mut b)?;
        self.budget.charge(t.contexts() as u64 / 8)?;
        let mut cx = vec![0; t.contexts()];
        let mut a = Arith::new(b.rest());
        let tpgdon = flags & 8 != 0;
        Ok(generic::decode(
            &mut a, &mut cx, &t, tpgdon, None, info.w, info.h,
        ))
    }

    fn refinement(&mut self, s: &Segment<'_>, info: &RegionInfo, data: &[u8]) -> Result<Bitmap> {
        let mut b = Bits::new(data);
        let flags = b.bits(8)?;
        let t = RefTemplate::parse(flags & 1 != 0, &mut b)?;
        self.budget.bitmap(u64::from(info.w), u64::from(info.h))?;
        let mut reference = None;
        for r in &s.refs {
            match self.resources.remove(r) {
                Some(Resource::Region(bm)) => reference = Some(bm),
                Some(other) => {
                    self.resources.insert(*r, other);
                }
                None if self.opts.lenient => {}
                None => return Err(Error::Invalid("referred-to region not found")),
            }
        }
        let reference = if let Some(r) = reference {
            r
        } else {
            self.budget.bitmap(u64::from(info.w), u64::from(info.h))?;
            let p = self
                .page
                .as_ref()
                .ok_or(Error::Invalid("segment before page information"))?;
            p.bitmap
                .extract(i64::from(info.x), i64::from(info.y), info.w, info.h)
        };
        if (reference.w, reference.h) != (info.w, info.h) {
            return Err(Error::Invalid("refinement of a region of another size"));
        }
        self.budget.charge(t.contexts() as u64 / 8)?;
        let mut cx = vec![0; t.contexts()];
        let mut a = Arith::new(b.rest());
        let r = Reference {
            bitmap: &reference,
            dx: 0,
            dy: 0,
        };
        let tpgron = flags & 2 != 0;
        Ok(refinement::decode(
            &mut a, &mut cx, t, tpgron, &r, info.w, info.h,
        ))
    }

    fn halftone(&mut self, s: &Segment<'_>, info: &RegionInfo, data: &[u8]) -> Result<Bitmap> {
        let dict = s
            .refs
            .iter()
            .find_map(|r| match self.resources.get(r) {
                Some(Resource::Patterns(p)) => Some(p),
                _ => None,
            })
            .ok_or(Error::Invalid("halftone region without patterns"))?;
        halftone::decode_region(data, info.w, info.h, dict, &mut self.budget)
    }

    fn text(&mut self, s: &Segment<'_>, info: &RegionInfo, data: &[u8]) -> Result<Bitmap> {
        let mut b = Bits::new(data);
        let flags = b.bits(16)?;
        let huff = flags & 1 != 0;
        let refine = flags & 2 != 0;
        let hflags = if huff { b.bits(16)? } else { 0 };
        let template = if refine {
            RefTemplate::parse(flags & 0x8000 != 0, &mut b)?
        } else {
            RefTemplate {
                one: true,
                at: [(0, 0); 2],
            }
        };
        // SBDSOFFSET is a signed five-bit field.
        let ds_offset = i64::from((flags >> 10) & 31);
        let p = text::Params {
            refine,
            w: info.w,
            h: info.h,
            instances: b.bits(32)?,
            log_strips: (flags >> 2) & 3,
            def_pixel: flags & 0x200 != 0,
            op: Op::from_bits((flags >> 7) & 3)?,
            transposed: flags & 0x40 != 0,
            corner: (flags >> 4) & 3,
            ds_offset: if ds_offset > 15 {
                ds_offset - 32
            } else {
                ds_offset
            },
            template,
            read_final_oob: false,
        };
        let symbols = self.symbols(&s.refs)?;
        let mut gr = vec![0u8; if refine { template.contexts() } else { 0 }];
        self.budget.charge(gr.len() as u64 / 8)?;
        if huff {
            let custom = self.tables(&s.refs);
            let mut src = TableSource::new(&custom);
            let t = HuffTables {
                fs: src.pick(hflags & 3, &[6, 7])?,
                ds: src.pick((hflags >> 2) & 3, &[8, 9, 10])?,
                dt: src.pick((hflags >> 4) & 3, &[11, 12, 13])?,
                rdw: src.pick((hflags >> 6) & 3, &[14, 15])?,
                rdh: src.pick((hflags >> 8) & 3, &[14, 15])?,
                rdx: src.pick((hflags >> 10) & 3, &[14, 15])?,
                rdy: src.pick((hflags >> 12) & 3, &[14, 15])?,
                rsize: src.pick((hflags >> 14) & 1, &[1])?,
            };
            let codes = SymCodes::Prefix(symbol_id_codes(&mut b, symbols.len())?);
            let mut c = Coder::Huff(&mut b, &t, &codes);
            text::decode(&p, &mut c, &symbols, &mut gr, &mut self.budget)
        } else {
            let mut cx = ArithCtx::new(symbols.len(), &mut self.budget)?;
            let mut a = Arith::new(b.rest());
            let mut c = Coder::Arith(&mut a, &mut cx);
            text::decode(&p, &mut c, &symbols, &mut gr, &mut self.budget)
        }
    }
}

/// 7.4.3.1.7.
fn symbol_id_codes(b: &mut Bits<'_>, n: usize) -> Result<Prefix> {
    let mut run_lens = [0u8; 35];
    for l in &mut run_lens {
        *l = b.bits(4)? as u8;
    }
    let runs = Prefix::new(&run_lens);
    let mut lens: Vec<u8> = Vec::new();
    while lens.len() < n {
        let (v, count) = match runs.read(b)? {
            l @ 0..=31 => (l as u8, 1),
            32 => {
                let prev = *lens
                    .last()
                    .ok_or(Error::Invalid("symbol ID run without a previous length"))?;
                (prev, b.bits(2)? as usize + 3)
            }
            33 => (0, b.bits(3)? as usize + 3),
            _ => (0, b.bits(7)? as usize + 11),
        };
        if lens.len() + count > n {
            return Err(Error::Invalid("symbol ID code lengths"));
        }
        lens.resize(lens.len() + count, v);
    }
    b.align();
    Ok(Prefix::new(&lens))
}
