//! CCITT T.4 (Group 3, 1-D and 2-D) and T.6 (Group 4) bilevel fax coding.
//!
//! Self-contained: no TIFF types, so a PDF `CCITTFaxDecode` filter can use it
//! directly. The [`Options`] fields map one to one onto the PDF filter
//! parameters: `K` selects [`Coding`] (`K < 0` is [`Coding::Group4`], `K = 0`
//! [`Coding::Group3_1D`], `K > 0` [`Coding::Group3_2D`]), `Columns`,
//! `Rows`, `EncodedByteAlign` and `BlackIs1` are fields of the same name.
//!
//! Output is packed 1-bit rows, most significant bit first, each row padded to
//! a whole byte. With `black_is_1` (the TIFF convention, and what libtiff
//! produces before applying `PhotometricInterpretation`) a black pixel is a 1
//! bit; PDF's default `BlackIs1 = false` wants `black_is_1: false`.
//!
//! Decoding is bounded: every code consumes at least one bit, a row ends when
//! it reaches `columns`, and output never exceeds [`Options::max_output_bytes`].
//!
//! ```
//! use rusty_tiff::ccitt::{self, Coding, Options};
//! // 16x2 image: row 0 all white, row 1 left half black.
//! let pixels = [0x00, 0x00, 0xFF, 0x00];
//! let opts = Options::new(Coding::Group4, 16).with_rows(2);
//! let coded = ccitt::encode(&pixels, &opts).unwrap();
//! assert_eq!(ccitt::decode_g4(&coded, 16, 2, &opts).unwrap(), pixels);
//! ```

use std::fmt;
use std::sync::OnceLock;

/// The coding scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(non_camel_case_types)]
pub enum Coding {
    /// Modified Huffman as TIFF compression 2 uses it: 1-D, no EOL codes,
    /// every row starting on a byte boundary.
    ModifiedHuffman,
    /// T.4 Group 3, 1-D: each row is preceded by an EOL code (TIFF
    /// compression 3 without the 2-D bit; PDF `K = 0`).
    Group3_1D,
    /// T.4 Group 3, 2-D: EOL plus a tag bit selecting 1-D or 2-D coding per
    /// row (TIFF compression 3 with `T4Options` bit 0; PDF `K > 0`).
    Group3_2D,
    /// T.6 Group 4: 2-D coding against the previous row, no EOLs, optional
    /// EOFB (TIFF compression 4; PDF `K < 0`).
    Group4,
}

/// Decoder (and encoder) parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The coding scheme.
    pub coding: Coding,
    /// Pixels per row.
    pub columns: u32,
    /// Number of rows, if known. `None` decodes until an end-of-block /
    /// return-to-control code or the end of the data.
    pub rows: Option<u32>,
    /// Rows begin on byte boundaries (PDF `EncodedByteAlign`). For Group 3
    /// the fill before an EOL is skipped regardless of this flag.
    pub encoded_byte_align: bool,
    /// Black is a 1 bit in the output (input, for [`encode`]).
    pub black_is_1: bool,
    /// Upper bound on decoded bytes; exceeding it is an error.
    pub max_output_bytes: usize,
    /// For encoding [`Coding::Group3_2D`]: one 1-D row every `k` rows.
    pub k: u32,
}

impl Options {
    /// Options for `coding` at `columns` pixels per row, unknown row count,
    /// black = 1, no byte alignment, 256 MiB output cap, `k = 4`.
    #[must_use]
    pub fn new(coding: Coding, columns: u32) -> Self {
        Self {
            coding,
            columns,
            rows: None,
            encoded_byte_align: false,
            black_is_1: true,
            max_output_bytes: 256 << 20,
            k: 4,
        }
    }

    /// Set a known row count.
    #[must_use]
    pub fn with_rows(mut self, rows: u32) -> Self {
        self.rows = Some(rows);
        self
    }
}

/// A CCITT decoding (or encoding) failure, with the row it happened on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CcittError {
    /// Row index where decoding failed.
    pub row: u32,
    /// What went wrong.
    pub message: String,
}

impl fmt::Display for CcittError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "row {}: {}", self.row, self.message)
    }
}

impl std::error::Error for CcittError {}

fn err<T>(row: u32, msg: impl Into<String>) -> Result<T, CcittError> {
    Err(CcittError {
        row,
        message: msg.into(),
    })
}

// ---------------------------------------------------------------------------
// Code tables (ITU-T T.4 tables 2 and 3): (code, length, run).

