//! Decode-side parse of the three Vorbis header packets (ISO Vorbis I §4.2).
//!
//! Mirrors lewton 0.10.2's `header.rs` field for field (the oracle this decoder
//! is gated against), with two kinds of deliberate difference:
//!
//! - **Hostile input is bounded.** Every allocation is either proportional to
//!   bits actually present in the packet or charged against a fixed [`Budget`];
//!   a 100-byte setup header cannot request gigabytes (an *ordered* codebook can
//!   declare 2^24 entries in a handful of bits, and `entries x dimensions` VQ
//!   tables multiply that by up to 65535).
//! - **Decode-ready tables.** Codebooks get a multi-level Huffman lookup table
//!   (one indexed load per codeword in the common case) instead of a tree, and
//!   floor 1 gets its neighbour / sort tables precomputed once instead of per
//!   packet.
//!
//! Spec corrections where lewton deviates (each would only matter for a stream
//! libvorbis never produces): residue cascade books are read for all 8 cascade
//! bits (lewton reads 7; FFmpeg and libvorbis read 8), and a floor-0 book index
//! equal to the codebook count is rejected (lewton accepts it and would index
//! out of range).

use super::bits::BitReader;
use crate::{Error, Result};

/// Header-parse allocation caps. Real libvorbis streams use a few thousand
/// entries per book and well under 10^6 in total; these caps sit far above any
/// real stream while bounding what a hostile header can make us allocate
/// (~16M x 4 B per category).
const MAX_TOTAL_ENTRIES: usize = 1 << 22;
const MAX_TOTAL_VQ: usize = 1 << 22;
/// Link entries address subtables with 21 bits.
const MAX_TOTAL_TABLE: usize = 1 << 21;

/// Main-table width of the Huffman lookup (codes up to this long resolve in one
/// load). 2^10 x 4 B = 4 KiB per book.
const FAST_BITS: u32 = 10;
/// Width of each second-and-later level for longer codes.
const SUB_BITS: u32 = 8;

struct Budget {
    entries: usize,
    vq: usize,
    table: usize,
}

impl Budget {
    fn charge(have: &mut usize, n: usize, what: &str) -> Result<()> {
        if n > *have {
            return Err(Error::unsupported(format!(
                "vorbis setup: {what} exceeds the decoder's allocation cap"
            )));
        }
        *have -= n;
        Ok(())
    }
}

fn eop_err(what: &str) -> Error {
    Error::invalid(format!("vorbis {what}: unexpected end of header packet"))
}

/// `ilog`: number of significant bits (`ilog(0) = 0`).
pub(crate) fn ilog(v: u64) -> u32 {
    64 - v.leading_zeros()
}

/// Vorbis float32 unpack (codebook min/delta), as lewton computes it.
fn float32_unpack(val: u32) -> f32 {
    let sgn = val & 0x8000_0000;
    let exp = (val & 0x7fe0_0000) >> 21;
    let mantissa = (val & 0x001f_ffff) as f64;
    let m = if sgn != 0 { -mantissa } else { mantissa };
    (m as f32) * (exp as f32 - 788.0).exp2()
}

/// Greatest `r` with `r^dims <= entries` (`dims >= 1`).
fn lookup1_values(entries: u32, dims: u32) -> u32 {
    let pow_le = |r: u64| -> bool {
        let mut p: u64 = 1;
        for _ in 0..dims {
            p = p.saturating_mul(r);
            if p > u64::from(entries) {
                return false;
            }
        }
        true
    };
    // Float estimate, then step to the exact answer.
    let mut r = (f64::from(entries)).powf(1.0 / f64::from(dims)).floor() as u64;
    while r > 0 && !pow_le(r) {
        r -= 1;
    }
    while pow_le(r + 1) {
        r += 1;
    }
    r as u32
}

// ---------------------------------------------------------------------------
// Identification + comment headers
// ---------------------------------------------------------------------------

/// The identification header (§4.2.2).
#[derive(Debug, Clone)]
pub struct IdentHeader {
    pub channels: u8,
    pub sample_rate: u32,
    pub bitrate_maximum: i32,
    pub bitrate_nominal: i32,
    pub bitrate_minimum: i32,
    /// log2 of the short / long block sizes (6..=13).
    pub blocksize_0: u8,
    pub blocksize_1: u8,
}

