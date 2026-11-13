//! Byte-identity gate: a digest over the encoder's output for a matrix of
//! content, layouts, lengths and levels. Encoder work that is meant to leave
//! the stream unchanged (window tables, buffer reuse, peak-heap trims) must
//! leave this digest unchanged.
//!
//! With `libm` the encoder's float decisions are platform-independent, so the
//! digest is pinned. Without it the platform libm decides the windows and the
//! digest is only printed (compare it before and after a change on one
//! machine).

fn fnv(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

fn signal(class: u32, n: usize, bps: u32, seed: u64) -> Vec<i32> {
    let full = ((1i64 << (bps - 1)) - 1) as f64;
    let mut s = seed;
    let mut lp = 0.0f64;
    (0..n)
        .map(|i| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let white = ((s >> 33) as f64 / (1u64 << 31) as f64) - 0.5;
            lp = 0.9 * lp + 0.1 * white;
            let t = i as f64;
            let v = match class {
                0 => lp * full * 0.02 + (t * 0.0196).sin() * full * 0.001, // quiet room
                1 => (t * 0.037).sin() * full * 0.7 + white * full * 0.01, // tone
                2 => (t * (0.002 + 0.25 * t / n as f64)).sin() * full * 0.6, // sweep
                _ => white * full * 1.6,                                   // loud noise
            };
            (v as i64).clamp(-(full as i64) - 1, full as i64) as i32
        })
        .collect()
}

fn digest() -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &(ch, bps) in &[(1u32, 16u32), (2, 16), (2, 24), (3, 8)] {
        for &n in &[1usize, 17, 512, 3904, 4096, 8000, 8192, 12345] {
            for class in 0..4u32 {
                let chans: Vec<Vec<i32>> = (0..ch)
                    .map(|c| signal(class, n, bps, 11 + c as u64 * 7 + class as u64))
                    .collect();
                let mut inter = Vec::with_capacity(n * ch as usize);
                for i in 0..n {
                    for c in &chans {
                        inter.push(c[i]);
                    }
                }
                for &level in &[0u32, 5, 8] {
                    let mut enc = rusty_flac::Encoder::new(16000, ch, bps).unwrap();
                    enc.set_compression_level(level);
                    enc.push_interleaved(&inter).unwrap();
                    fnv(&mut h, &enc.finish());
                }
            }
        }
    }
    h
}

#[test]
fn encoder_output_digest() {
    let d = digest();
    println!("stream digest: {d:#018x}");
    #[cfg(feature = "libm")]
    assert_eq!(d, PINNED_LIBM, "encoder output changed (libm build)");
}

/// Pinned when the LPC autocorrelation became integer arithmetic (Q15
/// windows, exact i64 lag sums). Before that the digest was
/// 0x3a773fb5ac8bbf92 under libm and 0x0023377cf11261fb with the platform
/// libm (x86_64-pc-windows-msvc); since then both builds produce this one on
/// that host, as they do for every file of the corpus gate.
#[cfg(feature = "libm")]
const PINNED_LIBM: u64 = 0xe97402e36f8f2429;