const WHITE: &[(u16, u8, u16)] = &[
    (0b00110101, 8, 0),
    (0b000111, 6, 1),
    (0b0111, 4, 2),
    (0b1000, 4, 3),
    (0b1011, 4, 4),
    (0b1100, 4, 5),
    (0b1110, 4, 6),
    (0b1111, 4, 7),
    (0b10011, 5, 8),
    (0b10100, 5, 9),
    (0b00111, 5, 10),
    (0b01000, 5, 11),
    (0b001000, 6, 12),
    (0b000011, 6, 13),
    (0b110100, 6, 14),
    (0b110101, 6, 15),
    (0b101010, 6, 16),
    (0b101011, 6, 17),
    (0b0100111, 7, 18),
    (0b0001100, 7, 19),
    (0b0001000, 7, 20),
    (0b0010111, 7, 21),
    (0b0000011, 7, 22),
    (0b0000100, 7, 23),
    (0b0101000, 7, 24),
    (0b0101011, 7, 25),
    (0b0010011, 7, 26),
    (0b0100100, 7, 27),
    (0b0011000, 7, 28),
    (0b00000010, 8, 29),
    (0b00000011, 8, 30),
    (0b00011010, 8, 31),
    (0b00011011, 8, 32),
    (0b00010010, 8, 33),
    (0b00010011, 8, 34),
    (0b00010100, 8, 35),
    (0b00010101, 8, 36),
    (0b00010110, 8, 37),
    (0b00010111, 8, 38),
    (0b00101000, 8, 39),
    (0b00101001, 8, 40),
    (0b00101010, 8, 41),
    (0b00101011, 8, 42),
    (0b00101100, 8, 43),
    (0b00101101, 8, 44),
    (0b00000100, 8, 45),
    (0b00000101, 8, 46),
    (0b00001010, 8, 47),
    (0b00001011, 8, 48),
    (0b01010010, 8, 49),
    (0b01010011, 8, 50),
    (0b01010100, 8, 51),
    (0b01010101, 8, 52),
    (0b00100100, 8, 53),
    (0b00100101, 8, 54),
    (0b01011000, 8, 55),
    (0b01011001, 8, 56),
    (0b01011010, 8, 57),
    (0b01011011, 8, 58),
    (0b01001010, 8, 59),
    (0b01001011, 8, 60),
    (0b00110010, 8, 61),
    (0b00110011, 8, 62),
    (0b00110100, 8, 63),
    (0b11011, 5, 64),
    (0b10010, 5, 128),
    (0b010111, 6, 192),
    (0b0110111, 7, 256),
    (0b00110110, 8, 320),
    (0b00110111, 8, 384),
    (0b01100100, 8, 448),
    (0b01100101, 8, 512),
    (0b01101000, 8, 576),
    (0b01100111, 8, 640),
    (0b011001100, 9, 704),
    (0b011001101, 9, 768),
    (0b011010010, 9, 832),
    (0b011010011, 9, 896),
    (0b011010100, 9, 960),
    (0b011010101, 9, 1024),
    (0b011010110, 9, 1088),
    (0b011010111, 9, 1152),
    (0b011011000, 9, 1216),
    (0b011011001, 9, 1280),
    (0b011011010, 9, 1344),
    (0b011011011, 9, 1408),
    (0b010011000, 9, 1472),
    (0b010011001, 9, 1536),
    (0b010011010, 9, 1600),
    (0b011000, 6, 1664),
    (0b010011011, 9, 1728),
];

const BLACK: &[(u16, u8, u16)] = &[
    (0b0000110111, 10, 0),
    (0b010, 3, 1),
    (0b11, 2, 2),
    (0b10, 2, 3),
    (0b011, 3, 4),
    (0b0011, 4, 5),
    (0b0010, 4, 6),
    (0b00011, 5, 7),
    (0b000101, 6, 8),
    (0b000100, 6, 9),
    (0b0000100, 7, 10),
    (0b0000101, 7, 11),
    (0b0000111, 7, 12),
    (0b00000100, 8, 13),
    (0b00000111, 8, 14),
    (0b000011000, 9, 15),
    (0b0000010111, 10, 16),
    (0b0000011000, 10, 17),
    (0b0000001000, 10, 18),
    (0b00001100111, 11, 19),
    (0b00001101000, 11, 20),
    (0b00001101100, 11, 21),
    (0b00000110111, 11, 22),
    (0b00000101000, 11, 23),
    (0b00000010111, 11, 24),
    (0b00000011000, 11, 25),
    (0b000011001010, 12, 26),
    (0b000011001011, 12, 27),
    (0b000011001100, 12, 28),
    (0b000011001101, 12, 29),
    (0b000001101000, 12, 30),
    (0b000001101001, 12, 31),
    (0b000001101010, 12, 32),
    (0b000001101011, 12, 33),
    (0b000011010010, 12, 34),
    (0b000011010011, 12, 35),
    (0b000011010100, 12, 36),
    (0b000011010101, 12, 37),
    (0b000011010110, 12, 38),
    (0b000011010111, 12, 39),
    (0b000001101100, 12, 40),
    (0b000001101101, 12, 41),
    (0b000011011010, 12, 42),
    (0b000011011011, 12, 43),
    (0b000001010100, 12, 44),
    (0b000001010101, 12, 45),
    (0b000001010110, 12, 46),
    (0b000001010111, 12, 47),
    (0b000001100100, 12, 48),
    (0b000001100101, 12, 49),
    (0b000001010010, 12, 50),
    (0b000001010011, 12, 51),
    (0b000000100100, 12, 52),
    (0b000000110111, 12, 53),
    (0b000000111000, 12, 54),
    (0b000000100111, 12, 55),
    (0b000000101000, 12, 56),
    (0b000001011000, 12, 57),
    (0b000001011001, 12, 58),
    (0b000000101011, 12, 59),
    (0b000000101100, 12, 60),
    (0b000001011010, 12, 61),
    (0b000001100110, 12, 62),
    (0b000001100111, 12, 63),
    (0b0000001111, 10, 64),
    (0b000011001000, 12, 128),
    (0b000011001001, 12, 192),
    (0b000001011011, 12, 256),
    (0b000000110011, 12, 320),
    (0b000000110100, 12, 384),
    (0b000000110101, 12, 448),
    (0b0000001101100, 13, 512),
    (0b0000001101101, 13, 576),
    (0b0000001001010, 13, 640),
    (0b0000001001011, 13, 704),
    (0b0000001001100, 13, 768),
    (0b0000001001101, 13, 832),
    (0b0000001110010, 13, 896),
    (0b0000001110011, 13, 960),
    (0b0000001110100, 13, 1024),
    (0b0000001110101, 13, 1088),
    (0b0000001110110, 13, 1152),
    (0b0000001110111, 13, 1216),
    (0b0000001010010, 13, 1280),
    (0b0000001010011, 13, 1344),
    (0b0000001010100, 13, 1408),
    (0b0000001010101, 13, 1472),
    (0b0000001011010, 13, 1536),
    (0b0000001011011, 13, 1600),
    (0b0000001100100, 13, 1664),
    (0b0000001100101, 13, 1728),
];