fn check_prefix(packet: &[u8], kind: u8, what: &str) -> Result<()> {
    if packet.len() < 7 || &packet[1..7] != b"vorbis" {
        return Err(Error::invalid(format!(
            "vorbis {what}: not a Vorbis header"
        )));
    }
    if packet[0] != kind {
        return Err(Error::invalid(format!(
            "vorbis {what}: wrong header type {}",
            packet[0]
        )));
    }
    Ok(())
}

pub(crate) fn parse_ident(packet: &[u8]) -> Result<IdentHeader> {
    check_prefix(packet, 1, "ident")?;
    if packet.len() < 30 {
        return Err(eop_err("ident"));
    }
    let u32le =
        |o: usize| u32::from_le_bytes([packet[o], packet[o + 1], packet[o + 2], packet[o + 3]]);
    let version = u32le(7);
    if version != 0 {
        return Err(Error::unsupported(
            "vorbis ident: unsupported vorbis_version",
        ));
    }
    let channels = packet[11];
    let sample_rate = u32le(12);
    let bs = packet[28];
    let (blocksize_0, blocksize_1) = (bs & 0x0f, bs >> 4);
    let framing = packet[29] & 1;
    if !(6..=13).contains(&blocksize_0)
        || !(6..=13).contains(&blocksize_1)
        || blocksize_0 > blocksize_1
        || framing != 1
        || channels == 0
        || sample_rate == 0
    {
        return Err(Error::invalid("vorbis ident: bad header fields"));
    }
    Ok(IdentHeader {
        channels,
        sample_rate,
        bitrate_maximum: u32le(16) as i32,
        bitrate_nominal: u32le(20) as i32,
        bitrate_minimum: u32le(24) as i32,
        blocksize_0,
        blocksize_1,
    })
}

/// The comment header (§5): vendor string and `KEY=value` user comments.
/// Non-UTF-8 bytes are replaced (lossy), never rejected.
#[derive(Debug, Clone, Default)]
pub struct CommentHeader {
    pub vendor: String,
    pub comments: Vec<String>,
}

pub(crate) fn parse_comment(packet: &[u8]) -> Result<CommentHeader> {
    check_prefix(packet, 3, "comment")?;
    let mut p = 7usize;
    let mut take = |n: usize| -> Result<&[u8]> {
        let s = packet
            .get(p..p.checked_add(n).ok_or_else(|| eop_err("comment"))?)
            .ok_or_else(|| eop_err("comment"))?;
        p += n;
        Ok(s)
    };
    let len = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let vlen = len(take(4)?);
    let vendor = String::from_utf8_lossy(take(vlen)?).into_owned();
    let n = len(take(4)?);
    // Each comment needs at least its 4-byte length: bounds the Vec by the packet.
    let mut comments = Vec::with_capacity(n.min(packet.len() / 4));
    for _ in 0..n {
        let l = len(take(4)?);
        comments.push(String::from_utf8_lossy(take(l)?).into_owned());
    }
    Ok(CommentHeader { vendor, comments })
}

// ---------------------------------------------------------------------------
// Codebooks
// ---------------------------------------------------------------------------

/// A decode-side codebook: a multi-level Huffman lookup table plus the VQ
/// value table (`entries x dim` scalars, empty for lookup type 0).
pub(crate) struct Codebook {
    pub dim: usize,
    pub entries: u32,
    /// Width of the first table level.
    fast_bits: u32,
    /// Leaf: `entry << 6 | code_len` (len 1..=32). Link: `base << 11 |
    /// sub_bits << 6` (low six bits zero). Empty when no entry is used.
    table: Vec<u32>,
    /// VQ lookup values, row `e` at `e*dim..(e+1)*dim`.
    pub vq: Vec<f32>,
    pub has_vq: bool,
}

impl Codebook {
    /// A book with no used entry cannot decode anything.
    #[inline(always)]
    pub fn usable(&self) -> bool {
        !self.table.is_empty()
    }

    /// Decode one codeword and return its entry number. Requires
    /// [`Self::usable`]. Past the end of the packet the result is garbage and
    /// [`BitReader::eop`] is set; callers check it.
    #[inline(always)]
    pub fn decode(&self, br: &mut BitReader) -> u32 {
        br.ensure32();
        let w = br.window();
        let e = self.table[(w & ((1u64 << self.fast_bits) - 1)) as usize];
        if e & 63 != 0 {
            br.consume(e & 63);
            return e >> 6;
        }
        self.decode_long(br, w, e)
    }

