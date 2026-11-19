//! G.711 A-law and µ-law: one byte per sample, expanded to s16.
//!
//! The expansions are the ITU-T G.711 reference ones (the same as FFmpeg's
//! `alaw2linear` / `ulaw2linear`), built once into 256-entry tables.

/// Expand one A-law byte to linear 16-bit.
pub(crate) const fn alaw_to_s16(byte: u8) -> i16 {
    let a = byte ^ 0x55;
    let mut t = ((a & 0x0f) as i32) << 4;
    let seg = ((a & 0x70) >> 4) as i32;
    match seg {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= seg - 1;
        }
    }
    (if a & 0x80 != 0 { t } else { -t }) as i16
}

/// Expand one µ-law byte to linear 16-bit.
pub(crate) const fn ulaw_to_s16(byte: u8) -> i16 {
    const BIAS: i32 = 0x84;
    let u = !byte;
    let mut t = (((u & 0x0f) as i32) << 3) + BIAS;
    t <<= (u & 0x70) >> 4;
    (if u & 0x80 != 0 { BIAS - t } else { t - BIAS }) as i16
}

const fn table(alaw: bool) -> [i16; 256] {
    let mut t = [0i16; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = if alaw {
            alaw_to_s16(i as u8)
        } else {
            ulaw_to_s16(i as u8)
        };
        i += 1;
    }
    t
}

pub(crate) static ALAW: [i16; 256] = table(true);
pub(crate) static ULAW: [i16; 256] = table(false);

/// Expand a packet of G.711 bytes through `table` to little-endian s16.
pub(crate) fn expand(table: &[i16; 256], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 2);
    for &b in data {
        out.extend_from_slice(&table[b as usize].to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_points() {
        // Values from the G.711 tables (and FFmpeg's pcm_alaw / pcm_mulaw).
        assert_eq!(ULAW[0xFF], 0);
        assert_eq!(ULAW[0x7F], 0);
        assert_eq!(ULAW[0x00], -32124);
        assert_eq!(ULAW[0x80], 32124);
        assert_eq!(ALAW[0xD5], 8);
        assert_eq!(ALAW[0x55], -8);
        assert_eq!(ALAW[0xAA], 32256);
        assert_eq!(ALAW[0x2A], -32256);
    }

    #[test]
    fn tables_are_odd_symmetric_and_monotone() {
        // Flipping the sign bit negates the value, and within one sign the
        // magnitude grows with the code (A-law after undoing its 0x55 mask).
        for b in 0..=255u8 {
            assert_eq!(ALAW[b as usize], -ALAW[(b ^ 0x80) as usize]);
        }
        for b in 0..0x7Fu8 {
            let (lo, hi) = (ULAW[(0xFF - b) as usize], ULAW[(0xFE - b) as usize]);
            assert!(hi > lo, "µ-law not monotone at {b}");
            let (lo, hi) = (ALAW[((b ^ 0x55) | 0x80) as usize], ALAW[(((b + 1) ^ 0x55) | 0x80) as usize]);
            assert!(hi > lo, "A-law not monotone at {b}");
        }
    }
}