/// Extended make-up codes 1792..=2560, shared by both colours.
const EXTENDED: &[(u16, u8, u16)] = &[
    (0b00000001000, 11, 1792),
    (0b00000001100, 11, 1856),
    (0b00000001101, 11, 1920),
    (0b000000010010, 12, 1984),
    (0b000000010011, 12, 2048),
    (0b000000010100, 12, 2112),
    (0b000000010101, 12, 2176),
    (0b000000010110, 12, 2240),
    (0b000000010111, 12, 2304),
    (0b000000011100, 12, 2368),
    (0b000000011101, 12, 2432),
    (0b000000011110, 12, 2496),
    (0b000000011111, 12, 2560),
];

const LOOKUP_BITS: u32 = 13;
const RUN_EOL: i16 = -2;
const RUN_INVALID: i16 = -1;

/// A 13-bit-indexed decode table entry: (code length, run or marker).
type RunTable = Vec<(u8, i16)>;

fn build_run_table(codes: &[(u16, u8, u16)]) -> RunTable {
    let mut t = vec![(0u8, RUN_INVALID); 1 << LOOKUP_BITS];
    let mut put = |code: u16, len: u8, val: i16| {
        let shift = LOOKUP_BITS - u32::from(len);
        let base = (code as usize) << shift;
        for e in &mut t[base..base + (1 << shift)] {
            *e = (len, val);
        }
    };
    for &(c, l, r) in codes.iter().chain(EXTENDED) {
        put(c, l, r as i16);
    }
    put(1, 12, RUN_EOL);
    t
}

fn tables() -> &'static (RunTable, RunTable) {
    static T: OnceLock<(RunTable, RunTable)> = OnceLock::new();
    T.get_or_init(|| (build_run_table(WHITE), build_run_table(BLACK)))
}

// 2-D mode codes, decoded from a 7-bit peek.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Pass,
    Horizontal,
    Vertical(i8),
    Extension,
    /// Seven zero bits: an EOL (or garbage) follows.
    Zeros,
}

fn mode_of(peek7: u32) -> (u8, Mode) {
    if peek7 & 0x40 != 0 {
        (1, Mode::Vertical(0))
    } else if peek7 & 0x20 != 0 {
        // 01x
        if peek7 & 0x10 != 0 {
            (3, Mode::Vertical(1))
        } else {
            (3, Mode::Vertical(-1))
        }
    } else if peek7 & 0x10 != 0 {
        (3, Mode::Horizontal) // 001
    } else if peek7 & 0x08 != 0 {
        (4, Mode::Pass) // 0001
    } else if peek7 & 0x04 != 0 {
        // 00001x
        if peek7 & 0x02 != 0 {
            (6, Mode::Vertical(2))
        } else {
            (6, Mode::Vertical(-2))
        }
    } else if peek7 & 0x02 != 0 {
        // 000001x
        if peek7 & 0x01 != 0 {
            (7, Mode::Vertical(3))
        } else {
            (7, Mode::Vertical(-3))
        }
    } else if peek7 & 0x01 != 0 {
        (7, Mode::Extension) // 0000001
    } else {
        (0, Mode::Zeros)
    }
}

// ---------------------------------------------------------------------------
// Bit reader.

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    /// Bit index just past the last 1 bit in `data` (0 if all zero).
    end_of_ones: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        let end_of_ones = data
            .iter()
            .rposition(|&b| b != 0)
            .map_or(0, |i| i * 8 + 8 - data[i].trailing_zeros() as usize);
        Self {
            data,
            pos: 0,
            end_of_ones,
        }
    }
}

