//! MMR (T.6) decoding as T.88 6.2.6 uses it.
//!
//! The run-code tables (ITU-T T.4 tables 2 and 3) are the ones in this
//! workspace's `rusty_tiff::ccitt` (same authors, Apache-2.0).

use crate::bitmap::Bitmap;
use crate::{Error, Result};
use std::sync::OnceLock;

/// (code, length, run).
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
const RUN_INVALID: u16 = u16::MAX;

/// A 13-bit-indexed decode table: (code length, run).
type RunTable = Box<[(u8, u16); 1 << LOOKUP_BITS]>;

fn build_run_table(codes: &[(u16, u8, u16)]) -> RunTable {
    let mut t = vec![(0u8, RUN_INVALID); 1 << LOOKUP_BITS];
    for &(code, len, run) in codes.iter().chain(EXTENDED) {
        let shift = LOOKUP_BITS - u32::from(len);
        let base = usize::from(code) << shift;
        t[base..base + (1 << shift)].fill((len, run));
    }
    let t = t.into_boxed_slice().try_into();
    t.unwrap_or_else(|_| unreachable!("of that length"))
}

fn tables() -> &'static (RunTable, RunTable) {
    static T: OnceLock<(RunTable, RunTable)> = OnceLock::new();
    T.get_or_init(|| (build_run_table(WHITE), build_run_table(BLACK)))
}

const EOFB: u32 = 0x00_1001;

/// What seven bits mean: the code's length and, above -10, the offset of a
/// vertical mode.
const MODES: [(u8, i8); 128] = {
    let mut t = [(0u8, BAD); 128];
    let mut i = 0;
    while i < 128 {
        t[i] = match i {
            0x40.. => (1, 0),
            0x30.. => (3, 1),
            0x20.. => (3, -1),
            0x10.. => (3, HORIZONTAL),
            0x08.. => (4, PASS),
            0x06.. => (6, 2),
            0x04.. => (6, -2),
            0x03 => (7, 3),
            0x02 => (7, -3),
            0x01 => (7, EXTENSION),
            _ => (0, BAD),
        };
        i += 1;
    }
    t
};
const HORIZONTAL: i8 = -10;
const PASS: i8 = -11;
const EXTENSION: i8 = -12;
const BAD: i8 = -13;