    #[inline(never)]
    fn decode_long(&self, br: &mut BitReader, w: u64, mut e: u32) -> u32 {
        let mut shift = self.fast_bits;
        loop {
            let sub = (e >> 6) & 31;
            let base = (e >> 11) as usize;
            e = self.table[base + ((w >> shift) & ((1u64 << sub) - 1)) as usize];
            if e & 63 != 0 {
                br.consume(e & 63);
                return e >> 6;
            }
            shift += sub;
        }
    }

    /// The VQ row for `entry` (`dim` scalars).
    #[inline(always)]
    pub fn row(&self, entry: u32) -> &[f32] {
        let o = entry as usize * self.dim;
        &self.vq[o..o + self.dim]
    }
}

/// Canonical codeword assignment (libvorbis `_make_words`): the natural
/// (MSb-first) codeword per used entry. Fails on an overpopulated length set.
fn make_words(lengths: &[u8]) -> Result<Vec<u32>> {
    let mut codes = vec![0u32; lengths.len()];
    let mut marker = [0u32; 33];
    for (i, &l) in lengths.iter().enumerate() {
        let length = l as usize;
        if length == 0 {
            continue;
        }
        let mut entry = marker[length];
        if length < 32 && (entry >> length) != 0 {
            return Err(Error::invalid("vorbis setup: overpopulated huffman tree"));
        }
        codes[i] = entry;
        let mut j = length;
        while j > 0 {
            if marker[j] & 1 != 0 {
                if j == 1 {
                    marker[1] = marker[1].wrapping_add(1);
                } else {
                    marker[j] = marker[j - 1] << 1;
                }
                break;
            }
            marker[j] = marker[j].wrapping_add(1);
            j -= 1;
        }
        for j in (length + 1)..33 {
            if (marker[j] >> 1) == entry {
                entry = marker[j];
                marker[j] = marker[j - 1] << 1;
            } else {
                break;
            }
        }
    }
    Ok(codes)
}

/// Fill one table level. `codes` are `(lsb_first_code, len, entry)` sharing the
/// low `shift` bits; the level is `2^bits` slots at `base`.
fn fill_level(
    table: &mut Vec<u32>,
    base: usize,
    bits: u32,
    shift: u32,
    codes: &[(u32, u8, u32)],
    budget: &mut Budget,
) -> Result<()> {
    let mask = (1u32 << bits) - 1;
    let mut long: Vec<(u32, u8, u32)> = Vec::new();
    for &(r, len, entry) in codes {
        let rem = u32::from(len) - shift;
        if rem <= bits {
            let idx = (r >> shift) & ((1u32 << rem) - 1);
            let step = 1usize << rem;
            let mut k = idx as usize;
            while k < (1usize << bits) {
                table[base + k] = (entry << 6) | u32::from(len);
                k += step;
            }
        } else {
            long.push((r, len, entry));
        }
    }
    if long.is_empty() {
        return Ok(());
    }
    // Group the longer codes by their slot in this level.
    long.sort_unstable_by_key(|&(r, _, _)| (r >> shift) & mask);
    let mut i = 0;
    while i < long.len() {
        let slot = (long[i].0 >> shift) & mask;
        let mut j = i;
        let mut max_len = 0u32;
        while j < long.len() && (long[j].0 >> shift) & mask == slot {
            max_len = max_len.max(u32::from(long[j].1));
            j += 1;
        }
        let sub = (max_len - shift - bits).min(SUB_BITS);
        let sub_base = table.len();
        Budget::charge(&mut budget.table, 1 << sub, "huffman tables")?;
        if sub_base + (1 << sub) > MAX_TOTAL_TABLE {
            return Err(Error::unsupported("vorbis setup: huffman tables too large"));
        }
        table.resize(sub_base + (1 << sub), 0);
        table[base + slot as usize] = ((sub_base as u32) << 11) | (sub << 6);
        fill_level(table, sub_base, sub, shift + bits, &long[i..j], budget)?;
        i = j;
    }
    Ok(())
}