impl Bits<'_> {
    #[inline]
    fn peek(&self, n: u32) -> u32 {
        let byte = self.pos >> 3;
        let mut w = 0u32;
        for i in 0..4 {
            w = (w << 8) | u32::from(*self.data.get(byte + i).unwrap_or(&0));
        }
        (w << (self.pos & 7)) >> (32 - n)
    }
    #[inline]
    fn consume(&mut self, n: u32) {
        self.pos += n as usize;
    }
    fn total(&self) -> usize {
        self.data.len() * 8
    }
    fn overrun(&self) -> bool {
        self.pos > self.total()
    }
    fn at_end(&self) -> bool {
        self.pos >= self.total()
    }
    /// True when no 1 bit remains (end of data or only zero fill left).
    fn rest_is_zero(&self) -> bool {
        self.pos >= self.end_of_ones
    }
    fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }
    /// If the next bits are fill zeros then an EOL (>= 11 zeros, a 1),
    /// consume them and return true; otherwise consume nothing.
    fn take_eol(&mut self) -> bool {
        let total = self.total();
        let mut p = self.pos;
        while p < total && (self.data[p >> 3] >> (7 - (p & 7))) & 1 == 0 {
            p += 1;
        }
        if p < total && p - self.pos >= 11 {
            self.pos = p + 1;
            true
        } else {
            false
        }
    }
}

enum RowEnd {
    Done,
    /// An EOL code interrupted the row (the rest of the row is white).
    Eol,
}

/// Read one run of the given colour (make-up codes summed).
fn read_run(b: &mut Bits<'_>, white: bool, row: u32) -> Result<Option<u32>, CcittError> {
    let (wt, bt) = tables();
    let t = if white { wt } else { bt };
    let mut total = 0u32;
    loop {
        let (len, val) = t[b.peek(LOOKUP_BITS) as usize];
        if val == RUN_INVALID {
            return err(
                row,
                format!("invalid {} run code", if white { "white" } else { "black" }),
            );
        }
        b.consume(u32::from(len));
        if b.overrun() {
            return err(row, "data ended inside a run code");
        }
        if val == RUN_EOL {
            return Ok(None);
        }
        let v = val as u32;
        total += v;
        if v < 64 {
            return Ok(Some(total));
        }
        if total > 1 << 24 {
            return err(row, "run length overflow");
        }
    }
}

/// Append a changing element, cancelling a zero-length run.
#[inline]
fn push_change(cur: &mut Vec<u32>, p: u32, width: u32) {
    if p >= width {
        return;
    }
    if cur.last() == Some(&p) {
        cur.pop();
    } else {
        cur.push(p);
    }
}

fn decode_1d_row(
    b: &mut Bits<'_>,
    width: u32,
    cur: &mut Vec<u32>,
    row: u32,
) -> Result<RowEnd, CcittError> {
    cur.clear();
    let mut a0 = 0u32;
    let mut white = true;
    while a0 < width {
        let Some(run) = read_run(b, white, row)? else {
            return Ok(RowEnd::Eol);
        };
        let a1 = a0.saturating_add(run).min(width);
        push_change(cur, a1, width);
        a0 = a1;
        white = !white;
    }
    Ok(RowEnd::Done)
}

/// `reference` holds the previous row's changing elements followed by at
/// least three `width` sentinels.
fn decode_2d_row(
    b: &mut Bits<'_>,
    width: u32,
    reference: &[u32],
    cur: &mut Vec<u32>,
    row: u32,
) -> Result<RowEnd, CcittError> {
    cur.clear();
    let w = i64::from(width);
    let mut a0: i64 = -1;
    let mut white = true;
    let mut bi = 0usize;
    while a0 < w {
        // b1: first change on the reference line right of a0 whose colour is
        // opposite a0's (even index = change to black).
        while bi > 0 && i64::from(reference[bi - 1]) > a0 {
            bi -= 1;
        }
        let parity = usize::from(!white);
        while i64::from(reference[bi]) <= a0 || (bi & 1) != parity {
            bi += 1;
        }
        let b1 = i64::from(reference[bi]);
        let b2 = i64::from(reference[bi + 1]);

        let (len, mode) = mode_of(b.peek(7));
        match mode {
            Mode::Zeros => {
                if b.peek(12) == 1 {
                    b.consume(12);
                    if b.overrun() {
                        return err(row, "data ended inside an EOL");
                    }
                    return Ok(RowEnd::Eol);
                }
                return err(row, "invalid 2-D mode code");
            }
            Mode::Extension => return err(row, "uncompressed-mode extension not supported"),
            _ => {}
        }
        b.consume(u32::from(len));
        if b.overrun() {
            return err(row, "data ended inside a mode code");
        }
        match mode {
            Mode::Pass => {
                a0 = b2;
            }
            Mode::Horizontal => {
                let start = a0.max(0);
                let (Some(r1), Some(r2)) = (read_run(b, white, row)?, read_run(b, !white, row)?)
                else {
                    return err(row, "EOL inside horizontal mode");
                };
                let a1 = (start + i64::from(r1)).min(w);
                let a2 = (a1 + i64::from(r2)).min(w);
                push_change(cur, a1 as u32, width);
                push_change(cur, a2 as u32, width);
                a0 = a2;
            }
            Mode::Vertical(d) => {
                // A damaged code can put a1 left of a0; libtiff clamps it to
                // a0 (a zero-length run) and carries on, so does this. Every
                // mode consumes bits, so a stuck a0 still terminates.
                let a1 = (b1 + i64::from(d)).max(a0.max(0)).min(w);
                push_change(cur, a1 as u32, width);
                a0 = a1;
                white = !white;
            }
            Mode::Extension | Mode::Zeros => unreachable!(),
        }
    }
    Ok(RowEnd::Done)
}

