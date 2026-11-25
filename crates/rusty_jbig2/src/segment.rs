//! Segment headers and the three file organisations (7.2, Annex D).

use crate::bitmap::Op;
use crate::huffman::Bits;
use crate::{Error, Result};

pub(crate) const FILE_ID: [u8; 8] = [0x97, 0x4A, 0x42, 0x32, 0x0D, 0x0A, 0x1A, 0x0A];

/// Segment types (7.3).
pub(crate) mod kind {
    pub(crate) const SYMBOL_DICT: u8 = 0;
    pub(crate) const TEXT: [u8; 3] = [4, 6, 7];
    pub(crate) const PATTERN_DICT: u8 = 16;
    pub(crate) const HALFTONE: [u8; 3] = [20, 22, 23];
    pub(crate) const GENERIC: [u8; 3] = [36, 38, 39];
    pub(crate) const REFINEMENT: [u8; 3] = [40, 42, 43];
    pub(crate) const PAGE_INFO: u8 = 48;
    pub(crate) const END_OF_PAGE: u8 = 49;
    pub(crate) const END_OF_STRIPE: u8 = 50;
    pub(crate) const END_OF_FILE: u8 = 51;
    pub(crate) const PROFILES: u8 = 52;
    pub(crate) const TABLES: u8 = 53;
    pub(crate) const EXTENSION: u8 = 62;
}

/// One segment; `data` borrows from the stream it was found in.
#[derive(Clone, Debug)]
pub(crate) struct Segment<'a> {
    pub(crate) number: u32,
    pub(crate) kind: u8,
    pub(crate) refs: Vec<u32>,
    pub(crate) page: u32,
    pub(crate) data: &'a [u8],
    /// Row count of a generic region whose length was not known in advance.
    pub(crate) rows: Option<u32>,
}

struct Header {
    number: u32,
    kind: u8,
    refs: Vec<u32>,
    page: u32,
    len: u32,
}

const UNKNOWN_LENGTH: u32 = u32::MAX;

/// Parses one segment header; returns it and the bytes used.
fn header(data: &[u8]) -> Result<(Header, usize)> {
    let mut b = Bits::new(data);
    let number = b.bits(32)?;
    let flags = b.bits(8)?;
    let short = b.bits(8)?;
    let count = match short >> 5 {
        n @ 0..=4 => n as usize,
        7 => {
            let n = (((short & 0x1F) << 24) | b.bits(24)?) as usize;
            b.take((n + 1).div_ceil(8))?;
            n
        }
        _ => return Err(Error::Invalid("referred-to segment count")),
    };
    let size = match number {
        0..=256 => 1,
        257..=65536 => 2,
        _ => 4,
    };
    let bytes = b.take(count.checked_mul(size).ok_or(Error::Truncated)?)?;
    let refs = bytes
        .chunks_exact(size)
        .map(|c| c.iter().fold(0u32, |v, &b| (v << 8) | u32::from(b)))
        .collect();
    let page = b.bits(if flags & 0x40 != 0 { 32 } else { 8 })?;
    let len = b.bits(32)?;
    let used = data.len() - b.rest().len();
    let h = Header {
        number,
        kind: (flags & 0x3F) as u8,
        refs,
        page,
        len,
    };
    Ok((h, used))
}

/// Splits off a segment's data. `rest` starts at the data part.
fn body(h: Header, rest: &[u8], lenient: bool) -> Result<(Segment<'_>, usize)> {
    let (data, used, rows) = if h.len == UNKNOWN_LENGTH {
        unknown_length(&h, rest)?
    } else {
        let n = h.len as usize;
        match rest.get(..n) {
            Some(d) => (d, n, None),
            None if lenient => (rest, rest.len(), None),
            None => return Err(Error::Truncated),
        }
    };
    let s = Segment {
        number: h.number,
        kind: h.kind,
        refs: h.refs,
        page: h.page,
        data,
        rows,
    };
    Ok((s, used))
}

