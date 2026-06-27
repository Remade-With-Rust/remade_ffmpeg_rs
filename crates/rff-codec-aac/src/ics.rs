//! `ics_info` — per-channel window/grouping configuration (ISO 14496-3
//! §4.4.2.1) and the derived short-window grouping.

#![allow(dead_code)]

use rff_core::{Error, Result};

use crate::bits::BitReader;
use crate::swb::swb_offsets;

/// AAC window sequence (transform block structure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSequence {
    OnlyLong,
    LongStart,
    EightShort,
    LongStop,
}

impl WindowSequence {
    fn from_bits(v: u32) -> WindowSequence {
        match v {
            0 => WindowSequence::OnlyLong,
            1 => WindowSequence::LongStart,
            2 => WindowSequence::EightShort,
            _ => WindowSequence::LongStop,
        }
    }

    pub fn is_short(self) -> bool {
        self == WindowSequence::EightShort
    }
}

/// Parsed `ics_info` plus the derived grouping for this channel.
#[derive(Debug, Clone)]
pub struct IcsInfo {
    pub window_sequence: WindowSequence,
    /// Window shape for this block: false = sine, true = KBD.
    pub window_shape_kbd: bool,
    pub max_sfb: u8,
    /// Number of transform windows (8 for EIGHT_SHORT, else 1).
    pub num_windows: usize,
    /// Number of window groups.
    pub num_window_groups: usize,
    /// Windows per group (`num_window_groups` entries summing to `num_windows`).
    pub window_group_length: Vec<u8>,
    /// Number of scalefactor bands per window for this block type.
    pub num_swb: usize,
}

/// Parse `ics_info` from the bitstream for sampling-frequency index `fs_index`.
pub fn parse_ics_info(r: &mut BitReader, fs_index: u8) -> Result<IcsInfo> {
    let _ics_reserved = r.read_bit()?;
    let window_sequence = WindowSequence::from_bits(r.read_bits(2)?);
    let window_shape_kbd = r.read_bool()?;

    let (max_sfb, scale_factor_grouping) = if window_sequence.is_short() {
        let max_sfb = r.read_bits(4)? as u8;
        let sfg = r.read_bits(7)?;
        (max_sfb, Some(sfg))
    } else {
        let max_sfb = r.read_bits(6)? as u8;
        let predictor_data_present = r.read_bool()?;
        if predictor_data_present {
            // AAC-LC has no prediction; a set bit means a non-LC stream.
            return Err(Error::unsupported(
                "aac: predictor_data_present (non-LC profile) not supported",
            ));
        }
        (max_sfb, None)
    };

    let (num_windows, num_window_groups, window_group_length) = match scale_factor_grouping {
        Some(sfg) => {
            // Each of the 7 grouping bits (MSB first) ties window i+1 to the
            // current group (1) or starts a new one (0).
            let mut groups = vec![1u8];
            for i in 0..7 {
                if (sfg >> (6 - i)) & 1 == 1 {
                    *groups.last_mut().unwrap() += 1;
                } else {
                    groups.push(1);
                }
            }
            (8, groups.len(), groups)
        }
        None => (1, 1, vec![1u8]),
    };

    let num_swb = swb_offsets(!window_sequence.is_short(), fs_index).len() - 1;

    Ok(IcsInfo {
        window_sequence,
        window_shape_kbd,
        max_sfb,
        num_windows,
        num_window_groups,
        window_group_length,
        num_swb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_long() {
        // ics_reserved(0) ws(00=OnlyLong) shape(0) max_sfb(49=110001) pred(0)
        // bits: 0 00 0 110001 0 → 0000 1100 010. = 0x0C 0x40
        let mut r = BitReader::new(&[0x0C, 0x40]);
        let info = parse_ics_info(&mut r, 4).unwrap();
        assert_eq!(info.window_sequence, WindowSequence::OnlyLong);
        assert!(!info.window_shape_kbd);
        assert_eq!(info.max_sfb, 49);
        assert_eq!(info.num_windows, 1);
        assert_eq!(info.num_window_groups, 1);
        assert_eq!(info.window_group_length, vec![1]);
        assert_eq!(info.num_swb, 49); // 44.1 kHz long
    }

    #[test]
    fn parses_eight_short_all_grouped() {
        // ics_reserved(0) ws(10=EightShort) shape(1=KBD) max_sfb(13=1101)
        // sfg(1111111 → one group of 8)
        // bits: 0 10 1 1101 1111111 → 0101 1101 1111 111. = 0x5D 0xFE
        let mut r = BitReader::new(&[0x5D, 0xFE]);
        let info = parse_ics_info(&mut r, 3).unwrap();
        assert_eq!(info.window_sequence, WindowSequence::EightShort);
        assert!(info.window_shape_kbd);
        assert_eq!(info.max_sfb, 13);
        assert_eq!(info.num_windows, 8);
        assert_eq!(info.num_window_groups, 1);
        assert_eq!(info.window_group_length, vec![8]);
        assert_eq!(info.num_swb, 14); // 48 kHz short
    }

    #[test]
    fn parses_eight_short_split_groups() {
        // sfg = 0b0101010 → bits decide grouping: produces several groups.
        // ics_reserved(0) ws(10) shape(0) max_sfb(0000) sfg(0101010)
        // bits: 0 10 0 0000 0101010 → 0100 0000  0101 0100 = 0x40 0x54
        let mut r = BitReader::new(&[0x40, 0x54]);
        let info = parse_ics_info(&mut r, 3).unwrap();
        assert_eq!(info.num_windows, 8);
        // 0,1,0,1,0,1,0 grouping bits → groups: [1,2,2,2,1] (sum 8).
        assert_eq!(info.window_group_length.iter().map(|&x| x as usize).sum::<usize>(), 8);
        assert_eq!(info.num_window_groups, info.window_group_length.len());
    }

    #[test]
    fn rejects_prediction() {
        // OnlyLong with predictor_data_present=1 → unsupported (non-LC).
        // 0 00 0 000000 1 → 0000 0000  0010 0000 = 0x00 0x20
        let mut r = BitReader::new(&[0x00, 0x20]);
        assert!(parse_ics_info(&mut r, 4).is_err());
    }
}