/// Build the lookup table from codeword lengths (lewton's validity rules: a
/// single used entry must have length 1 and decodes from one bit of either
/// value; otherwise the tree must be exactly complete).
fn build_table(lengths: &[u8], budget: &mut Budget) -> Result<(u32, Vec<u32>)> {
    let mut used = 0usize;
    let mut last = 0usize;
    let mut kraft: u64 = 0;
    let mut max_len = 0u32;
    for (i, &l) in lengths.iter().enumerate() {
        if l > 0 {
            used += 1;
            last = i;
            kraft += 1u64 << (32 - u32::from(l));
            max_len = max_len.max(u32::from(l));
        }
    }
    if used == 0 {
        return Ok((0, Vec::new()));
    }
    if used == 1 {
        if lengths[last] != 1 {
            return Err(Error::invalid(
                "vorbis setup: invalid single-entry codebook",
            ));
        }
        let leaf = ((last as u32) << 6) | 1;
        return Ok((1, vec![leaf, leaf]));
    }
    if kraft != 1u64 << 32 {
        return Err(Error::invalid(if kraft > 1u64 << 32 {
            "vorbis setup: overpopulated huffman tree"
        } else {
            "vorbis setup: underpopulated huffman tree"
        }));
    }
    let natural = make_words(lengths)?;
    let codes: Vec<(u32, u8, u32)> = lengths
        .iter()
        .enumerate()
        .filter(|(_, &l)| l > 0)
        .map(|(e, &l)| {
            let r = natural[e].reverse_bits() >> (32 - u32::from(l));
            (r, l, e as u32)
        })
        .collect();
    let fast = max_len.min(FAST_BITS);
    Budget::charge(&mut budget.table, 1 << fast, "huffman tables")?;
    let mut table = vec![0u32; 1 << fast];
    fill_level(&mut table, 0, fast, 0, &codes, budget)?;
    // A complete code fills every slot; a zero slot would loop forever.
    if table.contains(&0) {
        return Err(Error::invalid("vorbis setup: inconsistent huffman tree"));
    }
    Ok((fast, table))
}

fn read_codebook(br: &mut BitReader, budget: &mut Budget) -> Result<Codebook> {
    let eop = || eop_err("setup");
    if br.read(24) != 0x56_4342 {
        return Err(Error::invalid("vorbis setup: bad codebook sync pattern"));
    }
    let dim = br.read(16);
    let entries = br.read(24);
    let ordered = br.read_bool();
    if br.eop() {
        return Err(eop());
    }
    Budget::charge(&mut budget.entries, entries as usize, "codebook entries")?;

    let mut lengths: Vec<u8> = Vec::with_capacity(entries as usize);
    if !ordered {
        let sparse = br.read_bool();
        for _ in 0..entries {
            let l = if sparse {
                if br.read_bool() {
                    br.read(5) as u8 + 1
                } else {
                    0
                }
            } else {
                br.read(5) as u8 + 1
            };
            if br.eop() {
                return Err(eop());
            }
            lengths.push(l);
        }
    } else {
        let mut cur: u32 = 0;
        let mut len = br.read(5) + 1;
        while cur < entries {
            let number = br.read(ilog(u64::from(entries - cur)));
            if br.eop() {
                return Err(eop());
            }
            if number > entries - cur || (number > 0 && len > 32) {
                return Err(Error::invalid("vorbis setup: codebook length overflow"));
            }
            lengths.extend(std::iter::repeat_n(len as u8, number as usize));
            cur += number;
            len += 1;
        }
    }

    let lookup_type = br.read(4);
    if lookup_type > 2 {
        return Err(Error::invalid("vorbis setup: bad codebook lookup type"));
    }
    let mut vq = Vec::new();
    if lookup_type != 0 {
        let min = float32_unpack(br.read(32));
        let delta = float32_unpack(br.read(32));
        let value_bits = br.read(4) + 1;
        let seq_p = br.read_bool();
        if br.eop() {
            return Err(eop());
        }
        if dim == 0 {
            return Err(Error::invalid(
                "vorbis setup: VQ codebook with zero dimensions",
            ));
        }
        let lookup_values: u64 = if lookup_type == 1 {
            u64::from(lookup1_values(entries, dim))
        } else {
            u64::from(entries) * u64::from(dim)
        };
        // Every multiplicand costs `value_bits` header bits: bound by the packet.
        let need = lookup_values.saturating_mul(u64::from(value_bits));
        let have = br.bits_left() as u64;
        if need > have {
            return Err(eop());
        }
        let mut mults = Vec::with_capacity(lookup_values as usize);
        for _ in 0..lookup_values {
            mults.push(br.read(value_bits));
        }
        let total = (entries as usize)
            .checked_mul(dim as usize)
            .ok_or_else(|| Error::unsupported("vorbis setup: VQ table too large"))?;
        Budget::charge(&mut budget.vq, total, "VQ tables")?;
        vq.reserve_exact(total);
        let dim = dim as usize;
        if lookup_type == 1 {
            let lv = mults.len() as u64;
            for off in 0..u64::from(entries) {
                let mut last = 0.0f32;
                let mut div: u64 = 1;
                for _ in 0..dim {
                    let m = mults[((off / div) % lv) as usize];
                    let v = m as f32 * delta + min + last;
                    if seq_p {
                        last = v;
                    }
                    vq.push(v);
                    div = div.saturating_mul(lv);
                }
            }
        } else {
            for off in 0..entries as usize {
                let mut last = 0.0f32;
                for &m in &mults[off * dim..(off + 1) * dim] {
                    let v = m as f32 * delta + min + last;
                    if seq_p {
                        last = v;
                    }
                    vq.push(v);
                }
            }
        }
    }

    let (fast_bits, table) = build_table(&lengths, budget)?;
    Ok(Codebook {
        dim: dim as usize,
        entries,
        fast_bits,
        table,
        has_vq: lookup_type != 0,
        vq,
    })
}

