//! Constant tables defined by ISO/IEC 11172-3 (MPEG-1) and 13818-3 (MPEG-2).
//!
//! The small, fully-specified ones live here as real data. The large DSP tables
//! (the 34 Huffman codebooks, the 512-tap synthesis window, the scalefactor-band
//! boundary tables, the requantization power table) are declared with their
//! shape + a `brick:` note and filled in as we implement each stage — keeping the
//! framework compiling while the data is ported.

// ---- header decode tables (complete) -----------------------------------------

/// Bitrate (kbps) by `bitrate_index`, MPEG-1 Layer III. Index 0 = free format,
/// index 15 = reserved/invalid.
pub const BITRATE_V1_L3: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];

/// Bitrate (kbps) by `bitrate_index`, MPEG-2 / 2.5 Layer III.
pub const BITRATE_V2_L3: [u32; 16] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];

/// Base sample rates (Hz) by `samplerate_index`, for MPEG-1. MPEG-2 halves these
/// and MPEG-2.5 quarters them.
pub const SAMPLE_RATE: [u32; 4] = [44100, 48000, 32000, 0];

// ---- scalefactor decode tables (complete) ------------------------------------

/// `scalefac_compress` → (slen1, slen2): bit-lengths of the two scalefactor
/// groups for MPEG-1 long blocks.
pub const SCALEFAC_COMPRESS_V1: [(u8, u8); 16] = [
    (0, 0),
    (0, 1),
    (0, 2),
    (0, 3),
    (3, 0),
    (1, 1),
    (1, 2),
    (1, 3),
    (2, 1),
    (2, 2),
    (2, 3),
    (3, 1),
    (3, 2),
    (3, 3),
    (4, 2),
    (4, 3),
];

// ---- scalefactor-band boundary tables (to port) ------------------------------

/// Long-block scalefactor-band boundaries: cumulative line offsets (23 entries =
/// 22 bands), indexed by `samplerate_index` (0 = 44100, 1 = 48000, 2 = 32000),
/// MPEG-1. Drives requantization, region derivation, and stereo band maps.
pub const SFB_OFFSET_LONG_V1: [[u16; 23]; 3] = [
    // 44100 Hz
    [
        0, 4, 8, 12, 16, 20, 24, 30, 36, 44, 52, 62, 74, 90, 110, 134, 162, 196, 238, 288, 342,
        418, 576,
    ],
    // 48000 Hz
    [
        0, 4, 8, 12, 16, 20, 24, 30, 36, 42, 50, 60, 72, 88, 106, 128, 156, 190, 230, 276, 330,
        384, 576,
    ],
    // 32000 Hz
    [
        0, 4, 8, 12, 16, 20, 24, 30, 36, 44, 54, 66, 82, 102, 126, 156, 194, 240, 296, 364, 448,
        550, 576,
    ],
];

/// Short-block scalefactor-band boundaries: per-window line offsets (14 entries =
/// 13 bands), indexed by `samplerate_index`, MPEG-1. The max (192) is one window
/// of the 576/3 short-block lines; reorder interleaves the three windows.
pub const SFB_OFFSET_SHORT_V1: [[u16; 14]; 3] = [
    // 44100 Hz
    [0, 4, 8, 12, 16, 22, 30, 40, 52, 66, 84, 106, 136, 192],
    // 48000 Hz
    [0, 4, 8, 12, 16, 22, 28, 38, 50, 64, 80, 100, 126, 192],
    // 32000 Hz
    [0, 4, 8, 12, 16, 22, 30, 42, 58, 78, 104, 138, 180, 192],
];

// brick: SFB_OFFSET_{LONG,SHORT}_V2 for MPEG-2 (22050/24000/16000) and MPEG-2.5
// (11025/12000/8000) from ISO 13818-3 Table B.8.

/// Long-block scalefactor-band offsets for a sample rate (MPEG-1 rates for now).
pub fn sfb_long_offsets(sample_rate: u32) -> &'static [u16; 23] {
    let idx = match sample_rate {
        48000 => 1,
        32000 => 2,
        _ => 0, // 44100 (and, until V2 tables land, the fallback)
    };
    &SFB_OFFSET_LONG_V1[idx]
}

/// Short-block scalefactor-band offsets for a sample rate (MPEG-1 rates).
pub fn sfb_short_offsets(sample_rate: u32) -> &'static [u16; 14] {
    let idx = match sample_rate {
        48000 => 1,
        32000 => 2,
        _ => 0,
    };
    &SFB_OFFSET_SHORT_V1[idx]
}

/// Preflag additive table for long blocks (added to high-band scalefactors when
/// `preflag` is set). 22 entries (band 21 is uncoded → 0).
pub const PRETAB: [u8; 22] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 3, 2, 0,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sfb_offsets_monotonic_and_complete() {
        for row in &SFB_OFFSET_LONG_V1 {
            assert_eq!(row[0], 0);
            assert_eq!(
                *row.last().unwrap(),
                576,
                "long bands must cover all 576 lines"
            );
            assert!(
                row.windows(2).all(|w| w[0] < w[1]),
                "long sfb strictly increasing"
            );
        }
        for row in &SFB_OFFSET_SHORT_V1 {
            assert_eq!(row[0], 0);
            assert_eq!(*row.last().unwrap(), 192, "short window spans 576/3 lines");
            assert!(
                row.windows(2).all(|w| w[0] < w[1]),
                "short sfb strictly increasing"
            );
        }
    }
}

// ---- requantization (to port) ------------------------------------------------

/// `x^(4/3)` lookup for the 8207 possible Huffman magnitudes, the hot path of
/// requantization. Built once at init rather than `powf` per line.
///
/// brick: generate `i.powf(4.0/3.0)` for i in 0..8207 (lazy `OnceLock`).
pub const POW43_LEN: usize = 8207;

// ---- Huffman codebooks (to port) ---------------------------------------------

/// The 34 Layer III Huffman tables (big-value pairs) plus the 2 `count1` quad
/// tables. Each is a `(value, code, len)` set; the decoder builds a fast lookup.
///
/// brick: port the codeword tables from ISO Table B.7 and the linbits per table.
pub const HUFFMAN_TABLE_COUNT: usize = 34;

// ---- synthesis filterbank (to port) ------------------------------------------

// The 512-tap polyphase synthesis window `D[i]` (ISO 11172-3 Table 3-B.3) is the
// canonical ISO data generated into `decode/synth_window.rs`.
