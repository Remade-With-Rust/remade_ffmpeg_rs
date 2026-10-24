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

/// CRC-16 of `data`. The decoder runs every frame byte through it: on x86-64
/// with `std` and PCLMULQDQ it folds 64 bytes per step with carry-less
/// multiplies ([`crc16_clmul`]); otherwise it is slice-by-8 (one serial
/// table step per 8 bytes). [`crc16_bytewise`] is the oracle for both.
pub fn crc16(data: &[u8]) -> u16 {
    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    if data.len() >= 64
        && std::arch::is_x86_feature_detected!("pclmulqdq")
        && std::arch::is_x86_feature_detected!("ssse3")
    {
        // SAFETY: the CPU supports the features the kernel was compiled for.
        return unsafe { crc16_clmul(data) };
    }
    crc16_slice8(0, data)
}

/// `x^n mod P(x)` for FLAC's CRC-16 polynomial P = x^16 + x^15 + x^2 + 1,
/// as the 16 coefficient bits (bit i = x^i).
#[cfg(all(feature = "std", target_arch = "x86_64"))]
const fn xpow_mod(n: u32) -> u64 {
    let mut r: u32 = 1;
    let mut i = 0;
    while i < n {
        r <<= 1;
        if r & 0x1_0000 != 0 {
            r ^= 0x1_8005;
        }
        i += 1;
    }
    r as u64
}

/// CRC-16 by carry-less-multiply folding (`len >= 64`).
///
/// The message is read as 128-bit big-endian blocks (bit 127 = the first
/// message bit, so a block is a polynomial with bit i = x^i, which is what
/// PCLMULQDQ multiplies). Four accumulators X_j each take every fourth
/// block: X <- X * x^512 + B, where X * x^512 = X_hi * x^576 + X_lo * x^512
/// ≡ X_hi * (x^576 mod P) + X_lo * (x^512 mod P) — two 64x16-bit carry-less
/// products that fit 128 bits, so the accumulator stays congruent to its
/// share of the message mod P without ever being reduced. The four are then
/// folded into one with the 128-bit step (x^192 / x^128), the leftover
/// 16-byte blocks folded in the same way, and the tail bytes left over.
///
/// The final remainder needs no Barrett reduction: the table CRC of the
/// accumulator's 16 big-endian bytes is exactly X * x^16 mod P, which is the
/// CRC of everything folded so far; the tail then continues from it.
#[cfg(all(feature = "std", target_arch = "x86_64"))]
#[target_feature(enable = "pclmulqdq,ssse3")]
unsafe fn crc16_clmul(data: &[u8]) -> u16 {
    use core::arch::x86_64::*;
    debug_assert!(data.len() >= 64);
    const K128: (u64, u64) = (xpow_mod(192), xpow_mod(128));
    const K512: (u64, u64) = (xpow_mod(576), xpow_mod(512));
    let rev = _mm_set_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    // Macros, not closures, so the intrinsics are compiled in this
    // function's target-feature context.
    // SAFETY (all loads): `at + 16 <= data.len()` at every use.
    macro_rules! load {
        ($at:expr) => {
            _mm_shuffle_epi8(
                unsafe { _mm_loadu_si128(data.as_ptr().add($at) as *const __m128i) },
                rev,
            )
        };
    }
    macro_rules! fold {
        ($x:expr, $k:expr) => {
            _mm_xor_si128(
                _mm_clmulepi64_si128($x, $k, 0x11),
                _mm_clmulepi64_si128($x, $k, 0x00),
            )
        };
    }
    let k128 = _mm_set_epi64x(K128.0 as i64, K128.1 as i64);
    let k512 = _mm_set_epi64x(K512.0 as i64, K512.1 as i64);

    let groups = data.len() / 64;
    let (mut x0, mut x1, mut x2, mut x3) = (load!(0), load!(16), load!(32), load!(48));
    for g in 1..groups {
        let at = g * 64;
        x0 = _mm_xor_si128(fold!(x0, k512), load!(at));
        x1 = _mm_xor_si128(fold!(x1, k512), load!(at + 16));
        x2 = _mm_xor_si128(fold!(x2, k512), load!(at + 32));
        x3 = _mm_xor_si128(fold!(x3, k512), load!(at + 48));
    }
    let mut x = _mm_xor_si128(fold!(x0, k128), x1);
    x = _mm_xor_si128(fold!(x, k128), x2);
    x = _mm_xor_si128(fold!(x, k128), x3);
    let mut at = groups * 64;
    while at + 16 <= data.len() {
        x = _mm_xor_si128(fold!(x, k128), load!(at));
        at += 16;
    }
    let mut be = [0u8; 16];
    // SAFETY: `be` is 16 writable bytes.
    unsafe { _mm_storeu_si128(be.as_mut_ptr() as *mut __m128i, _mm_shuffle_epi8(x, rev)) };
    crc16_slice8(crc16_slice8(0, &be), &data[at..])
}

/// Slice-by-8 CRC-16 continuing from `crc`.
fn crc16_slice8(mut crc: u16, data: &[u8]) -> u16 {
    let t = &CRC16_SLICE;
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

    /// The carry-less-multiply fold agrees with the bytewise oracle at every
    /// length 64..=1100 (every group / block / tail split) and on a long
    /// buffer, over random bytes with 0xFF / 0x00 runs; so does the
    /// dispatching `crc16` (whichever path this CPU takes).
    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    #[test]
    fn crc16_clmul_matches_bytewise() {
        let mut x = 0x1234_5678_9ABC_DEF1u64;
        let data: Vec<u8> = (0..70_000)
            .map(|i| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                match i % 251 {
                    0..=20 => 0xFF,
                    21..=40 => 0,
                    _ => (x >> 56) as u8,
                }
            })
            .collect();
        let have = std::arch::is_x86_feature_detected!("pclmulqdq")
            && std::arch::is_x86_feature_detected!("ssse3");
        for start in [0usize, 1, 7] {
            for len in 64..=1100 {
                let d = &data[start..start + len];
                let want = crc16_bytewise(0, d);
                assert_eq!(crc16(d), want, "start={start} len={len}");
                assert_eq!(crc16_slice8(0, d), want, "slice8 len={len}");
                if have {
                    assert_eq!(unsafe { crc16_clmul(d) }, want, "clmul len={len}");
                }
            }
        }
        assert_eq!(crc16(&data), crc16_bytewise(0, &data));
        // A frame's own CRC appended brings the residue to zero.
        let mut f = data[..5000].to_vec();
        let c = crc16(&f);
        f.extend_from_slice(&c.to_be_bytes());
        assert_eq!(crc16(&f), 0);
    }
}
