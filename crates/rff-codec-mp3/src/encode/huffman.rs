//! Huffman encoding of the quantized spectrum — the inverse of decode's spectrum
//! decode (`decode/huffman.rs`). Bricks **B1–B4** (+ **N8** linbits and **N9**
//! region maps, both reused from the decode side).
//!
//! The quantized lines are partitioned into the `big_values` region (coded as
//! pairs by up to three sub-regions, each picking one of the 32 ISO pair-tables)
//! and the `count1` region (quads of `{0,±1}`). Everything past the last non-zero
//! line is implicit `rzero`. We reuse the decoder's canonical codebooks
//! (`PAIR_TABLES`, `QUAD_A/B`) — encoding is the same `(code, len)` data indexed
//! by symbol instead of matched bit-by-bit — so a round-trip through
//! `decode::huffman::decode` reproduces the coefficients exactly. That is B's
//! verification gate.

use crate::bitio::BitWriter;
use crate::decode::codebooks::{PAIR_TABLES, QUAD_A, QUAD_B};
use crate::decode::huffman::{PairTable, QuadTable};
use crate::frame::{BlockType, GranuleSideInfo, GRANULE_LINES};
use crate::header::FrameHeader;
use crate::tables;

use super::quantize::QuantizedGranule;

// ── B1/B2: per-symbol cost + emit (pairs and quads) ───────────────────────────

/// Bits to code one coordinate's escape+sign under a pair table, or `None` if the
/// value is out of the table's range (no escape big enough).
fn coord_bits(t: &PairTable, v: i32) -> Option<usize> {
    let maxc = t.dim as i32 - 1;
    let a = v.abs();
    let sign = if v != 0 { 1 } else { 0 };
    if t.linbits == 0 {
        if a > maxc {
            None
        } else {
            Some(sign)
        }
    } else if a >= maxc {
        if (a - maxc) as u32 >= (1u32 << t.linbits) {
            None
        } else {
            Some(t.linbits as usize + sign)
        }
    } else {
        Some(sign)
    }
}

/// Bit cost of coding pair `(x, y)` with pair table `t` (codeword + escapes +
/// signs), or `None` if either value is out of range.
fn pair_bits(t: &PairTable, x: i32, y: i32) -> Option<usize> {
    if t.dim == 0 {
        return if x == 0 && y == 0 { Some(0) } else { None };
    }
    let maxc = t.dim as i32 - 1;
    let cx = x.abs().min(maxc) as usize;
    let cy = y.abs().min(maxc) as usize;
    let idx = cx * t.dim as usize + cy;
    let (_, len) = t.book.code_len(idx)?;
    Some(len as usize + coord_bits(t, x)? + coord_bits(t, y)?)
}

/// Emit pair `(x, y)` with table `t`. Read order in the decoder is codeword, then
/// x-escape, x-sign, y-escape, y-sign — mirrored here.
fn pair_emit(t: &PairTable, x: i32, y: i32, w: &mut BitWriter) {
    if t.dim == 0 {
        return;
    }
    let maxc = t.dim as i32 - 1;
    let cx = x.abs().min(maxc) as usize;
    let cy = y.abs().min(maxc) as usize;
    let idx = cx * t.dim as usize + cy;
    let (code, len) = t.book.code_len(idx).expect("pair index in book");
    w.write(code as u32, len as u32);
    if t.linbits > 0 && x.abs() >= maxc {
        w.write((x.abs() - maxc) as u32, t.linbits as u32);
    }
    if x != 0 {
        w.write((x < 0) as u32, 1);
    }
    if t.linbits > 0 && y.abs() >= maxc {
        w.write((y.abs() - maxc) as u32, t.linbits as u32);
    }
    if y != 0 {
        w.write((y < 0) as u32, 1);
    }
}

fn quad_index(v: i32, w: i32, x: i32, y: i32) -> usize {
    (((v.abs() & 1) << 3) | ((w.abs() & 1) << 2) | ((x.abs() & 1) << 1) | (y.abs() & 1)) as usize
}

fn quad_bits(q: &QuadTable, v: i32, w: i32, x: i32, y: i32) -> usize {
    let (_, len) = q
        .book
        .code_len(quad_index(v, w, x, y))
        .expect("quad index 0..15");
    len as usize + [v, w, x, y].iter().filter(|&&c| c != 0).count()
}

fn quad_emit(q: &QuadTable, v: i32, w: i32, x: i32, y: i32, wr: &mut BitWriter) {
    let (code, len) = q
        .book
        .code_len(quad_index(v, w, x, y))
        .expect("quad index 0..15");
    wr.write(code as u32, len as u32);
    for &c in &[v, w, x, y] {
        if c != 0 {
            wr.write((c < 0) as u32, 1);
        }
    }
}

/// **B2** — estimate the bit cost of coding `coeffs` (an even-length pair region)
/// with pair `table`, without emitting. Non-covering tables cost "infinity" so the
/// selector skips them.
pub fn estimate_bits(coeffs: &[i32], table: u8) -> usize {
    let t = PAIR_TABLES[(table as usize).min(PAIR_TABLES.len() - 1)];
    let mut total = 0usize;
    let mut i = 0;
    while i + 1 < coeffs.len() {
        total =
            total.saturating_add(pair_bits(t, coeffs[i], coeffs[i + 1]).unwrap_or(usize::MAX / 4));
        i += 2;
    }
    total
}