// ---------------------------------------------------------------------------
// Floors
// ---------------------------------------------------------------------------

pub(crate) struct Floor0 {
    pub order: u32,
    pub amp_bits: u32,
    pub amp_offset: u32,
    pub books: Vec<u8>,
    pub book_bits: u32,
    /// `cos(omega)` of the bark map, per blocksize flag, length `n/2`
    /// (lewton's `compute_bark_map_cos_omega`, f32 arithmetic kept as-is).
    pub cos_omega: [Vec<f32>; 2],
}

pub(crate) struct Floor1 {
    pub partition_class: Vec<u8>,
    pub class_dims: [u8; 16],
    pub class_subclasses: [u8; 16],
    pub class_masterbook: [u8; 16],
    pub subclass_books: [[i16; 8]; 16],
    pub multiplier: u32,
    /// `[256, 128, 86, 64][multiplier - 1]`.
    pub range: i32,
    /// Bits of the two endpoint values: `ilog(range - 1)`.
    pub ybits: u32,
    pub x_list: Vec<u32>,
    /// Per post `i >= 2`: the low / high neighbour indices (§7.2.4).
    pub low: Vec<u8>,
    pub high: Vec<u8>,
    /// Post indices in ascending `x` order.
    pub sorted: Vec<u8>,
}

pub(crate) enum Floor {
    Zero(Floor0),
    One(Box<Floor1>),
}

fn bark(x: f32) -> f32 {
    13.1 * (0.00074 * x).atan() + 2.24 * (0.000_000_018_5 * x * x).atan() + 0.0001 * x
}

fn bark_map_cos_omega(n: usize, rate: u16, map_size: u16) -> Vec<f32> {
    let hfl = f32::from(rate) / 2.0;
    let hfl_dn = hfl / n as f32;
    let const_part = f32::from(map_size) / bark(hfl);
    let bms_m1 = f32::from(map_size) - 1.0;
    let omega_factor = std::f32::consts::PI / f32::from(map_size);
    (0..n)
        .map(|i| {
            let foobar = (bark(i as f32 * hfl_dn) * const_part).floor();
            (foobar.min(bms_m1) * omega_factor).cos()
        })
        .collect()
}

