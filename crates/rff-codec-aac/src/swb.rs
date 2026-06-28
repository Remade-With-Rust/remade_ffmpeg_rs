//! Scalefactor-band (SWB) offset tables — ISO 14496-3 Annex 4.A.
//!
//! Each table lists the spectral-coefficient index where each scalefactor band
//! starts, ending with the transform's coefficient count (1024 for long
//! windows, 128 for one short window). `num_swb = table.len() - 1`. Tables are
//! shared across several sampling rates per the spec's grouping.

#![allow(dead_code)]

// ---- Long-window (1024) offsets ------------------------------------------

const LONG_96: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 96, 108, 120, 132,
    144, 156, 172, 188, 212, 240, 276, 320, 384, 448, 512, 576, 640, 704, 768, 832, 896, 960, 1024,
];

const LONG_64: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64, 72, 80, 88, 100, 112, 124, 140,
    156, 172, 192, 216, 240, 268, 304, 344, 384, 424, 464, 504, 544, 584, 624, 664, 704, 744, 784,
    824, 864, 904, 944, 984, 1024,
];

const LONG_48: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 1024,
];

const LONG_32: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 960, 992, 1024,
];

const LONG_24: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 52, 60, 68, 76, 84, 92, 100, 108, 116, 124, 136,
    148, 160, 172, 188, 204, 220, 240, 260, 284, 308, 336, 364, 396, 432, 468, 508, 552, 600, 652,
    704, 768, 832, 896, 960, 1024,
];

const LONG_16: &[u16] = &[
    0, 8, 16, 24, 32, 40, 48, 56, 64, 72, 80, 88, 100, 112, 124, 136, 148, 160, 172, 184, 196, 212,
    228, 244, 260, 280, 300, 320, 344, 368, 396, 424, 456, 492, 532, 572, 616, 664, 716, 772, 832,
    896, 960, 1024,
];

const LONG_8: &[u16] = &[
    0, 12, 24, 36, 48, 60, 72, 84, 96, 108, 120, 132, 144, 156, 172, 188, 204, 220, 236, 252, 268,
    288, 308, 328, 348, 372, 396, 420, 448, 476, 508, 544, 580, 620, 664, 712, 764, 820, 880, 944,
    1024,
];

// ---- Short-window (128) offsets ------------------------------------------

const SHORT_96: &[u16] = &[0, 4, 8, 12, 16, 20, 24, 32, 40, 48, 64, 92, 128];
const SHORT_64: &[u16] = &[0, 4, 8, 12, 16, 20, 24, 32, 40, 48, 64, 92, 128];
const SHORT_48: &[u16] = &[0, 4, 8, 12, 16, 20, 28, 36, 44, 56, 68, 80, 96, 112, 128];
const SHORT_24: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 64, 76, 92, 108, 128,
];
const SHORT_16: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 32, 40, 48, 60, 72, 88, 108, 128,
];
const SHORT_8: &[u16] = &[
    0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 60, 72, 88, 108, 128,
];

/// SWB offset table for a window type and sampling-frequency index. Returns the
/// full offset array (ending in 1024 long / 128 short); `num_swb = len - 1`.
pub fn swb_offsets(long: bool, fs_index: u8) -> &'static [u16] {
    if long {
        match fs_index {
            0 | 1 => LONG_96,
            2 => LONG_64,
            3 | 4 => LONG_48,
            5 => LONG_32,
            6 | 7 => LONG_24,
            8 | 9 | 10 => LONG_16,
            _ => LONG_8,
        }
    } else {
        match fs_index {
            0 | 1 => SHORT_96,
            2 => SHORT_64,
            3 | 4 | 5 => SHORT_48,
            6 | 7 => SHORT_24,
            8 | 9 | 10 => SHORT_16,
            _ => SHORT_8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(table: &[u16], end: u16) {
        assert_eq!(table[0], 0, "must start at 0");
        assert_eq!(*table.last().unwrap(), end, "must end at {end}");
        assert!(
            table.windows(2).all(|w| w[1] > w[0]),
            "offsets must be strictly increasing"
        );
    }

    #[test]
    fn all_long_tables_well_formed() {
        for fs in 0u8..=12 {
            check(swb_offsets(true, fs), 1024);
        }
    }

    #[test]
    fn all_short_tables_well_formed() {
        for fs in 0u8..=12 {
            check(swb_offsets(false, fs), 128);
        }
    }

    #[test]
    fn known_band_counts() {
        // num_swb = len - 1, against the spec's documented counts.
        assert_eq!(swb_offsets(true, 3).len() - 1, 49); // 48 kHz long
        assert_eq!(swb_offsets(true, 5).len() - 1, 51); // 32 kHz long
        assert_eq!(swb_offsets(true, 11).len() - 1, 40); // 8 kHz long
        assert_eq!(swb_offsets(false, 3).len() - 1, 14); // 48 kHz short
        assert_eq!(swb_offsets(false, 8).len() - 1, 15); // 16 kHz short
    }
}