// ── N9/B4: region boundaries + table/partition selection ──────────────────────

/// **N9** — long-block region boundaries from `(region0_count, region1_count)`,
/// the exact inverse of the decoder's `region_bounds` (long branch).
fn region_bounds_long(sfb: &[u16; 23], r0c: u8, r1c: u8, bv2: usize) -> (usize, usize) {
    let i1 = (r0c as usize + 1).min(22);
    let i2 = (r0c as usize + r1c as usize + 2).min(22);
    let r1 = (sfb[i1] as usize).min(bv2);
    let r2 = (sfb[i2] as usize).min(bv2).max(r1);
    (r1, r2)
}

/// Cheapest pair table covering `coeffs[lo..hi]` (an even-aligned region).
fn best_pair_table(coeffs: &[i32; GRANULE_LINES], lo: usize, hi: usize) -> u8 {
    if lo >= hi {
        return 0;
    }
    let slice = &coeffs[lo..hi];
    let mut best = (usize::MAX, 0u8);
    for table in 0u8..PAIR_TABLES.len() as u8 {
        let cost = estimate_bits(slice, table);
        if cost < best.0 {
            best = (cost, table);
        }
    }
    best.1
}

/// Cheaper of the two count1 quad tables for `coeffs[lo..hi]` (`false`=A, `true`=B).
fn best_quad_table(coeffs: &[i32; GRANULE_LINES], lo: usize, hi: usize) -> bool {
    let (mut a, mut b) = (0usize, 0usize);
    let mut i = lo;
    while i + 4 <= hi {
        a += quad_bits(
            &QUAD_A,
            coeffs[i],
            coeffs[i + 1],
            coeffs[i + 2],
            coeffs[i + 3],
        );
        b += quad_bits(
            &QUAD_B,
            coeffs[i],
            coeffs[i + 1],
            coeffs[i + 2],
            coeffs[i + 3],
        );
        i += 4;
    }
    b < a
}

/// Roughly split the big-values region into thirds at scalefactor-band boundaries.
fn choose_regions(sfb: &[u16; 23], big_end: usize) -> (u8, u8) {
    if big_end == 0 {
        return (0, 0);
    }
    let third = big_end / 3;
    let two_third = 2 * big_end / 3;
    let i1 = (1..=22)
        .filter(|&i| sfb[i] as usize <= third)
        .next_back()
        .unwrap_or(1);
    let i2 = (i1..=22)
        .filter(|&i| sfb[i] as usize <= two_third)
        .next_back()
        .unwrap_or(i1);
    ((i1 - 1).min(15) as u8, (i2 - i1).min(7) as u8)
}

/// **B4** — partition a quantized spectrum (long block) and pick its codebooks:
/// fills the Huffman-relevant `GranuleSideInfo` fields so [`encode`] and the
/// decoder agree on the layout.
pub fn select(header: &FrameHeader, coeffs: &[i32; GRANULE_LINES]) -> GranuleSideInfo {
    // big_values must cover every line with magnitude > 1 (count1 only codes ±1).
    let big_end = match coeffs.iter().rposition(|&c| c.abs() > 1) {
        Some(i) => (i / 2 + 1) * 2, // even, includes the pair holding line i
        None => 0,
    };
    let last_nz = coeffs.iter().rposition(|&c| c != 0).map_or(0, |i| i + 1);
    let region_end = last_nz.max(big_end);
    let count1_quads = (region_end - big_end).div_ceil(4);
    let count1_end = (big_end + count1_quads * 4).min(GRANULE_LINES);
    debug_assert!(
        coeffs[count1_end..].iter().all(|&c| c == 0),
        "rzero invariant: no non-zero line past the coded region"
    );

    let sfb = tables::sfb_long_offsets(header.sample_rate);
    let (r0c, r1c) = choose_regions(sfb, big_end);
    let (r1, r2) = region_bounds_long(sfb, r0c, r1c, big_end);

    GranuleSideInfo {
        big_values: (big_end / 2) as u16,
        block_type: BlockType::Long,
        region0_count: r0c,
        region1_count: r1c,
        table_select: [
            best_pair_table(coeffs, 0, r1),
            best_pair_table(coeffs, r1, r2),
            best_pair_table(coeffs, r2, big_end),
        ],
        count1table_select: best_quad_table(coeffs, big_end, count1_end),
        ..Default::default()
    }
}

// ── B3: emit the whole spectrum ───────────────────────────────────────────────

