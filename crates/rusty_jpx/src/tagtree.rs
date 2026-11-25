//! Tag trees (B.10.2) and the packet header bit reader (B.10.1).

use crate::error::{JpxError, JpxResult};

/// Packet header bits: MSB first, a zero bit stuffed after each `FF`.
pub(crate) struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    byte: u8,
    left: u32,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8], pos: usize) -> Self {
        Bits {
            data,
            pos,
            byte: 0,
            left: 0,
        }
    }

    #[inline]
    pub fn bit(&mut self) -> JpxResult<u32> {
        if self.left == 0 {
            let stuffed = self.byte == 0xFF;
            self.byte = *self.data.get(self.pos).ok_or(JpxError::Truncated)?;
            self.pos += 1;
            self.left = if stuffed { 7 } else { 8 };
        }
        self.left -= 1;
        Ok(u32::from(self.byte >> self.left) & 1)
    }

    pub fn bits(&mut self, n: u32) -> JpxResult<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    /// Byte position after the header, past the byte stuffed after a final
    /// `FF`.
    pub fn finish(self) -> JpxResult<usize> {
        if self.byte == 0xFF {
            if self.pos >= self.data.len() {
                return Err(JpxError::Truncated);
            }
            return Ok(self.pos + 1);
        }
        Ok(self.pos)
    }
}

#[derive(Clone, Copy, Default)]
struct Node {
    /// Lower bound of the value so far.
    low: u16,
    known: bool,
}

/// A tag tree over a `w` x `h` grid.
pub(crate) struct TagTree {
    nodes: Vec<Node>,
    /// (offset into `nodes`, width) per level, leaves first.
    levels: Vec<(u32, u32)>,
}

impl TagTree {
    pub fn new(w: u32, h: u32) -> Self {
        let mut levels = Vec::new();
        let (mut w, mut h, mut total) = (w, h, 0);
        if w > 0 && h > 0 {
            loop {
                levels.push((total, w));
                total += w * h;
                if w == 1 && h == 1 {
                    break;
                }
                w = w.div_ceil(2);
                h = h.div_ceil(2);
            }
        }
        TagTree {
            nodes: vec![Node::default(); total as usize],
            levels,
        }
    }

    /// Read bits until it is known whether leaf (`x`, `y`)'s value is below
    /// `threshold`; returns the value when it is.
    pub fn decode(
        &mut self,
        bits: &mut Bits,
        x: u32,
        y: u32,
        threshold: u16,
    ) -> JpxResult<Option<u16>> {
        let mut low = 0;
        for (l, &(offset, width)) in self.levels.iter().enumerate().rev() {
            let node = &mut self.nodes[(offset + (y >> l) * width + (x >> l)) as usize];
            low = low.max(node.low);
            while !node.known && low < threshold {
                if bits.bit()? != 0 {
                    node.known = true;
                } else {
                    low += 1;
                }
            }
            node.low = low;
            if !node.known {
                return Ok(None);
            }
        }
        Ok((low < threshold).then_some(low))
    }
}
