//! FLAC's two CRCs (no reflection, init 0), table-driven.
//!
//! CRC-8  poly 0x07  — over the frame header.
//! CRC-16 poly 0x8005 — over the whole frame.

const fn build_crc8_table() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

const fn build_crc16_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

const CRC8_TABLE: [u8; 256] = build_crc8_table();
const CRC16_TABLE: [u16; 256] = build_crc16_table();

pub fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc = CRC8_TABLE[(crc ^ b) as usize];
    }
    crc
}

/// Slice-by-8 tables: `CRC16_SLICE[k][x]` is the CRC of byte `x` followed by
/// `k` zero bytes, so eight message bytes fold in with eight independent
/// lookups instead of a serial chain of eight.
static CRC16_SLICE: [[u16; 256]; 8] = {
    let mut t = [[0u16; 256]; 8];
    t[0] = CRC16_TABLE;
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev << 8) ^ CRC16_TABLE[(prev >> 8) as usize];
            i += 1;
        }
        k += 1;
    }
    t
};

/// CRC-16 of `data`. The decoder runs every frame byte through it, so it is
/// slice-by-8 (one serial table step per 8 bytes, not per byte);
/// [`crc16_bytewise`] is the oracle.
pub fn crc16(data: &[u8]) -> u16 {
    let t = &CRC16_SLICE;
    let mut crc = 0u16;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        crc = t[7][(c[0] ^ (crc >> 8) as u8) as usize]
            ^ t[6][(c[1] ^ crc as u8) as usize]
            ^ t[5][c[2] as usize]
            ^ t[4][c[3] as usize]
            ^ t[3][c[4] as usize]
            ^ t[2][c[5] as usize]
            ^ t[1][c[6] as usize]
            ^ t[0][c[7] as usize];
    }
    crc16_bytewise(crc, chunks.remainder())
}

/// Byte-at-a-time CRC-16 continuing from `crc` (the table-driven original).
fn crc16_bytewise(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc = (crc << 8) ^ CRC16_TABLE[((crc >> 8) ^ b as u16) as usize];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Bitwise reference implementations (the pre-table originals).
    fn crc8_ref(data: &[u8]) -> u8 {
        let mut crc = 0u8;
        for &b in data {
            crc ^= b;
            for _ in 0..8 {
                crc = if crc & 0x80 != 0 {
                    (crc << 1) ^ 0x07
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    fn crc16_ref(data: &[u8]) -> u16 {
        let mut crc = 0u16;
        for &b in data {
            crc ^= (b as u16) << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 != 0 {
                    (crc << 1) ^ 0x8005
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    #[test]
    fn tables_match_bitwise_reference() {
        let data: Vec<u8> = (0..4096u32).map(|i| (i * 31 + 7) as u8).collect();
        for len in [0, 1, 2, 63, 64, 65, 4096] {
            assert_eq!(crc8(&data[..len]), crc8_ref(&data[..len]), "crc8 len={len}");
            assert_eq!(
                crc16(&data[..len]),
                crc16_ref(&data[..len]),
                "crc16 len={len}"
            );
        }
    }

    /// Slice-by-8 agrees with the bytewise oracle at every length / tail and
    /// on random bytes (incl. the 0xFF / 0x00 extremes).
    #[test]
    fn crc16_slice_by_8_matches_bytewise() {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let data: Vec<u8> = (0..3000)
            .map(|i| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                match i % 97 {
                    0..=5 => 0xFF,
                    6..=9 => 0,
                    _ => (x >> 56) as u8,
                }
            })
            .collect();
        for len in 0..200 {
            assert_eq!(
                crc16(&data[..len]),
                crc16_bytewise(0, &data[..len]),
                "len={len}"
            );
            assert_eq!(crc16(&data[..len]), crc16_ref(&data[..len]), "len={len}");
        }
        assert_eq!(crc16(&data), crc16_bytewise(0, &data));
    }
}