/// **B3** — encode one quantized granule's spectrum into `writer`, per the layout
/// in `quant.side`. Returns the number of Huffman bits written (the part3 length);
/// the caller adds the scalefactor (part2) bits to get `part2_3_length`.
pub fn encode(quant: &QuantizedGranule, header: &FrameHeader, writer: &mut BitWriter) -> usize {
    let start = writer.bit_len();
    let gi = &quant.side;
    let coeffs = &quant.coeffs;
    let sfb = tables::sfb_long_offsets(header.sample_rate);
    let bv2 = (gi.big_values as usize * 2).min(GRANULE_LINES);
    let (r1, r2) = region_bounds_long(sfb, gi.region0_count, gi.region1_count, bv2);

    // big_values: pairs, table chosen per region.
    let mut i = 0;
    while i + 1 < bv2 {
        let t = if i < r1 {
            gi.table_select[0]
        } else if i < r2 {
            gi.table_select[1]
        } else {
            gi.table_select[2]
        } as usize;
        pair_emit(
            PAIR_TABLES[t.min(PAIR_TABLES.len() - 1)],
            coeffs[i],
            coeffs[i + 1],
            writer,
        );
        i += 2;
    }

    // count1: quads up to rzero (the decoder stops at the part2_3 budget, which the
    // caller sets from the returned length, so it reads exactly these).
    let quad = if gi.count1table_select {
        &QUAD_B
    } else {
        &QUAD_A
    };
    let rzero = coeffs
        .iter()
        .rposition(|&c| c != 0)
        .map_or(0, |x| x + 1)
        .max(bv2);
    while i + 4 <= GRANULE_LINES && i < rzero {
        quad_emit(
            quad,
            coeffs[i],
            coeffs[i + 1],
            coeffs[i + 2],
            coeffs[i + 3],
            writer,
        );
        i += 4;
    }

    writer.bit_len() - start
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::ChannelMode;
    use crate::header::MpegVersion;

    fn hdr() -> FrameHeader {
        FrameHeader {
            version: MpegVersion::V1,
            crc_protected: false,
            bitrate_kbps: 128,
            sample_rate: 44100,
            padding: false,
            channel_mode: ChannelMode::Mono,
            copyright: false,
            original: true,
            emphasis: 0,
        }
    }

    /// Select tables, encode, decode back — coefficients must survive exactly.
    fn round_trip(coeffs: [i32; GRANULE_LINES]) {
        let header = hdr();
        let side = select(&header, &coeffs);
        let quant = QuantizedGranule {
            coeffs,
            side: side.clone(),
            scalefactors: [0; 39],
        };
        let mut w = BitWriter::new();
        let hlen = encode(&quant, &header, &mut w);
        let bits = w.finish();

        let mut pos = 0;
        let (out, _nz) = crate::decode::huffman::decode(&bits, &mut pos, hlen, &header, &side);
        assert_eq!(pos, hlen, "decoder must consume exactly the emitted bits");
        assert_eq!(out, coeffs, "spectrum must round-trip exactly");
    }

    #[test]
    fn round_trip_count1_only() {
        // Small values + trailing zeros: count1-dominated.
        let mut c = [0i32; GRANULE_LINES];
        for (i, v) in c.iter_mut().take(40).enumerate() {
            *v = [(-1), 0, 1, 1, 0, -1, 0, 1][i % 8];
        }
        round_trip(c);
    }

    #[test]
    fn round_trip_big_values_and_signs() {
        let mut c = [0i32; GRANULE_LINES];
        // Low-frequency big values (need pair tables), then small, then zeros.
        let seed = [12, -7, 3, -2, 5, 9, -4, 2, 1, -1, 1, 0, 2, -3];
        c[..seed.len()].copy_from_slice(&seed);
        for (i, v) in c.iter_mut().take(80).skip(seed.len()).enumerate() {
            *v = [0, 1, -1, 0, 1, 0, -1, 0][i % 8];
        }
        round_trip(c);
    }

    #[test]
    fn round_trip_linbits_escape() {
        // Large magnitudes force the ESC (linbits) tables.
        let mut c = [0i32; GRANULE_LINES];
        c[0] = 600;
        c[1] = -432;
        c[2] = 87;
        c[3] = -15;
        c[4] = 1;
        c[5] = -1;
        round_trip(c);
    }

    #[test]
    fn round_trip_all_zero() {
        round_trip([0i32; GRANULE_LINES]);
    }

    #[test]
    fn round_trip_dense_spectrum() {
        // A broad, deterministic spectrum with rzero in the top lines.
        let mut c = [0i32; GRANULE_LINES];
        let mut s = 0x9E3779B9u32;
        for v in c.iter_mut().take(500) {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let r = (s >> 24) as i32; // 0..255
            *v = (r % 11) - 5; // -5..5
        }
        round_trip(c);
    }

    #[test]
    fn estimate_matches_emitted_bits() {
        // B2's estimate must equal what B3 actually writes for a pure pair region.
        let mut c = [0i32; GRANULE_LINES];
        let seed = [4, -3, 2, -1, 5, 6, -2, 1];
        c[..seed.len()].copy_from_slice(&seed);
        let table = best_pair_table(&c, 0, seed.len());
        let est = estimate_bits(&c[0..seed.len()], table);

        let t = PAIR_TABLES[table as usize];
        let mut w = BitWriter::new();
        let mut i = 0;
        while i + 1 < seed.len() {
            pair_emit(t, c[i], c[i + 1], &mut w);
            i += 2;
        }
        assert_eq!(est, w.bit_len(), "estimate must match emitted bits");
    }
}
