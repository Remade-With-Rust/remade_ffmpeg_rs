//! Bit reading and Annex B Huffman tables.

use crate::{Error, Result};

/// MSB-first bit reader; reading past the end is [`Error::Truncated`].
pub(crate) struct Bits<'a> {
    data: &'a [u8],
    /// Position in bits.
    pos: usize,
}

impl<'a> Bits<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub(crate) fn bit(&mut self) -> Result<u32> {
        let b = self.data.get(self.pos >> 3).ok_or(Error::Truncated)?;
        let v = (b >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Ok(u32::from(v))
    }

    /// `n` is at most 32.
    pub(crate) fn bits(&mut self, n: u32) -> Result<u32> {
        let end = self.pos + n as usize;
        if end > self.data.len() * 8 {
            return Err(Error::Truncated);
        }
        let mut v = 0u64;
        let mut byte = self.pos >> 3;
        let last = end.div_ceil(8);
        while byte < last {
            v = (v << 8) | u64::from(self.data[byte]);
            byte += 1;
        }
        self.pos = end;
        Ok(((v >> (last * 8 - end)) & ((1u64 << n) - 1)) as u32)
    }

    pub(crate) fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// Align, then take `n` bytes.
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        self.align();
        let s = self.pos >> 3;
        let e = s.checked_add(n).ok_or(Error::Truncated)?;
        let out = self.data.get(s..e).ok_or(Error::Truncated)?;
        self.pos = e * 8;
        Ok(out)
    }

    /// Align, then everything that is left.
    pub(crate) fn rest(&mut self) -> &'a [u8] {
        self.align();
        self.data.get(self.pos >> 3..).unwrap_or(&[])
    }
}

/// What a table line decodes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Line {
    /// `low + offset`, with this many offset bits.
    Up(i32, u8),
    /// `low - offset` (the lower range line), 32 offset bits.
    Down(i32),
    Oob,
}

/// A prefix code: symbol indices in canonical order plus the number of codes
/// of each length (B.3 assigns codes in exactly this order).
#[derive(Clone, Debug)]
pub(crate) struct Prefix {
    count: Vec<u32>,
    sorted: Vec<u32>,
}

impl Prefix {
    /// Codes for `lens` (0 = unused) by B.3.
    pub(crate) fn new(lens: &[u8]) -> Self {
        let max = usize::from(lens.iter().copied().max().unwrap_or(0));
        let mut count = vec![0u32; max + 1];
        for &l in lens {
            count[usize::from(l)] += 1;
        }
        count[0] = 0;
        let mut at = vec![0u32; max + 1];
        for l in 1..=max {
            at[l] = at[l - 1] + count[l - 1];
        }
        let mut sorted = vec![0u32; lens.iter().filter(|&&l| l != 0).count()];
        for (i, &l) in lens.iter().enumerate() {
            if l != 0 {
                sorted[at[usize::from(l)] as usize] = i as u32;
                at[usize::from(l)] += 1;
            }
        }
        Self { count, sorted }
    }