/// Bits, zeros past the end; what was read is compared with the end a row at
/// a time.
struct Reader<'a> {
    data: &'a [u8],
    /// The next byte that is not in `buf`.
    next: usize,
    /// At least 32 bits from the top on, whenever it is looked at.
    buf: u64,
    have: u32,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        let mut r = Self {
            data,
            next: 0,
            buf: 0,
            have: 0,
        };
        r.refill();
        r
    }

    /// Position in bits.
    fn pos(&self) -> usize {
        self.next * 8 - self.have as usize
    }

    fn overrun(&self) -> bool {
        self.pos() > self.data.len() * 8
    }

    #[inline]
    fn refill(&mut self) {
        if let Some(s) = self
            .data
            .get(self.next..)
            .and_then(<[u8]>::first_chunk::<8>)
        {
            // What bits of it there are below the whole bytes taken come out
            // the same the next time.
            self.buf |= u64::from_be_bytes(*s) >> self.have;
            let n = (63 - self.have) >> 3;
            self.next += n as usize;
            self.have += n * 8;
        } else {
            self.refill_at_end();
        }
    }

    #[cold]
    #[inline(never)]
    fn refill_at_end(&mut self) {
        while self.have <= 56 {
            let b = self.data.get(self.next).map_or(0, |&b| u64::from(b));
            self.buf |= b << (56 - self.have);
            self.next += 1;
            self.have += 8;
        }
    }

    /// The next `n` (1 to 24) bits.
    #[inline]
    fn peek(&self, n: u32) -> u32 {
        (self.buf >> (64 - n)) as u32
    }

    /// At most 13 bits.
    #[inline]
    fn consume(&mut self, n: u32) {
        self.buf <<= n;
        self.have -= n;
        if self.have < 32 {
            self.refill();
        }
    }

    /// One run by the table of its colour, make-up codes summed. Runs are
    /// capped where they are used, and every code consumes bits. `None`: not a
    /// code.
    #[inline]
    fn run(&mut self, t: &RunTable) -> Option<u32> {
        let mut total = 0u32;
        loop {
            let (len, run) = t[self.peek(LOOKUP_BITS) as usize];
            if run == RUN_INVALID {
                return None;
            }
            self.consume(u32::from(len));
            if run < 64 {
                return Some(total.saturating_add(u32::from(run)));
            }
            total = total.saturating_add(u32::from(run));
        }
    }

    /// What is wrong where a code of `bits` bits was expected.
    #[cold]
    fn error(&self, bits: usize, what: &'static str) -> Error {
        if self.pos() + bits > self.data.len() * 8 {
            Error::Truncated
        } else {
            Error::Invalid(what)
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

/// Decode a row into `row`, zeroed, and its changing elements into `cur`.
/// `reference` holds the previous row's changing elements, increasing,
/// followed by three `width` sentinels. Its even entries change to black.
fn decode_row(
    r: &mut Reader<'_>,
    runs: &(RunTable, RunTable),
    width: u32,
    reference: &[u32],
    cur: &mut Vec<u32>,
    row: &mut [u8],
) -> Result<()> {
    cur.clear();
    let w = i64::from(width);
    let mut a0 = -1i64;
    // Where to look for b1: odd when black.
    let mut bi = 0usize;
    while a0 < w {
        // b1: the first change right of a0 of the opposite colour. The
        // elements increase, so it is not left of `bi`.
        let mut b1 = w;
        while let Some(&b) = reference.get(bi) {
            b1 = i64::from(b);
            if b1 > a0 {
                break;
            }
            bi += 2;
        }
        let white = bi & 1 == 0;
        let (len, mode) = MODES[r.peek(7) as usize];
        if mode > HORIZONTAL {
            r.consume(u32::from(len));
            // A damaged code can put a1 left of a0; a zero-length run then,
            // and every mode consumes bits, so a stuck a0 still terminates.
            let from = a0.max(0);
            let a1 = (b1 + i64::from(mode)).max(from).min(w);
            push_change(cur, a1 as u32, width);
            if !white {
                paint(row, from as usize, a1 as usize);
            }
            a0 = a1;
            // A step of at most three left passes at most the one element
            // before b1 (it would have to pass two more to be of use).
            let before = mode < 0 && bi > 0;
            if before && reference.get(bi - 1).is_some_and(|&b| i64::from(b) > a0) {
                bi -= 1;
            } else {
                bi += 1;
            }
            continue;
        }
        let from = a0.max(0);
        match mode {
            HORIZONTAL => {
                r.consume(3);
                let (first, second) = if white {
                    (&runs.0, &runs.1)
                } else {
                    (&runs.1, &runs.0)
                };
                let bad = |r: &Reader<'_>| r.error(13, "MMR run code");
                let Some(n) = r.run(first) else {
                    return Err(bad(r));
                };
                let a1 = (from + i64::from(n)).min(w);
                let Some(n) = r.run(second) else {
                    return Err(bad(r));
                };
                let a2 = (a1 + i64::from(n)).min(w);
                push_change(cur, a1 as u32, width);
                push_change(cur, a2 as u32, width);
                let (s, e) = if white { (a1, a2) } else { (from, a1) };
                paint(row, s as usize, e as usize);
                a0 = a2;
            }
            PASS => {
                r.consume(4);
                let b2 = reference.get(bi + 1).map_or(w, |&b| i64::from(b));
                if !white {
                    paint(row, from as usize, b2 as usize);
                }
                a0 = b2;
            }
            EXTENSION => return Err(Error::Invalid("MMR extension code")),
            _ => {
                if a0 < 0 && r.peek(24) == EOFB {
                    // The image ends here, rows short.
                    return Err(Error::Truncated);
                }
                return Err(r.error(7, "MMR mode code"));
            }
        }
    }
    // Zeros past the end are no code, so this is where they would show.
    if r.overrun() {
        return Err(Error::Truncated);
    }
    Ok(())
}

/// Set bits `s..e` of a row.
#[inline]
fn paint(row: &mut [u8], s: usize, e: usize) {
    if s >= e {
        return;
    }
    let sb = s >> 3;
    let end = (s & 7) + (e - s);
    if end <= 64 {
        if let Some(w) = row.get_mut(sb..).and_then(<[u8]>::first_chunk_mut::<8>) {
            let m = (!0u64 << (64 - (e - s))) >> (s & 7);
            *w = (u64::from_be_bytes(*w) | m).to_be_bytes();
            return;
        }
    }
    paint_bytes(row, s, e);
}

/// [`paint`], long runs and ends of rows. `s < e`.
#[inline(never)]
fn paint_bytes(row: &mut [u8], s: usize, e: usize) {
    let (sb, eb) = (s >> 3, e >> 3);
    let head = 0xFFu8 >> (s & 7);
    let tail = !(0xFFu8 >> (e & 7));
    if sb == eb {
        row[sb] |= head & tail;
        return;
    }
    row[sb] |= head;
    row[sb + 1..eb].fill(0xFF);
    if tail != 0 {
        row[eb] |= tail;
    }
}

/// Decode `w` x `h` pixels (the caller has charged the budget). Returns the
/// bitmap and the number of bytes used, EOFB included when present. Rows from
/// the first damaged one on are white if `lenient`, else an error.
pub(crate) fn decode(data: &[u8], w: u32, h: u32, lenient: bool) -> Result<(Bitmap, usize)> {
    let mut out = Bitmap::new(w, h, false);
    let mut r = Reader::new(data);
    let runs = tables();
    let mut reference = vec![w; 3];
    let mut cur = Vec::new();
    if w > 0 {
        for row in out.data.chunks_exact_mut(out.stride) {
            match decode_row(&mut r, runs, w, &reference, &mut cur, row) {
                Ok(()) => {}
                Err(_) if lenient => {
                    row.fill(0);
                    break;
                }
                Err(e) => return Err(e),
            }
            std::mem::swap(&mut reference, &mut cur);
            reference.extend_from_slice(&[w; 3]);
        }
    }
    let mut pos = r.pos();
    if r.peek(24) == EOFB && pos + 24 <= data.len() * 8 {
        pos += 24;
    }
    Ok((out, pos.div_ceil(8)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_matches_single_bits() {
        for len in [1usize, 3, 7, 8, 9, 17, 30] {
            for s in 0..len * 8 {
                for e in s..=len * 8 {
                    let mut row = vec![0x10u8; len];
                    paint(&mut row, s, e);
                    for x in 0..len * 8 {
                        let want = (s..e).contains(&x) || x & 7 == 3;
                        assert_eq!(
                            (row[x >> 3] >> (7 - (x & 7))) & 1 == 1,
                            want,
                            "{len} {s} {e}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn reader_matches_single_bits() {
        let data: Vec<u8> = (0..41u32).map(|i| (i * 73 + 11) as u8).collect();
        let bit = |i: usize| {
            data.get(i >> 3)
                .map_or(0, |b| u32::from(b >> (7 - (i & 7))) & 1)
        };
        for seed in 1..14usize {
            let mut r = Reader::new(&data);
            let (mut pos, mut k) = (0, seed);
            while pos < 44 * 8 {
                for n in [1u32, 7, 13, 24] {
                    let want = (0..n as usize).fold(0, |v, i| v << 1 | bit(pos + i));
                    assert_eq!(r.peek(n), want);
                }
                k = (k * 5 + 3) % 1009;
                let n = k % 13 + 1;
                r.consume(n as u32);
                pos += n;
                assert_eq!(r.pos(), pos);
                assert_eq!(r.overrun(), pos > data.len() * 8);
            }
        }
    }

    #[test]
    fn modes_are_the_prefix_code() {
        let codes: [(&str, i8); 10] = [
            ("1", 0),
            ("011", 1),
            ("010", -1),
            ("001", HORIZONTAL),
            ("0001", PASS),
            ("000011", 2),
            ("000010", -2),
            ("0000011", 3),
            ("0000010", -3),
            ("0000001", EXTENSION),
        ];
        for (i, &(len, mode)) in MODES.iter().enumerate() {
            let bits = format!("{i:07b}");
            match codes.iter().find(|(c, _)| bits.starts_with(c)) {
                Some(&(c, m)) => assert_eq!((usize::from(len), mode), (c.len(), m)),
                None => assert_eq!((i, mode), (0, BAD)),
            }
        }
    }
}