/// 7.2.7: the end of a generic region of unknown length is found by its
/// marker, which is followed by a row count.
fn unknown_length<'a>(h: &Header, rest: &'a [u8]) -> Result<(&'a [u8], usize, Option<u32>)> {
    if !kind::GENERIC.contains(&h.kind) {
        return Err(Error::Invalid("unknown length on this segment type"));
    }
    let flags = *rest.get(RegionInfo::LEN).ok_or(Error::Truncated)?;
    let (marker, start) = if flags & 1 != 0 {
        ([0x00, 0x00], RegionInfo::LEN + 1)
    } else {
        // The AT flags could contain the marker by chance.
        let at = match (flags & 6, flags & 0x10) {
            (0, 0) => 8,
            (0, _) => 24,
            _ => 2,
        };
        ([0xFF, 0xAC], RegionInfo::LEN + 1 + at)
    };
    let tail = rest.get(start..).ok_or(Error::Truncated)?;
    let i = tail
        .windows(2)
        .position(|w| w == marker)
        .ok_or(Error::Truncated)?;
    let end = start + i + 2;
    let rows = rest
        .get(end..)
        .and_then(<[u8]>::first_chunk::<4>)
        .ok_or(Error::Truncated)?;
    Ok((&rest[..end], end + 4, Some(u32::from_be_bytes(*rows))))
}

/// Sequential segments (file data after the header, or an embedded stream),
/// and what stopped the parse early.
pub(crate) fn sequential(data: &[u8], lenient: bool) -> (Vec<Segment<'_>>, Option<Error>) {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let rest = &data[pos..];
        // PDF writers leave an end-of-line after the stream data.
        if rest.len() < 11 && rest.iter().all(|b| matches!(b, b'\r' | b'\n')) {
            break;
        }
        let seg = header(rest).and_then(|(h, n)| {
            let (s, m) = body(h, &rest[n..], lenient)?;
            Ok((s, n + m))
        });
        match seg {
            Ok((s, n)) => {
                let eof = s.kind == kind::END_OF_FILE;
                out.push(s);
                pos += n;
                if eof {
                    break;
                }
            }
            Err(e) => return (out, Some(e)),
        }
    }
    (out, None)
}

/// Random-access organisation: all headers, then all data parts.
pub(crate) fn random_access(data: &[u8], lenient: bool) -> (Vec<Segment<'_>>, Option<Error>) {
    let mut headers = Vec::new();
    let mut pos = 0;
    loop {
        match header(&data[pos..]) {
            Ok((h, n)) => {
                pos += n;
                let eof = h.kind == kind::END_OF_FILE;
                headers.push(h);
                if eof {
                    break;
                }
            }
            Err(e) => return (Vec::new(), Some(e)),
        }
    }
    let mut out = Vec::new();
    for h in headers {
        match body(h, &data[pos..], lenient) {
            Ok((s, n)) => {
                out.push(s);
                pos += n;
            }
            Err(e) => return (out, Some(e)),
        }
    }
    (out, None)
}

/// The region segment information field (7.4.1).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RegionInfo {
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) op: Op,
}

impl RegionInfo {
    pub(crate) const LEN: usize = 17;

    pub(crate) fn parse(b: &mut Bits<'_>) -> Result<Self> {
        let (w, h, x, y) = (b.bits(32)?, b.bits(32)?, b.bits(32)?, b.bits(32)?);
        let flags = b.bits(8)?;
        if flags & 8 != 0 {
            return Err(Error::Unsupported("colour region"));
        }
        let op = Op::from_bits(flags & 7)?;
        Ok(Self { w, h, x, y, op })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The examples of 7.2.8.
    #[test]
    fn header_examples() {
        let (h, n) = header(&[
            0x00, 0x00, 0x00, 0x20, 0x86, 0x6B, 0x02, 0x1E, 0x05, 0x04, 0, 0, 0, 9, 0xEE,
        ])
        .unwrap();
        assert_eq!((h.number, h.kind, h.page, h.len, n), (32, 6, 4, 9, 14));
        assert_eq!(h.refs, [2, 30, 5]);
        let (h, _) = header(&[
            0x00, 0x00, 0x02, 0x34, 0x40, 0xE0, 0x00, 0x00, 0x09, 0x02, 0xFD, 0x01, 0x00, 0x00,
            0x02, 0x00, 0x1E, 0x00, 0x05, 0x02, 0x00, 0x02, 0x01, 0x02, 0x02, 0x02, 0x03, 0x02,
            0x04, 0x00, 0x00, 0x04, 0x01, 0, 0, 0, 0,
        ])
        .unwrap();
        assert_eq!((h.number, h.kind, h.page), (564, 0, 1025));
        assert_eq!(h.refs, [256, 2, 30, 5, 512, 513, 514, 515, 516]);
    }
}