    /// Reads one code; the index of the symbol it was assigned to.
    pub(crate) fn read(&self, b: &mut Bits<'_>) -> Result<usize> {
        // `diff` is the code read so far minus the first code of its length.
        let mut diff = 0u64;
        let mut index = 0u64;
        for &c in self.count.get(1..).unwrap_or(&[]) {
            diff = (diff << 1) | u64::from(b.bit()?);
            let c = u64::from(c);
            if diff < c {
                return Ok(self.sorted[(index + diff) as usize] as usize);
            }
            diff -= c;
            index += c;
            // No later length can have this many codes.
            if diff > 1 << 40 {
                break;
            }
        }
        Err(Error::Invalid("unassigned Huffman code"))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Table {
    lines: Vec<Line>,
    prefix: Prefix,
}

impl Table {
    fn build(lines: Vec<Line>, lens: &[u8]) -> Self {
        Self {
            prefix: Prefix::new(lens),
            lines,
        }
    }

    /// Parse a tables segment's data (B.2).
    pub(crate) fn parse(data: &[u8]) -> Result<Self> {
        let mut b = Bits::new(data);
        let flags = b.bits(8)?;
        if flags & 0x80 != 0 {
            return Err(Error::Invalid("reserved code table flag"));
        }
        let oob = flags & 1 != 0;
        let ps = ((flags >> 1) & 7) + 1;
        let rs = ((flags >> 4) & 7) + 1;
        let low = b.bits(32)? as i32;
        let high = b.bits(32)? as i32;
        let mut lines = Vec::new();
        let mut lens = Vec::new();
        let mut cur = i64::from(low);
        // Each line consumes at least two bits, so this is bounded by `data`.
        loop {
            lens.push(b.bits(ps)? as u8);
            let r = b.bits(rs)?;
            if r > 32 {
                return Err(Error::Invalid("code table range length above 32"));
            }
            let l = i32::try_from(cur).map_err(|_| Error::Invalid("code table range overflow"))?;
            lines.push(Line::Up(l, r as u8));
            cur += 1i64 << r;
            if cur >= i64::from(high) {
                break;
            }
        }
        lens.push(b.bits(ps)? as u8);
        // HTLOW - 1 below the i32 range can only be reached by an unused line.
        lines.push(Line::Down(low.wrapping_sub(1)));
        lens.push(b.bits(ps)? as u8);
        lines.push(Line::Up(high, 32));
        if oob {
            lens.push(b.bits(ps)? as u8);
            lines.push(Line::Oob);
        }
        Ok(Self::build(lines, &lens))
    }

    /// B.4; `None` is OOB.
    pub(crate) fn read(&self, b: &mut Bits<'_>) -> Result<Option<i64>> {
        Ok(match self.lines[self.prefix.read(b)?] {
            Line::Up(low, n) => Some(i64::from(low) + i64::from(b.bits(u32::from(n))?)),
            Line::Down(low) => Some(i64::from(low) - i64::from(b.bits(32)?)),
            Line::Oob => None,
        })
    }

    /// [`Table::read`] where OOB is not allowed.
    pub(crate) fn read_some(&self, b: &mut Bits<'_>) -> Result<i64> {
        self.read(b)?
            .ok_or(Error::Invalid("unexpected out-of-band Huffman value"))
    }

    /// Table B.`n` (1 to 15).
    pub(crate) fn standard(n: usize) -> Self {
        let mut lines = Vec::new();
        let mut lens = Vec::new();
        for &(len, l) in STANDARD[n - 1] {
            lens.push(len);
            lines.push(l);
        }
        Self::build(lines, &lens)
    }
}

const fn u(len: u8, low: i32, range: u8) -> (u8, Line) {
    (len, Line::Up(low, range))
}
const fn d(len: u8, low: i32) -> (u8, Line) {
    (len, Line::Down(low))
}
const fn o(len: u8) -> (u8, Line) {
    (len, Line::Oob)
}

/// B.5, in the tables' own line order (B.3 breaks ties by it).
#[rustfmt::skip]
const STANDARD: [&[(u8, Line)]; 15] = [
    // B.1
    &[u(1, 0, 4), u(2, 16, 8), u(3, 272, 16), u(3, 65808, 32)],
    // B.2
    &[u(1, 0, 0), u(2, 1, 0), u(3, 2, 0), u(4, 3, 3), u(5, 11, 6), u(6, 75, 32), o(6)],
    // B.3
    &[u(8, -256, 8), u(1, 0, 0), u(2, 1, 0), u(3, 2, 0), u(4, 3, 3), u(5, 11, 6), d(8, -257),
      u(7, 75, 32), o(6)],
    // B.4
    &[u(1, 1, 0), u(2, 2, 0), u(3, 3, 0), u(4, 4, 3), u(5, 12, 6), u(5, 76, 32)],
    // B.5
    &[u(7, -255, 8), u(1, 1, 0), u(2, 2, 0), u(3, 3, 0), u(4, 4, 3), u(5, 12, 6), d(7, -256),
      u(6, 76, 32)],
    // B.6
    &[u(5, -2048, 10), u(4, -1024, 9), u(4, -512, 8), u(4, -256, 7), u(5, -128, 6), u(5, -64, 5),
      u(4, -32, 5), u(2, 0, 7), u(3, 128, 7), u(3, 256, 8), u(4, 512, 9), u(4, 1024, 10),
      d(6, -2049), u(6, 2048, 32)],
    // B.7
    &[u(4, -1024, 9), u(3, -512, 8), u(4, -256, 7), u(5, -128, 6), u(5, -64, 5), u(4, -32, 5),
      u(4, 0, 5), u(5, 32, 5), u(5, 64, 6), u(4, 128, 7), u(3, 256, 8), u(3, 512, 9),
      u(3, 1024, 10), d(5, -1025), u(5, 2048, 32)],
    // B.8
    &[u(8, -15, 3), u(9, -7, 1), u(8, -5, 1), u(9, -3, 0), u(7, -2, 0), u(4, -1, 0), u(2, 0, 1),
      u(5, 2, 0), u(6, 3, 0), u(3, 4, 4), u(6, 20, 1), u(4, 22, 4), u(4, 38, 5), u(5, 70, 6),
      u(5, 134, 7), u(6, 262, 7), u(7, 390, 8), u(6, 646, 10), d(9, -16), u(9, 1670, 32), o(2)],
    // B.9
    &[u(8, -31, 4), u(9, -15, 2), u(8, -11, 2), u(9, -7, 1), u(7, -5, 1), u(4, -3, 1), u(3, -1, 1),
      u(3, 1, 1), u(5, 3, 1), u(6, 5, 1), u(3, 7, 5), u(6, 39, 2), u(4, 43, 5), u(4, 75, 6),
      u(5, 139, 7), u(5, 267, 8), u(6, 523, 8), u(7, 779, 9), u(6, 1291, 11), d(9, -32),
      u(9, 3339, 32), o(2)],
    // B.10
    &[u(7, -21, 4), u(8, -5, 0), u(7, -4, 0), u(5, -3, 0), u(2, -2, 2), u(5, 2, 0), u(6, 3, 0),
      u(7, 4, 0), u(8, 5, 0), u(2, 6, 6), u(5, 70, 5), u(6, 102, 5), u(6, 134, 6), u(6, 198, 7),
      u(6, 326, 8), u(6, 582, 9), u(6, 1094, 10), u(7, 2118, 11), d(8, -22), u(8, 4166, 32), o(2)],
    // B.11
    &[u(1, 1, 0), u(2, 2, 1), u(4, 4, 0), u(4, 5, 1), u(5, 7, 1), u(5, 9, 2), u(6, 13, 2),
      u(7, 17, 2), u(7, 21, 3), u(7, 29, 4), u(7, 45, 5), u(7, 77, 6), u(7, 141, 32)],
    // B.12
    &[u(1, 1, 0), u(2, 2, 0), u(3, 3, 1), u(5, 5, 0), u(5, 6, 1), u(6, 8, 1), u(7, 10, 0),
      u(7, 11, 1), u(7, 13, 2), u(7, 17, 3), u(7, 25, 4), u(8, 41, 5), u(8, 73, 32)],
    // B.13
    &[u(1, 1, 0), u(3, 2, 0), u(4, 3, 0), u(5, 4, 0), u(4, 5, 1), u(3, 7, 3), u(6, 15, 1),
      u(6, 17, 2), u(6, 21, 3), u(6, 29, 4), u(6, 45, 5), u(7, 77, 6), u(7, 141, 32)],
    // B.14
    &[u(3, -2, 0), u(3, -1, 0), u(1, 0, 0), u(3, 1, 0), u(3, 2, 0)],
    // B.15
    &[u(7, -24, 4), u(6, -8, 2), u(5, -4, 1), u(4, -2, 0), u(3, -1, 0), u(1, 0, 0), u(3, 1, 0),
      u(4, 2, 0), u(5, 3, 1), u(6, 5, 2), u(7, 9, 4), d(7, -25), u(7, 25, 32)],
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every standard table is a complete prefix code and its lines cover
    /// consecutive ranges: a transcription slip breaks one or the other.
    #[test]
    fn standard_tables_are_complete_and_contiguous() {
        for (i, t) in STANDARD.iter().enumerate() {
            let kraft: u64 = t.iter().map(|(l, _)| 1u64 << (32 - u32::from(*l))).sum();
            assert_eq!(kraft, 1 << 32, "B.{}", i + 1);
            let mut next: Option<i64> = None;
            let mut first = None;
            for (_, l) in *t {
                match *l {
                    Line::Up(low, n) => {
                        first.get_or_insert(low);
                        assert_eq!(
                            next.unwrap_or(i64::from(low)),
                            i64::from(low),
                            "B.{}",
                            i + 1
                        );
                        next = Some(i64::from(low) + (1i64 << n));
                    }
                    Line::Down(low) => assert_eq!(first, Some(low + 1), "B.{}", i + 1),
                    Line::Oob => {}
                }
            }
        }
    }

    /// Codes printed in B.5, spot-checked at the awkward ties.
    #[test]
    fn standard_table_codes() {
        let read = |n: usize, code: &str| {
            let mut bytes = vec![0u8; 12];
            for (i, c) in code.bytes().enumerate() {
                bytes[i / 8] |= (c - b'0') << (7 - i % 8);
            }
            Table::standard(n).read(&mut Bits::new(&bytes)).unwrap()
        };
        assert_eq!(read(1, "110"), Some(272));
        assert_eq!(read(2, "111111"), None);
        assert_eq!(read(3, "11111110"), Some(-256));
        assert_eq!(read(3, "11111111"), Some(-257));
        assert_eq!(read(5, "1111111"), Some(-256));
        assert_eq!(read(6, "111110"), Some(-2049));
        assert_eq!(read(7, "000"), Some(-512));
        assert_eq!(read(8, "111111100"), Some(-7));
        assert_eq!(read(8, "01"), None);
        assert_eq!(read(9, "00"), None);
        assert_eq!(read(10, "10"), None);
        assert_eq!(read(10, "1111100"), Some(4));
        assert_eq!(read(11, "1111111"), Some(141));
        assert_eq!(read(12, "11111110"), Some(41));
        assert_eq!(read(13, "101"), Some(7));
        assert_eq!(read(14, "100"), Some(-2));
        assert_eq!(read(15, "1111110"), Some(-25));
    }

    /// The B.4 example: Table B.1 transmitted as a custom table.
    #[test]
    fn annex_b_example() {
        let t = Table::parse(&[0x42, 0, 0, 0, 0, 0, 1, 1, 0x10, 0x49, 0x23, 0x81, 0x80]).unwrap();
        let s = Table::standard(1);
        for seed in 0..500u32 {
            let bytes = (seed.wrapping_mul(0x9E37_79B9)).to_be_bytes();
            let mut data = bytes.to_vec();
            data.extend_from_slice(&[0x5A; 8]);
            assert_eq!(
                t.read(&mut Bits::new(&data)).unwrap(),
                s.read(&mut Bits::new(&data)).unwrap()
            );
        }
    }

    #[test]
    fn bits_reader() {
        let mut b = Bits::new(&[0xA5, 0xC3, 0xFF, 0x00, 0x12, 0x34]);
        assert_eq!(b.bits(3).unwrap(), 0b101);
        assert_eq!(b.bits(32).unwrap(), 0x2E1F_F800);
        assert_eq!(b.bits(0).unwrap(), 0);
        assert_eq!(b.take(1).unwrap(), [0x34]);
        assert_eq!(b.bit(), Err(Error::Truncated));
    }
}
