use crate::decoder::DecodingError;

use super::vp8::TreeNode;

#[must_use]
#[repr(transparent)]
pub(crate) struct BitResult<T> {
    value_if_not_past_eof: T,
}

#[must_use]
pub(crate) struct BitResultAccumulator;

impl<T> BitResult<T> {
    const fn ok(value: T) -> Self {
        Self {
            value_if_not_past_eof: value,
        }
    }

    /// Instead of checking this result now, accumulate the burden of checking
    /// into an accumulator. This accumulator must be checked in the end.
    #[inline(always)]
    pub(crate) fn or_accumulate(self, acc: &mut BitResultAccumulator) -> T {
        let _ = acc;
        self.value_if_not_past_eof
    }
}

impl<T: Default> BitResult<T> {
    fn err() -> Self {
        Self {
            value_if_not_past_eof: T::default(),
        }
    }
}

#[cfg_attr(test, derive(Debug))]
pub(crate) struct ArithmeticDecoder {
    chunks: Box<[[u8; 4]]>,
    state: State,
    final_bytes: [u8; 3],
    final_bytes_remaining: i8,
}

#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy)]
struct State {
    chunk_index: usize,
    value: u64,
    /// The coder's range minus one (127..=254 once normalised), the form
    /// libwebp keeps: the split then needs no +1/-1 round trip per bit.
    range_m1: u32,
    bit_count: i32,
}

#[cfg_attr(test, derive(Debug))]
struct FastDecoder<'a> {
    chunks: &'a [[u8; 4]],
    uncommitted_state: State,
    save_state: &'a mut State,
}

impl ArithmeticDecoder {
    pub(crate) fn new() -> ArithmeticDecoder {
        let state = State {
            chunk_index: 0,
            value: 0,
            range_m1: 254,
            bit_count: -8,
        };
        ArithmeticDecoder {
            chunks: Box::new([]),
            state,
            final_bytes: [0; 3],
            final_bytes_remaining: Self::FINAL_BYTES_REMAINING_EOF,
        }
    }

    pub(crate) fn init(&mut self, mut buf: Vec<[u8; 4]>, len: usize) -> Result<(), DecodingError> {
        let mut final_bytes = [0; 3];
        let final_bytes_remaining = if len == 4 * buf.len() {
            0
        } else {
            // Pop the last chunk (which is partial), then get length.
            let Some(last_chunk) = buf.pop() else {
                return Err(DecodingError::NotEnoughInitData);
            };
            let len_rounded_down = 4 * buf.len();
            let num_bytes_popped = len - len_rounded_down;
            debug_assert!(num_bytes_popped <= 3);
            final_bytes[..num_bytes_popped].copy_from_slice(&last_chunk[..num_bytes_popped]);
            for i in num_bytes_popped..4 {
                debug_assert_eq!(last_chunk[i], 0, "unexpected {last_chunk:?}");
            }
            num_bytes_popped as i8
        };

        let chunks = buf.into_boxed_slice();
        let state = State {
            chunk_index: 0,
            value: 0,
            range_m1: 254,
            bit_count: -8,
        };
        *self = Self {
            chunks,
            state,
            final_bytes,
            final_bytes_remaining,
        };
        Ok(())
    }

    /// Start a span of reading operations from the buffer, without stopping
    /// when the buffer runs out. For all valid webp images, the buffer will not
    /// run out prematurely. Conversely if the buffer ends early, the webp image
    /// cannot be correctly decoded and any intermediate results need to be
    /// discarded anyway.
    ///
    /// Each call to `start_accumulated_result` must be followed by a call to
    /// `check` on the *same* `ArithmeticDecoder`.
    #[inline(always)]
    pub(crate) fn start_accumulated_result(&mut self) -> BitResultAccumulator {
        BitResultAccumulator
    }

    /// Check that the read operations done so far were all valid.
    #[inline(always)]
    pub(crate) fn check<T>(
        &self,
        acc: BitResultAccumulator,
        value_if_not_past_eof: T,
    ) -> Result<T, DecodingError> {
        // The accumulator does not store any state because doing so is
        // too computationally expensive. Passing it around is a bit of
        // formality (that is optimized out) to ensure we call `check` .
        // Instead we check whether we have read past the end of the file.
        let BitResultAccumulator = acc;

        if self.is_past_eof() {
            Err(DecodingError::BitStreamError)
        } else {
            Ok(value_if_not_past_eof)
        }
    }