/// Paint pixels `s..e` of a packed MSB-first row black: a head mask, whole
/// bytes, a tail mask. Setting one bit per pixel was 55% of a G4 decode.
#[inline]
fn paint_span(out: &mut [u8], s: usize, e: usize, black_is_1: bool) {
    if s >= e {
        return;
    }
    let (sb, eb) = (s >> 3, e >> 3);
    let head = 0xFFu8 >> (s & 7);
    // Bits of byte `eb` before `e` (none when `e` is byte-aligned).
    let tail = !(0xFFu8 >> (e & 7));
    let apply = |b: &mut u8, m: u8| {
        if black_is_1 {
            *b |= m;
        } else {
            *b &= !m;
        }
    };
    if sb == eb {
        apply(&mut out[sb], head & tail);
        return;
    }
    apply(&mut out[sb], head);
    let fill = if black_is_1 { 0xFF } else { 0 };
    let mid = &mut out[sb + 1..eb];
    if mid.len() <= 16 {
        for b in mid {
            *b = fill;
        }
    } else {
        mid.fill(fill);
    }
    if tail != 0 {
        apply(&mut out[eb], tail);
    }
}

fn render_row(changes: &[u32], width: u32, out: &mut [u8], black_is_1: bool) {
    // White background; padding bits are cleared below so they are always 0.
    out.fill(if black_is_1 { 0 } else { 0xFF });
    let mut k = 0;
    while k < changes.len() {
        let s = changes[k] as usize;
        let e = changes.get(k + 1).map_or(width, |&x| x) as usize;
        paint_span(out, s, e, black_is_1);
        k += 2;
    }
    if !black_is_1 && !width.is_multiple_of(8) {
        let last = out.len() - 1;
        out[last] &= 0xFFu8 << (8 - width % 8);
    }
}

/// Decode with explicit options. Returns packed rows (see the module docs).
pub fn decode(data: &[u8], opts: &Options) -> Result<Vec<u8>, CcittError> {
    let mut out = Vec::new();
    decode_append(data, opts, &mut out)?;
    Ok(out)
}

/// [`decode`], appending the rows to `out`: each row is rendered straight
/// into its place (no per-row buffer copy, and the TIFF decoder needs no
/// copy of a whole decoded strip into the image).
pub(crate) fn decode_append(
    data: &[u8],
    opts: &Options,
    out: &mut Vec<u8>,
) -> Result<(), CcittError> {
    let width = opts.columns;
    if width == 0 || width > 1 << 20 {
        return err(0, format!("unsupported column count {width}"));
    }
    let stride = width.div_ceil(8) as usize;
    if let Some(rows) = opts.rows {
        if (rows as usize).saturating_mul(stride) > opts.max_output_bytes {
            return err(0, "output would exceed max_output_bytes");
        }
    }
    let base = out.len();
    out.reserve(opts.rows.map_or(0, |r| r as usize * stride));
    let mut b = Bits::new(data);
    let mut reference: Vec<u32> = vec![width; 3];
    let mut cur: Vec<u32> = Vec::new();
    let mut eol_pending = false;
    let mut row: u32 = 0;

    loop {
        if let Some(rows) = opts.rows {
            if row >= rows {
                break;
            }
        } else if b.rest_is_zero() {
            // Unknown row count: trailing fill (or nothing) ends the image.
            break;
        } else if out.len() - base + stride > opts.max_output_bytes {
            return err(row, "output would exceed max_output_bytes");
        }
        let end = match opts.coding {
            Coding::ModifiedHuffman => {
                b.align();
                if b.at_end() {
                    if opts.rows.is_none() {
                        break;
                    }
                    return err(row, "data ended");
                }
                decode_1d_row(&mut b, width, &mut cur, row)?
            }
            Coding::Group4 => {
                if opts.encoded_byte_align {
                    b.align();
                }
                // EOFB (two EOLs) ends the image.
                if b.peek(24) == 0x001001 {
                    break;
                }
                if b.at_end() {
                    if opts.rows.is_none() {
                        break;
                    }
                    return err(row, "data ended");
                }
                decode_2d_row(&mut b, width, &reference, &mut cur, row)?
            }
            Coding::Group3_1D | Coding::Group3_2D => {
                let mut had_eol = eol_pending || b.take_eol();
                if !had_eol && opts.encoded_byte_align {
                    b.align();
                    had_eol = b.take_eol();
                }
                eol_pending = false;
                if had_eol {
                    // Another EOL straight away (after an optional tag bit in
                    // 2-D mode) is RTC: end of page.
                    let save = b.pos;
                    if opts.coding == Coding::Group3_2D {
                        b.consume(1);
                    }
                    if b.take_eol() {
                        break;
                    }
                    b.pos = save;
                }
                if b.at_end() {
                    if opts.rows.is_none() {
                        break;
                    }
                    return err(row, "data ended");
                }
                let two_d = if opts.coding == Coding::Group3_2D {
                    let tag = b.peek(1);
                    b.consume(1);
                    tag == 0
                } else {
                    false
                };
                if two_d {
                    decode_2d_row(&mut b, width, &reference, &mut cur, row)?
                } else {
                    decode_1d_row(&mut b, width, &mut cur, row)?
                }
            }
        };
        if matches!(end, RowEnd::Eol) {
            match opts.coding {
                Coding::Group3_1D | Coding::Group3_2D => {
                    // A premature EOL: keep what was decoded, rest white, and
                    // let the EOL frame the next row.
                    eol_pending = true;
                }
                Coding::Group4 => {
                    if cur.is_empty() {
                        // EOFB at a row start.
                        break;
                    }
                    return err(row, "EOL inside a Group 4 row");
                }
                Coding::ModifiedHuffman => return err(row, "EOL in Modified Huffman data"),
            }
        }
        let at = out.len();
        out.resize(at + stride, 0);
        render_row(&cur, width, &mut out[at..], opts.black_is_1);
        reference.clear();
        reference.extend_from_slice(&cur);
        reference.extend_from_slice(&[width; 3]);
        row += 1;
    }
    if let Some(rows) = opts.rows {
        if row < rows {
            return err(row, format!("only {row} of {rows} rows present"));
        }
    }
    Ok(())
}