fn read_floor(br: &mut BitReader, books: &[Codebook], bs: (u8, u8)) -> Result<Floor> {
    let ncb = books.len();
    match br.read(16) {
        0 => {
            let order = br.read(8);
            let rate = br.read(16) as u16;
            let map_size = br.read(16) as u16;
            let amp_bits = br.read(6);
            let amp_offset = br.read(8);
            let nbooks = br.read(4) + 1;
            let mut list = Vec::with_capacity(nbooks as usize);
            for _ in 0..nbooks {
                let b = br.read(8) as usize;
                if b >= ncb {
                    return Err(Error::invalid("vorbis setup: floor0 book out of range"));
                }
                list.push(b as u8);
            }
            if br.eop() {
                return Err(eop_err("setup"));
            }
            Ok(Floor::Zero(Floor0 {
                order,
                amp_bits,
                amp_offset,
                book_bits: ilog(u64::from(nbooks)),
                books: list,
                cos_omega: [
                    bark_map_cos_omega(1 << (bs.0 - 1), rate, map_size),
                    bark_map_cos_omega(1 << (bs.1 - 1), rate, map_size),
                ],
            }))
        }
        1 => {
            let partitions = br.read(5) as usize;
            let mut partition_class = Vec::with_capacity(partitions);
            let mut max_class: i32 = -1;
            for _ in 0..partitions {
                let c = br.read(4) as u8;
                max_class = max_class.max(i32::from(c));
                partition_class.push(c);
            }
            let mut class_dims = [0u8; 16];
            let mut class_subclasses = [0u8; 16];
            let mut class_masterbook = [0u8; 16];
            let mut subclass_books = [[-1i16; 8]; 16];
            for c in 0..(max_class + 1) as usize {
                class_dims[c] = br.read(3) as u8 + 1;
                let sub = br.read(2) as u8;
                class_subclasses[c] = sub;
                if sub != 0 {
                    let mb = br.read(8) as usize;
                    if mb >= ncb {
                        return Err(Error::invalid(
                            "vorbis setup: floor1 masterbook out of range",
                        ));
                    }
                    class_masterbook[c] = mb as u8;
                }
                for slot in subclass_books[c].iter_mut().take(1 << sub) {
                    let b = br.read(8) as i16 - 1;
                    if b >= ncb as i16 {
                        return Err(Error::invalid(
                            "vorbis setup: floor1 subclass book out of range",
                        ));
                    }
                    *slot = b;
                }
            }
            let multiplier = br.read(2) + 1;
            let rangebits = br.read(4);
            let mut values = 2usize;
            for &c in &partition_class {
                values += class_dims[c as usize] as usize;
            }
            if values > 65 {
                return Err(Error::invalid("vorbis setup: floor1 has too many posts"));
            }
            let mut x_list = Vec::with_capacity(values);
            x_list.push(0);
            x_list.push(1u32 << rangebits);
            for &c in &partition_class {
                for _ in 0..class_dims[c as usize] {
                    x_list.push(br.read(rangebits));
                }
            }
            if br.eop() {
                return Err(eop_err("setup"));
            }
            let mut sorted: Vec<u8> = (0..x_list.len() as u8).collect();
            sorted.sort_by_key(|&i| x_list[i as usize]);
            if sorted
                .windows(2)
                .any(|w| x_list[w[0] as usize] == x_list[w[1] as usize])
            {
                return Err(Error::invalid("vorbis setup: duplicate floor1 x value"));
            }
            // Neighbours: the closest earlier post below / above (first wins on
            // ties, which cannot occur — x values are unique).
            let mut low = vec![0u8; x_list.len()];
            let mut high = vec![0u8; x_list.len()];
            for i in 2..x_list.len() {
                let x = x_list[i];
                let (mut lo, mut hi) = (0usize, 1usize);
                let (mut lx, mut hx) = (0u32, u32::MAX);
                for (k, &xk) in x_list[..i].iter().enumerate() {
                    if xk < x && xk >= lx {
                        lo = k;
                        lx = xk;
                    }
                    if xk > x && xk < hx {
                        hi = k;
                        hx = xk;
                    }
                }
                low[i] = lo as u8;
                high[i] = hi as u8;
            }
            let range = [256, 128, 86, 64][(multiplier - 1) as usize];
            Ok(Floor::One(Box::new(Floor1 {
                partition_class,
                class_dims,
                class_subclasses,
                class_masterbook,
                subclass_books,
                multiplier,
                range,
                ybits: ilog((range - 1) as u64),
                x_list,
                low,
                high,
                sorted,
            })))
        }
        _ => Err(Error::invalid("vorbis setup: bad floor type")),
    }
}

// ---------------------------------------------------------------------------
// Residues, mappings, modes
// ---------------------------------------------------------------------------

pub(crate) struct Residue {
    pub kind: u8,
    pub begin: u32,
    pub end: u32,
    pub partition_size: u32,
    pub classifications: u32,
    pub classbook: u8,
    /// Per classification, per pass: the VQ book, or -1.
    pub books: Vec<[i16; 8]>,
}

fn read_residue(br: &mut BitReader, books: &[Codebook]) -> Result<Residue> {
    let kind = br.read(16);
    if kind > 2 {
        return Err(Error::invalid("vorbis setup: bad residue type"));
    }
    let begin = br.read(24);
    let end = br.read(24);
    if begin > end {
        return Err(Error::invalid("vorbis setup: residue begin > end"));
    }
    let partition_size = br.read(24) + 1;
    let classifications = br.read(6) + 1;
    let classbook = br.read(8) as u8;
    let mut cascade = [0u8; 64];
    for c in cascade.iter_mut().take(classifications as usize) {
        let low = br.read(3) as u8;
        let high = if br.read_bool() { br.read(5) as u8 } else { 0 };
        *c = (high << 3) | low;
    }
    let mut rbooks = Vec::with_capacity(classifications as usize);
    for &c in cascade.iter().take(classifications as usize) {
        let mut b = [-1i16; 8];
        for (j, slot) in b.iter_mut().enumerate() {
            if c & (1 << j) != 0 {
                let v = br.read(8) as usize;
                if v >= books.len() || !books[v].has_vq {
                    return Err(Error::invalid("vorbis setup: bad residue book"));
                }
                *slot = v as i16;
            }
        }
        rbooks.push(b);
    }
    if br.eop() {
        return Err(eop_err("setup"));
    }
    if classbook as usize >= books.len() {
        return Err(Error::invalid(
            "vorbis setup: residue classbook out of range",
        ));
    }
    Ok(Residue {
        kind: kind as u8,
        begin,
        end,
        partition_size,
        classifications,
        classbook,
        books: rbooks,
    })
}