    fn keep_accumulating<T>(
        &self,
        acc: BitResultAccumulator,
        value_if_not_past_eof: T,
    ) -> BitResult<T> {
        // The BitResult will be checked later by a different accumulator.
        // Because it does not carry state, that is fine.
        let BitResultAccumulator = acc;

        BitResult::ok(value_if_not_past_eof)
    }

    // Do not inline this because inlining seems to worsen performance.
    // (Re-measured 2026-08-12 on the 8K lossy pinned A/B: inlining reads flat,
    // inside the noise floor — upstream's choice stands.)
    #[inline(never)]
    pub(crate) fn read_bool(&mut self, probability: u8) -> BitResult<bool> {
        if let Some(b) = self.fast().read_bool(probability) {
            return BitResult::ok(b);
        }

        self.cold_read_bool(probability)
    }

    // Do not inline this because inlining seems to worsen performance.
    #[inline(never)]
    pub(crate) fn read_flag(&mut self) -> BitResult<bool> {
        if let Some(b) = self.fast().read_flag() {
            return BitResult::ok(b);
        }

        self.cold_read_flag()
    }

    // Do not inline this because inlining seems to worsen performance.
    #[inline(never)]
    pub(crate) fn read_literal(&mut self, n: u8) -> BitResult<u8> {
        if let Some(v) = self.fast().read_literal(n) {
            return BitResult::ok(v);
        }

        self.cold_read_literal(n)
    }

    // Do not inline this because inlining seems to worsen performance.
    #[inline(never)]
    pub(crate) fn read_optional_signed_value(&mut self, n: u8) -> BitResult<i32> {
        if let Some(v) = self.fast().read_optional_signed_value(n) {
            return BitResult::ok(v);
        }

        self.cold_read_optional_signed_value(n)
    }

    // This is generic and inlined just to skip the first bounds check.
    #[inline]
    pub(crate) fn read_with_tree<const N: usize>(&mut self, tree: &[TreeNode; N]) -> BitResult<i8> {
        let first_node = tree[0];
        self.read_with_tree_with_first_node(tree, first_node)
    }

    // Do not inline this because inlining significantly worsens performance.
    #[inline(never)]
    pub(crate) fn read_with_tree_with_first_node(
        &mut self,
        tree: &[TreeNode],
        first_node: TreeNode,
    ) -> BitResult<i8> {
        if let Some(v) = self.fast().read_with_tree(tree, first_node) {
            return BitResult::ok(v);
        }

        self.cold_read_with_tree(tree, usize::from(first_node.index))
    }