/// Decode Group 4 (T.6) data of `width` x `height` pixels. `opts.coding`,
/// `columns` and `rows` are overridden; the other fields apply.
pub fn decode_g4(
    data: &[u8],
    width: u32,
    height: u32,
    opts: &Options,
) -> Result<Vec<u8>, CcittError> {
    let o = Options {
        coding: Coding::Group4,
        columns: width,
        rows: Some(height),
        ..opts.clone()
    };
    decode(data, &o)
}

/// Decode Group 3 (T.4) data, 1-D or 2-D per `two_d`.
pub fn decode_g3(
    data: &[u8],
    width: u32,
    height: u32,
    two_d: bool,
    opts: &Options,
) -> Result<Vec<u8>, CcittError> {
    let o = Options {
        coding: if two_d {
            Coding::Group3_2D
        } else {
            Coding::Group3_1D
        },
        columns: width,
        rows: Some(height),
        ..opts.clone()
    };
    decode(data, &o)
}

// ---------------------------------------------------------------------------
// Encoder.

struct BitOut {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl BitOut {
    /// Append the low `len` bits of `code` (at most 24), most significant
    /// first. `n` (under 8) bits wait in the low end of `acc`; the code is
    /// shifted in whole and every complete byte leaves, so a code costs a
    /// shift and one or two pushes, where it was a loop turn per bit.
    #[inline]
    fn put(&mut self, code: u32, len: u32) {
        self.acc = (self.acc << len) | (code & ((1 << len) - 1));
        self.n += len;
        while self.n >= 8 {
            self.n -= 8;
            self.out.push((self.acc >> self.n) as u8);
        }
    }
    fn align(&mut self) {
        if self.n > 0 {
            self.put(0, 8 - self.n);
        }
    }
}

fn code_for(table: &[(u16, u8, u16)], run: u32) -> (u32, u32) {
    let hit = table
        .iter()
        .chain(EXTENDED)
        .find(|&&(_, _, r)| u32::from(r) == run);
    hit.map_or((0, 0), |&(c, l, _)| (u32::from(c), u32::from(l)))
}

fn put_run(o: &mut BitOut, white: bool, mut run: u32) {
    let t = if white { WHITE } else { BLACK };
    while run >= 2560 {
        let (c, l) = code_for(t, 2560);
        o.put(c, l);
        run -= 2560;
    }
    if run >= 64 {
        let (c, l) = code_for(t, run / 64 * 64);
        o.put(c, l);
        run %= 64;
    }
    let (c, l) = code_for(t, run);
    o.put(c, l);
}

/// The changing elements of a packed row: every pixel whose colour differs
/// from the pixel on its left (white before the row).
///
/// A byte at a time: with black as 1, `b ^ (b >> 1 | left << 7)` has a bit
/// set exactly where a pixel differs from its left neighbour, so a byte
/// without a change (most of a scanned page) costs one test, and each change
/// is one `leading_zeros`. Testing one pixel per loop turn was 17 Ir a pixel.
fn row_changes(row: &[u8], width: u32, black_is_1: bool, out: &mut Vec<u32>) {
    out.clear();
    let width = width as usize;
    let flip = if black_is_1 { 0 } else { 0xFF };
    // The pixel left of the current byte (1 = black).
    let mut left = 0u8;
    for (i, &raw) in row[..width.div_ceil(8)].iter().enumerate() {
        let b = raw ^ flip;
        let mut t = b ^ ((b >> 1) | (left << 7));
        left = b & 1;
        let base = i * 8;
        if base + 8 > width {
            // The last byte: its pad bits are not pixels.
            t &= !(0xFFu8 >> (width - base));
        }
        while t != 0 {
            let k = t.leading_zeros();
            out.push(base as u32 + k);
            t &= !(0x80u8 >> k);
        }
    }
}

fn encode_1d_row(o: &mut BitOut, changes: &[u32], width: u32) {
    let mut a0 = 0u32;
    let mut white = true;
    for &c in changes.iter().chain(std::iter::once(&width)) {
        put_run(o, white, c - a0);
        a0 = c;
        white = !white;
    }
}

fn encode_2d_row(o: &mut BitOut, cur: &[u32], reference: &[u32], width: u32) {
    // Both lists carry three trailing `width` sentinels.
    let w = i64::from(width);
    let mut a0: i64 = -1;
    let mut white = true;
    let mut ai = 0usize;
    let mut bi = 0usize;
    while a0 < w {
        while i64::from(cur[ai]) <= a0 {
            ai += 1;
        }
        let a1 = i64::from(cur[ai]);
        while bi > 0 && i64::from(reference[bi - 1]) > a0 {
            bi -= 1;
        }
        let parity = usize::from(!white);
        while i64::from(reference[bi]) <= a0 || (bi & 1) != parity {
            bi += 1;
        }
        let b1 = i64::from(reference[bi]);
        let b2 = i64::from(reference[bi + 1]);
        if b2 < a1 {
            o.put(0b0001, 4);
            a0 = b2;
        } else if (a1 - b1).abs() <= 3 {
            let (c, l) = match a1 - b1 {
                0 => (1, 1),
                1 => (0b011, 3),
                2 => (0b000011, 6),
                3 => (0b0000011, 7),
                -1 => (0b010, 3),
                -2 => (0b000010, 6),
                _ => (0b0000010, 7),
            };
            o.put(c, l);
            a0 = a1;
            white = !white;
        } else {
            let a2 = i64::from(cur[ai + 1]);
            o.put(0b001, 3);
            put_run(o, white, (a1 - a0.max(0)) as u32);
            put_run(o, !white, (a2 - a1) as u32);
            a0 = a2;
        }
    }
}

/// Encode packed 1-bit rows (`opts.columns` wide, `opts.rows` rows, black per
/// `opts.black_is_1`). Group 3 output starts every row with an EOL and ends
/// with RTC; Group 4 output ends with EOFB.
pub fn encode(pixels: &[u8], opts: &Options) -> Result<Vec<u8>, CcittError> {
    let width = opts.columns;
    let Some(rows) = opts.rows else {
        return err(0, "encoding needs a row count");
    };
    if width == 0 {
        return err(0, "zero columns");
    }
    let stride = width.div_ceil(8) as usize;
    if pixels.len() < stride * rows as usize {
        return err(0, "pixel buffer too short");
    }
    let mut o = BitOut {
        out: Vec::new(),
        acc: 0,
        n: 0,
    };
    let mut reference = vec![width; 3];
    let mut cur = Vec::new();
    for r in 0..rows {
        let row = &pixels[r as usize * stride..][..stride];
        row_changes(row, width, opts.black_is_1, &mut cur);
        match opts.coding {
            Coding::ModifiedHuffman => {
                encode_1d_row(&mut o, &cur, width);
                o.align();
            }
            Coding::Group3_1D => {
                o.put(1, 12);
                encode_1d_row(&mut o, &cur, width);
            }
            Coding::Group3_2D => {
                o.put(1, 12);
                let one_d = r % opts.k.max(1) == 0;
                o.put(u32::from(one_d), 1);
                if one_d {
                    encode_1d_row(&mut o, &cur, width);
                } else {
                    let mut c = cur.clone();
                    c.extend_from_slice(&[width; 3]);
                    encode_2d_row(&mut o, &c, &reference, width);
                }
            }
            Coding::Group4 => {
                if opts.encoded_byte_align {
                    o.align();
                }
                let mut c = cur.clone();
                c.extend_from_slice(&[width; 3]);
                encode_2d_row(&mut o, &c, &reference, width);
            }
        }
        reference.clear();
        reference.extend_from_slice(&cur);
        reference.extend_from_slice(&[width; 3]);
    }
    match opts.coding {
        Coding::ModifiedHuffman => {}
        Coding::Group3_1D => {
            for _ in 0..6 {
                o.put(1, 12);
            }
        }
        Coding::Group3_2D => {
            for _ in 0..6 {
                o.put(1, 12);
                o.put(1, 1);
            }
        }
        Coding::Group4 => {
            o.put(1, 12);
            o.put(1, 12);
        }
    }
    o.align();
    Ok(o.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_changes_matches_per_pixel_scan() {
        let mut seed = 21u32;
        let mut out = Vec::new();
        for width in [1u32, 2, 7, 8, 9, 15, 16, 17, 63, 64, 65, 200] {
            for density in [0u32, 1, 8, 128, 255] {
                for black_is_1 in [false, true] {
                    let row: Vec<u8> = (0..width.div_ceil(8))
                        .map(|_| {
                            // Runs of equal bits of varying length (and
                            // plain noise at the top density).
                            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                            let r = (seed >> 16) as u8;
                            if u32::from(r) < density {
                                (seed >> 8) as u8
                            } else if r & 1 == 0 {
                                0
                            } else {
                                0xFF
                            }
                        })
                        .collect();
                    let mut want = Vec::new();
                    let mut prev_black = false;
                    for x in 0..width as usize {
                        let bit = (row[x >> 3] >> (7 - (x & 7))) & 1 == 1;
                        let black = bit == black_is_1;
                        if black != prev_black {
                            want.push(x as u32);
                            prev_black = black;
                        }
                    }
                    row_changes(&row, width, black_is_1, &mut out);
                    assert_eq!(out, want, "width {width} density {density} {black_is_1}");
                }
            }
        }
    }

    #[test]
    fn bit_writer_matches_per_bit_writes() {
        let mut seed = 9u32;
        let mut o = BitOut {
            out: Vec::new(),
            acc: 0,
            n: 0,
        };
        // The definition: one bit at a time into a byte.
        let (mut want, mut acc, mut n) = (Vec::new(), 0u8, 0u32);
        for i in 0..5000u32 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let len = (seed >> 16) % 14;
            let code = seed >> 8;
            o.put(code, len);
            for k in (0..len).rev() {
                acc = (acc << 1) | ((code >> k) & 1) as u8;
                n += 1;
                if n == 8 {
                    want.push(acc);
                    (acc, n) = (0, 0);
                }
            }
            if i % 97 == 0 {
                o.align();
                if n > 0 {
                    want.push(acc << (8 - n));
                    (acc, n) = (0, 0);
                }
            }
        }
        o.align();
        if n > 0 {
            want.push(acc << (8 - n));
        }
        assert_eq!(o.out, want);
    }

    #[test]
    fn paint_span_matches_per_bit() {
        for width in 1..70usize {
            let bytes = width.div_ceil(8);
            for s in 0..=width {
                for e in s..=width {
                    for black_is_1 in [true, false] {
                        let init = if black_is_1 { 0u8 } else { 0xFF };
                        let mut want = vec![init; bytes];
                        for x in s..e {
                            if black_is_1 {
                                want[x >> 3] |= 0x80 >> (x & 7);
                            } else {
                                want[x >> 3] &= !(0x80 >> (x & 7));
                            }
                        }
                        let mut got = vec![init; bytes];
                        paint_span(&mut got, s, e, black_is_1);
                        assert_eq!(got, want, "{width} {s} {e} {black_is_1}");
                    }
                }
            }
        }
    }

    #[test]
    fn tables_are_prefix_free_and_complete() {
        // build_run_table would silently overwrite on a collision; check none.
        for codes in [WHITE, BLACK] {
            let all: Vec<_> = codes.iter().chain(EXTENDED).copied().collect();
            for (i, &(c1, l1, _)) in all.iter().enumerate() {
                for &(c2, l2, _) in &all[i + 1..] {
                    let l = l1.min(l2);
                    assert_ne!(
                        c1 >> (l1 - l),
                        c2 >> (l2 - l),
                        "prefix clash {c1:b}/{l1} {c2:b}/{l2}"
                    );
                }
            }
            for r in 0..64 {
                assert!(codes.iter().any(|e| u32::from(e.2) == r));
            }
            assert_eq!(codes.len(), 64 + 27);
        }
    }

    fn pseudo_image(w: u32, h: u32, seed: u32) -> Vec<u8> {
        let stride = w.div_ceil(8) as usize;
        let mut px = vec![0u8; stride * h as usize];
        let mut s = seed;
        for y in 0..h as usize {
            let mut x = 0usize;
            let mut black = false;
            while x < w as usize {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let run = match s >> 29 {
                    0 => (s >> 8) % 3000,
                    1 => (s >> 8) % 200,
                    _ => (s >> 8) % 12,
                } as usize;
                if y > 0 && s & 0x100 != 0 {
                    // Copy the row above, for vertical-mode coverage.
                    let (a, b) = px.split_at_mut(y * stride);
                    b[..stride].copy_from_slice(&a[(y - 1) * stride..]);
                    break;
                }
                for xx in x..(x + run).min(w as usize) {
                    if black {
                        px[y * stride + (xx >> 3)] |= 0x80 >> (xx & 7);
                    }
                }
                x += run;
                black = !black;
            }
        }
        px
    }

    #[test]
    fn self_round_trip_all_codings() {
        for &(w, h) in &[(1u32, 1u32), (7, 3), (64, 10), (1728, 40), (3000, 5)] {
            for seed in 0..4 {
                let px = pseudo_image(w, h, seed);
                for coding in [
                    Coding::ModifiedHuffman,
                    Coding::Group3_1D,
                    Coding::Group3_2D,
                    Coding::Group4,
                ] {
                    for black_is_1 in [true, false] {
                        let mut o = Options::new(coding, w).with_rows(h);
                        o.black_is_1 = black_is_1;
                        let src = if black_is_1 {
                            px.clone()
                        } else {
                            let mut v: Vec<u8> = px.iter().map(|b| !b).collect();
                            if w % 8 != 0 {
                                let stride = w.div_ceil(8) as usize;
                                for r in 0..h as usize {
                                    v[r * stride + stride - 1] &= 0xFFu8 << (8 - w % 8);
                                }
                            }
                            v
                        };
                        let enc = encode(&src, &o).unwrap();
                        assert_eq!(decode(&enc, &o).unwrap(), src, "{coding:?} {w}x{h}");
                        // Unknown row count stops at RTC/EOFB or end of data.
                        o.rows = None;
                        assert_eq!(decode(&enc, &o).unwrap(), src, "{coding:?} rows=None");
                    }
                }
            }
        }
    }

    #[test]
    fn garbage_is_bounded() {
        let mut s = 99u32;
        for i in 0..3000 {
            let n = (i % 200) as usize;
            let data: Vec<u8> = (0..n)
                .map(|_| {
                    s = s.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    (s >> 16) as u8
                })
                .collect();
            for coding in [
                Coding::ModifiedHuffman,
                Coding::Group3_1D,
                Coding::Group3_2D,
                Coding::Group4,
            ] {
                let mut o = Options::new(coding, 1 + (i % 300));
                o.max_output_bytes = 1 << 16;
                let _ = decode(&data, &o);
                let _ = decode(&data, &o.clone().with_rows(50));
            }
        }
    }
}