pub(crate) struct Mapping {
    /// `(magnitude, angle)` coupling steps, in stream order.
    pub coupling: Vec<(u8, u8)>,
    /// Submap of each channel.
    pub mux: Vec<u8>,
    pub submap_floor: Vec<u8>,
    pub submap_residue: Vec<u8>,
}

fn read_mapping(
    br: &mut BitReader,
    channels: u8,
    nfloors: usize,
    nresidues: usize,
) -> Result<Mapping> {
    if br.read(16) != 0 {
        return Err(Error::invalid("vorbis setup: bad mapping type"));
    }
    let submaps = if br.read_bool() { br.read(4) + 1 } else { 1 };
    let steps = if br.read_bool() { br.read(8) + 1 } else { 0 };
    let cbits = ilog(u64::from(channels) - 1);
    let mut coupling = Vec::with_capacity(steps as usize);
    for _ in 0..steps {
        let m = br.read(cbits);
        let a = br.read(cbits);
        if m == a || m >= u32::from(channels) || a >= u32::from(channels) {
            return Err(Error::invalid("vorbis setup: bad coupling channels"));
        }
        coupling.push((m as u8, a as u8));
    }
    if br.read(2) != 0 {
        return Err(Error::invalid(
            "vorbis setup: nonzero mapping reserved bits",
        ));
    }
    let mux = if submaps > 1 {
        let mut m = Vec::with_capacity(channels as usize);
        for _ in 0..channels {
            let v = br.read(4);
            if v >= submaps {
                return Err(Error::invalid("vorbis setup: mux out of range"));
            }
            m.push(v as u8);
        }
        m
    } else {
        vec![0u8; channels as usize]
    };
    let mut submap_floor = Vec::with_capacity(submaps as usize);
    let mut submap_residue = Vec::with_capacity(submaps as usize);
    for _ in 0..submaps {
        let _unused = br.read(8);
        let f = br.read(8) as usize;
        let r = br.read(8) as usize;
        if f >= nfloors || r >= nresidues {
            return Err(Error::invalid(
                "vorbis setup: submap floor/residue out of range",
            ));
        }
        submap_floor.push(f as u8);
        submap_residue.push(r as u8);
    }
    if br.eop() {
        return Err(eop_err("setup"));
    }
    Ok(Mapping {
        coupling,
        mux,
        submap_floor,
        submap_residue,
    })
}

pub(crate) struct Mode {
    pub blockflag: bool,
    pub mapping: u8,
}

/// The parsed setup header.
pub(crate) struct Setup {
    pub books: Vec<Codebook>,
    pub floors: Vec<Floor>,
    pub residues: Vec<Residue>,
    pub mappings: Vec<Mapping>,
    pub modes: Vec<Mode>,
}