    // As a similar (but different) speedup to BitResult, the FastDecoder reads
    // bits under an assumption and validates it at the end.
    //
    // The idea here is that for normal-sized webp images, the vast majority
    // of bits are somewhere other than in the last four bytes. Therefore we
    // can pretend the buffer has infinite size. After we are done reading,
    // we check if we actually read past the end of `self.chunks`.
    // If so, we backtrack (or rather we discard `uncommitted_state`)
    // and try again with the slow approach. This might result in doing double
    // work for those last few bytes -- in fact we even keep retrying the fast
    // method to save an if-statement --, but more than make up for that by
    // speeding up reading from the other thousands or millions of bytes.
    fn fast(&mut self) -> FastDecoder<'_> {
        FastDecoder {
            chunks: &self.chunks,
            uncommitted_state: self.state,
            save_state: &mut self.state,
        }
    }

    const FINAL_BYTES_REMAINING_EOF: i8 = -0xE;

    fn load_from_final_bytes(&mut self) {
        match self.final_bytes_remaining {
            1.. => {
                self.final_bytes_remaining -= 1;
                let byte = self.final_bytes[0];
                self.final_bytes.rotate_left(1);
                self.state.value <<= 8;
                self.state.value |= u64::from(byte);
                self.state.bit_count += 8;
            }
            0 => {
                // libwebp seems to (sometimes?) allow bitstreams that read one byte past the end.
                // This replicates that logic.
                self.final_bytes_remaining -= 1;
                self.state.value <<= 8;
                self.state.bit_count += 8;
            }
            _ => {
                self.final_bytes_remaining = Self::FINAL_BYTES_REMAINING_EOF;
            }
        }
    }

    fn is_past_eof(&self) -> bool {
        self.final_bytes_remaining == Self::FINAL_BYTES_REMAINING_EOF
    }

    fn cold_read_bit(&mut self, probability: u8) -> BitResult<bool> {
        if self.state.bit_count < 0 {
            if let Some(chunk) = self.chunks.get(self.state.chunk_index).copied() {
                let v = u32::from_be_bytes(chunk);
                self.state.chunk_index += 1;
                self.state.value <<= 32;
                self.state.value |= u64::from(v);
                self.state.bit_count += 32;
            } else {
                self.load_from_final_bytes();
                if self.is_past_eof() {
                    return BitResult::err();
                }
            }
        }
        debug_assert!(self.state.bit_count >= 0);

        let probability = u32::from(probability);
        let mut range = self.state.range_m1 + 1;
        let split = 1 + (((range - 1) * probability) >> 8);
        let bigsplit = u64::from(split) << self.state.bit_count;

        let retval = if let Some(new_value) = self.state.value.checked_sub(bigsplit) {
            range -= split;
            self.state.value = new_value;
            true
        } else {
            range = split;
            false
        };
        debug_assert!(range > 0);

        // Compute shift required to satisfy `range >= 128`.
        // Apply that shift to `range` and `self.state.bitcount`.
        //
        // Subtract 24 because we only care about leading zeros in the
        // lowest byte of `range` which is a `u32`.
        let shift = range.leading_zeros().saturating_sub(24);
        range <<= shift;
        self.state.bit_count -= shift as i32;
        debug_assert!(range >= 128);
        self.state.range_m1 = range - 1;

        BitResult::ok(retval)
    }

    #[cold]
    #[inline(never)]
    fn cold_read_bool(&mut self, probability: u8) -> BitResult<bool> {
        self.cold_read_bit(probability)
    }

    #[cold]
    #[inline(never)]
    fn cold_read_flag(&mut self) -> BitResult<bool> {
        self.cold_read_bit(128)
    }

    #[cold]
    #[inline(never)]
    fn cold_read_literal(&mut self, n: u8) -> BitResult<u8> {
        let mut v = 0u8;
        let mut res = self.start_accumulated_result();

        for _ in 0..n {
            let b = self.cold_read_flag().or_accumulate(&mut res);
            v = (v << 1) + u8::from(b);
        }

        self.keep_accumulating(res, v)
    }

    #[cold]
    #[inline(never)]
    fn cold_read_optional_signed_value(&mut self, n: u8) -> BitResult<i32> {
        let mut res = self.start_accumulated_result();
        let flag = self.cold_read_flag().or_accumulate(&mut res);
        if !flag {
            // We should not read further bits if the flag is not set.
            return self.keep_accumulating(res, 0);
        }
        let magnitude = self.cold_read_literal(n).or_accumulate(&mut res);
        let sign = self.cold_read_flag().or_accumulate(&mut res);

        let value = if sign {
            -i32::from(magnitude)
        } else {
            i32::from(magnitude)
        };
        self.keep_accumulating(res, value)
    }

    #[cold]
    #[inline(never)]
    fn cold_read_with_tree(&mut self, tree: &[TreeNode], start: usize) -> BitResult<i8> {
        let mut index = start;
        let mut res = self.start_accumulated_result();

        loop {
            let node = tree[index];
            let prob = node.prob;
            let b = self.cold_read_bit(prob).or_accumulate(&mut res);
            let t = if b { node.right } else { node.left };
            let new_index = usize::from(t);
            if new_index < tree.len() {
                index = new_index;
            } else {
                let value = TreeNode::value_from_branch(t);
                return self.keep_accumulating(res, value);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared fast-path bit cores: run on a caller-held `State` snapshot with
// zero-fill past the buffer end. Validity (did we run past the real data?) is
// checked when the snapshot is committed. Used by both the per-read
// `FastDecoder` and the block-scoped `FastSession`.
// ---------------------------------------------------------------------------

/// Renormalisation, indexed by the un-normalised `range - 1` (0..=254; 255
/// is never produced): the left shift that brings the range back to
/// 128..=255 (`leading_zeros(range) - 24`), and the shifted range, again
/// minus one. The range never exceeds 255 (it starts there and only shrinks
/// before each renormalisation), so two table loads replace a bit scan, its
/// zero case, a saturating subtract and a variable shift.
const NORM: ([u8; 256], [u8; 256]) = {
    let mut shift = [0u8; 256];
    let mut range_m1 = [255u8; 256];
    let mut i = 0;
    while i < 255 {
        let range = i as u32 + 1;
        let s = (range as u8).leading_zeros();
        shift[i] = s as u8;
        range_m1[i] = ((range << s) - 1) as u8;
        i += 1;
    }
    (shift, range_m1)
};

/// Store the un-normalised `range - 1` back, normalised.
#[inline(always)]
fn renormalise(state: &mut State, range_m1: u32) {
    debug_assert!(range_m1 < 255);
    let i = range_m1 as u8 as usize;
    state.bit_count -= i32::from(NORM.0[i]);
    state.range_m1 = u32::from(NORM.1[i]);
    debug_assert!(state.range_m1 >= 127);
}

#[inline(always)]
fn state_read_bit(chunks: &[[u8; 4]], state: &mut State, probability: u8) -> bool {
    if state.bit_count < 0 {
        let chunk = chunks.get(state.chunk_index).copied().unwrap_or_default();
        let v = u32::from_be_bytes(chunk);
        state.chunk_index += 1;
        state.value <<= 32;
        state.value |= u64::from(v);
        state.bit_count += 32;
    }
    debug_assert!(state.bit_count >= 0);

    // `split - 1`, with split = 1 + (((range - 1) * probability) >> 8)
    let split_m1 = (state.range_m1 * u32::from(probability)) >> 8;
    let bigsplit = u64::from(split_m1 + 1) << state.bit_count;

    if let Some(new_value) = state.value.checked_sub(bigsplit) {
        state.value = new_value;
        // range - split, minus one
        renormalise(state, state.range_m1 - split_m1 - 1);
        true
    } else {
        // range = split
        renormalise(state, split_m1);
        false
    }
}

#[inline(always)]
fn state_read_flag(chunks: &[[u8; 4]], state: &mut State) -> bool {
    if state.bit_count < 0 {
        let chunk = chunks.get(state.chunk_index).copied().unwrap_or_default();
        let v = u32::from_be_bytes(chunk);
        state.chunk_index += 1;
        state.value <<= 32;
        state.value |= u64::from(v);
        state.bit_count += 32;
    }
    debug_assert!(state.bit_count >= 0);

    let half_range = (state.range_m1 + 1) / 2;
    let split = state.range_m1 + 1 - half_range;
    let bigsplit = u64::from(split) << state.bit_count;

    if let Some(new_value) = state.value.checked_sub(bigsplit) {
        state.value = new_value;
        renormalise(state, half_range - 1);
        true
    } else {
        renormalise(state, split - 1);
        false
    }
}

/// Read an even-odds flag and apply it as a sign: `-v` when the flag is set.
///
/// The same decode as [`state_read_flag`], without its branch or table
/// loads (libwebp's `VP8GetSigned`): halving a normalised range always
/// renormalises by exactly one bit, so the new `range - 1` is the old one,
/// minus one when the flag is set, with its low bit forced; and the flag
/// itself is a 0 / -1 mask that conditionally subtracts the split and
/// negates `v`. Sign bits are close to random, so the branch was one the
/// predictor could not learn.
#[inline(always)]
fn state_read_signed(chunks: &[[u8; 4]], state: &mut State, v: i32) -> i32 {
    // A range of exactly 255 (only the state before the first read) halves
    // to 128, which needs no shift: leave that one case to the general read.
    if state.range_m1 == 254 {
        return if state_read_flag(chunks, state) {
            -v
        } else {
            v
        };
    }
    if state.bit_count < 0 {
        let chunk = chunks.get(state.chunk_index).copied().unwrap_or_default();
        let v = u32::from_be_bytes(chunk);
        state.chunk_index += 1;
        state.value <<= 32;
        state.value |= u64::from(v);
        state.bit_count += 32;
    }
    debug_assert!(state.bit_count >= 0);

    // `split - 1`, with split = range - range / 2
    let split_m1 = state.range_m1 >> 1;
    let window = (state.value >> state.bit_count) as u32;
    // -1 when the flag is set (window >= split), else 0
    let mask = (split_m1 as i32 - window as i32) >> 31;
    state.value -= u64::from((split_m1 + 1) & mask as u32) << state.bit_count;
    state.range_m1 = state.range_m1.wrapping_add(mask as u32) | 1;
    state.bit_count -= 1;
    (v ^ mask) - mask
}

/// A block-scoped fast-reading session: the decoder state is snapshotted
/// once, every read runs on the snapshot, and the whole block commits with
/// one validity check — instead of a state copy in and out per read. If the
/// session ran past the real buffer the commit fails, the decoder state is
/// unchanged, and the caller redoes the block through the committed per-read
/// API (whose cold path handles the file tail correctly).
pub(crate) struct FastSession {
    state: State,
}

impl ArithmeticDecoder {
    #[inline(always)]
    pub(crate) fn begin_session(&self) -> FastSession {
        FastSession { state: self.state }
    }

    #[inline(always)]
    pub(crate) fn commit_session(&mut self, s: FastSession) -> bool {
        if s.state.chunk_index <= self.chunks.len() {
            self.state = s.state;
            true
        } else {
            false
        }
    }

    #[inline(always)]
    pub(crate) fn session_read_bool(&self, s: &mut FastSession, probability: u8) -> bool {
        state_read_bit(&self.chunks, &mut s.state, probability)
    }

    #[inline(always)]
    pub(crate) fn session_read_flag(&self, s: &mut FastSession) -> bool {
        state_read_flag(&self.chunks, &mut s.state)
    }

    /// Read a sign flag and return `v` with it applied.
    #[inline(always)]
    pub(crate) fn session_read_signed(&self, s: &mut FastSession, v: i32) -> i32 {
        state_read_signed(&self.chunks, &mut s.state, v)
    }

    #[inline(always)]
    pub(crate) fn session_read_with_tree(
        &self,
        s: &mut FastSession,
        tree: &[TreeNode],
        mut node: TreeNode,
    ) -> i8 {
        loop {
            let b = state_read_bit(&self.chunks, &mut s.state, node.prob);
            let i = if b { node.right } else { node.left };
            let Some(next_node) = tree.get(usize::from(i)) else {
                return TreeNode::value_from_branch(i);
            };
            node = *next_node;
        }
    }
}

impl FastDecoder<'_> {
    fn commit_if_valid<T>(self, value_if_not_past_eof: T) -> Option<T> {
        // If `chunk_index > self.chunks.len()`, it means we used zeroes
        // instead of an actual chunk and `value_if_not_past_eof` is nonsense.
        if self.uncommitted_state.chunk_index <= self.chunks.len() {
            *self.save_state = self.uncommitted_state;
            Some(value_if_not_past_eof)
        } else {
            None
        }
    }

    fn read_bool(mut self, probability: u8) -> Option<bool> {
        let bit = self.fast_read_bit(probability);
        self.commit_if_valid(bit)
    }

    fn read_flag(mut self) -> Option<bool> {
        let value = self.fast_read_flag();
        self.commit_if_valid(value)
    }

    fn read_literal(mut self, n: u8) -> Option<u8> {
        let value = self.fast_read_literal(n);
        self.commit_if_valid(value)
    }

    fn read_optional_signed_value(mut self, n: u8) -> Option<i32> {
        let flag = self.fast_read_flag();
        if !flag {
            // We should not read further bits if the flag is not set.
            return self.commit_if_valid(0);
        }
        let magnitude = self.fast_read_literal(n);
        let sign = self.fast_read_flag();
        let value = if sign {
            -i32::from(magnitude)
        } else {
            i32::from(magnitude)
        };
        self.commit_if_valid(value)
    }

    fn read_with_tree(mut self, tree: &[TreeNode], first_node: TreeNode) -> Option<i8> {
        let value = self.fast_read_with_tree(tree, first_node);
        self.commit_if_valid(value)
    }

    fn fast_read_bit(&mut self, probability: u8) -> bool {
        state_read_bit(self.chunks, &mut self.uncommitted_state, probability)
    }

    fn fast_read_flag(&mut self) -> bool {
        state_read_flag(self.chunks, &mut self.uncommitted_state)
    }

    fn fast_read_literal(&mut self, n: u8) -> u8 {
        let mut v = 0u8;
        for _ in 0..n {
            let b = self.fast_read_flag();
            v = (v << 1) + u8::from(b);
        }
        v
    }

    fn fast_read_with_tree(&mut self, tree: &[TreeNode], mut node: TreeNode) -> i8 {
        loop {
            let prob = node.prob;
            let b = self.fast_read_bit(prob);
            let i = if b { node.right } else { node.left };
            let Some(next_node) = tree.get(usize::from(i)) else {
                return TreeNode::value_from_branch(i);
            };
            node = *next_node;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arithmetic_decoder_hello_short() {
        let mut decoder = ArithmeticDecoder::new();
        let data = b"hel";
        let size = data.len();
        let mut buf = vec![[0u8; 4]; 1];
        buf.as_mut_slice().as_flattened_mut()[..size].copy_from_slice(&data[..]);
        decoder.init(buf, size).unwrap();
        let mut res = decoder.start_accumulated_result();
        assert_eq!(false, decoder.read_flag().or_accumulate(&mut res));
        assert_eq!(true, decoder.read_bool(10).or_accumulate(&mut res));
        assert_eq!(false, decoder.read_bool(250).or_accumulate(&mut res));
        assert_eq!(1, decoder.read_literal(1).or_accumulate(&mut res));
        assert_eq!(5, decoder.read_literal(3).or_accumulate(&mut res));
        assert_eq!(64, decoder.read_literal(8).or_accumulate(&mut res));
        assert_eq!(185, decoder.read_literal(8).or_accumulate(&mut res));
        decoder.check(res, ()).unwrap();
    }

    #[test]
    fn test_arithmetic_decoder_hello_long() {
        let mut decoder = ArithmeticDecoder::new();
        let data = b"hello world";
        let size = data.len();
        let mut buf = vec![[0u8; 4]; (size + 3) / 4];
        buf.as_mut_slice().as_flattened_mut()[..size].copy_from_slice(&data[..]);
        decoder.init(buf, size).unwrap();
        let mut res = decoder.start_accumulated_result();
        assert_eq!(false, decoder.read_flag().or_accumulate(&mut res));
        assert_eq!(true, decoder.read_bool(10).or_accumulate(&mut res));
        assert_eq!(false, decoder.read_bool(250).or_accumulate(&mut res));
        assert_eq!(1, decoder.read_literal(1).or_accumulate(&mut res));
        assert_eq!(5, decoder.read_literal(3).or_accumulate(&mut res));
        assert_eq!(64, decoder.read_literal(8).or_accumulate(&mut res));
        assert_eq!(185, decoder.read_literal(8).or_accumulate(&mut res));
        assert_eq!(31, decoder.read_literal(8).or_accumulate(&mut res));
        decoder.check(res, ()).unwrap();
    }

    /// `state_read_signed` must leave the same state and give the same sign
    /// as `state_read_flag`, for every normalised range and bit position.
    #[test]
    fn signed_read_matches_flag_read() {
        let chunks: Vec<[u8; 4]> = (0u32..64)
            .map(|i| (i.wrapping_mul(2654435761) ^ 0x5bd1e995).to_be_bytes())
            .collect();
        for range_m1 in 127u32..=254 {
            for bit_count in -1i32..32 {
                for window in 0..=range_m1 {
                    // any value below `range << bit_count` is a valid state
                    let shift = bit_count.max(0) as u32;
                    let low = u64::from(window.wrapping_mul(0x9e37_79b9)) & ((1u64 << shift) - 1);
                    let start = State {
                        chunk_index: 3,
                        value: if bit_count < 0 {
                            u64::from(window) >> 1
                        } else {
                            (u64::from(window) << shift) | low
                        },
                        range_m1,
                        bit_count,
                    };
                    let (mut a, mut b) = (start, start);
                    let flag = state_read_flag(&chunks, &mut a);
                    let signed = state_read_signed(&chunks, &mut b, 7);
                    assert_eq!(signed, if flag { -7 } else { 7 });
                    assert_eq!(
                        (a.chunk_index, a.value, a.range_m1, a.bit_count),
                        (b.chunk_index, b.value, b.range_m1, b.bit_count),
                        "range_m1 {range_m1} bit_count {bit_count} window {window}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_arithmetic_decoder_uninit() {
        let mut decoder = ArithmeticDecoder::new();
        let mut res = decoder.start_accumulated_result();
        let _ = decoder.read_flag().or_accumulate(&mut res);
        let result = decoder.check(res, ());
        assert!(result.is_err());
    }
}
