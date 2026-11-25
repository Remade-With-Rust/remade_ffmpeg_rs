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
type RunTable = Vec<(u8, u16)>;

fn build_run_table(codes: &[(u16, u8, u16)]) -> RunTable {
    let mut t = vec![(0u8, RUN_INVALID); 1 << LOOKUP_BITS];
    for &(code, len, run) in codes.iter().chain(EXTENDED) {
        let shift = LOOKUP_BITS - u32::from(len);
        let base = usize::from(code) << shift;
        t[base..base + (1 << shift)].fill((len, run));
    }
    t
}

fn tables() -> &'static (RunTable, RunTable) {
    static T: OnceLock<(RunTable, RunTable)> = OnceLock::new();
    T.get_or_init(|| (build_run_table(WHITE), build_run_table(BLACK)))
}

const EOFB: u32 = 0x00_1001;

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    /// The next `n` (at most 24) bits; zeros past the end.
    #[inline]
    fn peek(&self, n: u32) -> u32 {
        let byte = self.pos >> 3;
        let w = if let Some(s) = self.data.get(byte..).and_then(<[u8]>::first_chunk::<4>) {
            u32::from_be_bytes(*s)
        } else {
            let mut w = 0u32;
            for i in 0..4 {
                w = (w << 8) | u32::from(self.data.get(byte + i).copied().unwrap_or(0));
            }
            w
        };
        (w << (self.pos & 7)) >> (32 - n)
    }

    #[inline]
    fn consume(&mut self, n: u32) -> Result<()> {
        self.pos += n as usize;
        if self.pos > self.data.len() * 8 {
            return Err(Error::Truncated);
        }
        Ok(())
    }

    /// One run of the given colour, make-up codes summed. Runs are capped
    /// where they are used, and every code consumes bits.
    fn run(&mut self, white: bool) -> Result<u32> {
        let (wt, bt) = tables();
        let t = if white { wt } else { bt };
        let mut total = 0u32;
        loop {
            let (len, run) = t[self.peek(LOOKUP_BITS) as usize];
            if run == RUN_INVALID {
                return Err(if self.pos + 13 > self.data.len() * 8 {
                    Error::Truncated
                } else {
                    Error::Invalid("MMR run code")
                });
            }
            self.consume(u32::from(len))?;
            total = total.saturating_add(u32::from(run));
            if run < 64 {
                return Ok(total);
            }
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

/// `reference` holds the previous row's changing elements followed by three
/// `width` sentinels. Its even entries change to black.
fn decode_row(r: &mut Reader<'_>, width: u32, reference: &[u32], cur: &mut Vec<u32>) -> Result<()> {
    cur.clear();
    let w = i64::from(width);
    let mut a0: i64 = -1;
    let mut white = true;
    let mut bi = 0usize;
    while a0 < w {
        // b1: the first change right of a0 of the opposite colour.
        while bi > 0 && i64::from(reference[bi - 1]) > a0 {
            bi -= 1;
        }
        let parity = usize::from(!white);
        while bi + 2 < reference.len() && (i64::from(reference[bi]) <= a0 || (bi & 1) != parity) {
            bi += 1;
        }
        let b1 = i64::from(reference[bi]);
        let b2 = i64::from(reference.get(bi + 1).copied().unwrap_or(width));

        let p = r.peek(7);
        let (len, vertical) = match p {
            0x40.. => (1, 0),
            0x30.. => (3, 1),
            0x20.. => (3, -1),
            0x10.. => {
                r.consume(3)?;
                let start = a0.max(0);
                let a1 = (start + i64::from(r.run(white)?)).min(w);
                let a2 = (a1 + i64::from(r.run(!white)?)).min(w);
                push_change(cur, a1 as u32, width);
                push_change(cur, a2 as u32, width);
                a0 = a2;
                continue;
            }
            0x08.. => {
                r.consume(4)?;
                a0 = b2;
                continue;
            }
            0x06.. => (6, 2),
            0x04.. => (6, -2),
            0x03 => (7, 3),
            0x02 => (7, -3),
            0x01 => return Err(Error::Invalid("MMR extension code")),
            _ => {
                if a0 < 0 && r.peek(24) == EOFB {
                    // The image ends here, rows short.
                    return Err(Error::Truncated);
                }
                return Err(if r.pos + 7 > r.data.len() * 8 {
                    Error::Truncated
                } else {
                    Error::Invalid("MMR mode code")
                });
            }
        };
        r.consume(len)?;
        // A damaged code can put a1 left of a0; a zero-length run then, and
        // every mode consumes bits, so a stuck a0 still terminates.
        let a1 = (b1 + vertical).max(a0.max(0)).min(w);
        push_change(cur, a1 as u32, width);
        a0 = a1;
        white = !white;
    }
    Ok(())
}

/// Set bits `s..e` of a zeroed row.
fn paint(row: &mut [u8], s: usize, e: usize) {
    if s >= e {
        return;
    }
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
    let mut r = Reader { data, pos: 0 };
    let mut reference = vec![w; 3];
    let mut cur = Vec::new();
    if w > 0 {
        for row in out.data.chunks_exact_mut(out.stride) {
            match decode_row(&mut r, w, &reference, &mut cur) {
                Ok(()) => {}
                Err(_) if lenient => break,
                Err(e) => return Err(e),
            }
            for pair in cur.chunks(2) {
                paint(
                    row,
                    pair[0] as usize,
                    pair.get(1).map_or(w, |&e| e) as usize,
                );
            }
            std::mem::swap(&mut reference, &mut cur);
            reference.extend_from_slice(&[w; 3]);
        }
    }
    if r.peek(24) == EOFB && r.pos + 24 <= data.len() * 8 {
        r.pos += 24;
    }
    Ok((out, r.pos.div_ceil(8)))
}