pub(crate) fn parse_setup(packet: &[u8], ident: &IdentHeader) -> Result<Setup> {
    check_prefix(packet, 5, "setup")?;
    let mut br = BitReader::new(&packet[7..]);
    let mut budget = Budget {
        entries: MAX_TOTAL_ENTRIES,
        vq: MAX_TOTAL_VQ,
        table: MAX_TOTAL_TABLE,
    };
    let nbooks = br.read(8) as usize + 1;
    let mut books = Vec::with_capacity(nbooks);
    for _ in 0..nbooks {
        books.push(read_codebook(&mut br, &mut budget)?);
    }
    let ntime = br.read(6) + 1;
    for _ in 0..ntime {
        if br.read(16) != 0 {
            return Err(Error::invalid(
                "vorbis setup: nonzero time domain transform",
            ));
        }
    }
    let nfloors = br.read(6) as usize + 1;
    let mut floors = Vec::with_capacity(nfloors);
    for _ in 0..nfloors {
        floors.push(read_floor(
            &mut br,
            &books,
            (ident.blocksize_0, ident.blocksize_1),
        )?);
    }
    let nresidues = br.read(6) as usize + 1;
    let mut residues = Vec::with_capacity(nresidues);
    for _ in 0..nresidues {
        residues.push(read_residue(&mut br, &books)?);
    }
    let nmappings = br.read(6) as usize + 1;
    let mut mappings = Vec::with_capacity(nmappings);
    for _ in 0..nmappings {
        mappings.push(read_mapping(&mut br, ident.channels, nfloors, nresidues)?);
    }
    let nmodes = br.read(6) as usize + 1;
    let mut modes = Vec::with_capacity(nmodes);
    for _ in 0..nmodes {
        let blockflag = br.read_bool();
        let wt = br.read(16);
        let tt = br.read(16);
        let mapping = br.read(8) as usize;
        if wt != 0 || tt != 0 || mapping >= nmappings {
            return Err(Error::invalid("vorbis setup: bad mode"));
        }
        modes.push(Mode {
            blockflag,
            mapping: mapping as u8,
        });
    }
    if !br.read_bool() || br.eop() {
        return Err(Error::invalid("vorbis setup: framing bit not set"));
    }
    Ok(Setup {
        books,
        floors,
        residues,
        mappings,
        modes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup1_values_matches_spec() {
        assert_eq!(lookup1_values(1025, 10), 2);
        assert_eq!(lookup1_values(1024, 10), 2);
        assert_eq!(lookup1_values(1023, 10), 1);
        assert_eq!(lookup1_values(3126, 5), 5);
        assert_eq!(lookup1_values(3125, 5), 5);
        assert_eq!(lookup1_values(3124, 5), 4);
        assert_eq!(lookup1_values(1, 1), 1);
        assert_eq!(lookup1_values(0, 3), 0);
        assert_eq!(lookup1_values(16_777_215, 1), 16_777_215);
        assert_eq!(lookup1_values(6561, 8), 3);
        assert_eq!(lookup1_values(6560, 8), 2);
        assert_eq!(lookup1_values(1, 65535), 1);
    }

    #[test]
    fn make_words_matches_spec_example() {
        let codes = make_words(&[2, 4, 4, 4, 4, 2, 3, 3]).unwrap();
        assert_eq!(
            codes,
            vec![0b00, 0b0100, 0b0101, 0b0110, 0b0111, 0b10, 0b110, 0b111]
        );
    }

    /// Every codeword of a mixed short/long complete code decodes to its entry
    /// through the multi-level table, consuming exactly its length.
    #[test]
    fn table_decodes_every_codeword() {
        // A complete code with lengths 1..=20 plus two of length 21 (a chain),
        // forcing several table levels, interleaved with a short-code group.
        let mut lengths: Vec<u8> = (1..=20).collect();
        lengths.push(21);
        lengths.push(21);
        let mut budget = Budget {
            entries: MAX_TOTAL_ENTRIES,
            vq: MAX_TOTAL_VQ,
            table: MAX_TOTAL_TABLE,
        };
        let (fast_bits, table) = build_table(&lengths, &mut budget).unwrap();
        let book = Codebook {
            dim: 1,
            entries: lengths.len() as u32,
            fast_bits,
            table,
            vq: Vec::new(),
            has_vq: false,
        };
        let natural = make_words(&lengths).unwrap();
        for (e, (&c, &l)) in natural.iter().zip(&lengths).enumerate() {
            // Emit the codeword MSb-first into an LSb-first stream.
            let r = c.reverse_bits() >> (32 - u32::from(l));
            let bytes = (u64::from(r) | (0xA5A5u64 << l)).to_le_bytes();
            let mut br = BitReader::new(&bytes);
            assert_eq!(book.decode(&mut br), e as u32, "entry {e}");
            assert_eq!(br.bits_consumed(), l as usize);
        }
    }

    #[test]
    fn underpopulated_and_single_entry_rules() {
        let mut b = Budget {
            entries: MAX_TOTAL_ENTRIES,
            vq: MAX_TOTAL_VQ,
            table: MAX_TOTAL_TABLE,
        };
        assert!(build_table(&[1, 2], &mut b).is_err());
        assert!(build_table(&[1, 1, 1], &mut b).is_err());
        assert!(build_table(&[0, 2, 0], &mut b).is_err());
        let (fb, t) = build_table(&[0, 1, 0], &mut b).unwrap();
        assert_eq!((fb, t.len()), (1, 2));
        assert!(build_table(&[0, 0], &mut b).unwrap().1.is_empty());
    }
}
