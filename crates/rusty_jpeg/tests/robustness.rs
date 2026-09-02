//! The decoder must not PANIC on malformed input.
//!
//! A decoder is a parser for hostile bytes. `Err` is the correct outcome for a
//! corrupt file; a panic is not — in a library it takes down the caller's
//! process, and this crate is published for anyone to point at untrusted data.
//!
//! `fuzz/` carries proper cargo-fuzz targets for deep exploration. This is the
//! part that runs on stable, in ordinary CI, on every change: a deterministic
//! mutation schedule over self-generated seeds. Deterministic matters — a
//! failure here reproduces exactly and converts straight into a regression test,
//! rather than being a note that a fuzzer once saw something.
//!
//! Measured baseline when this was written: 80,000 mutated inputs across four
//! seeds produced 38,192 successful decodes, 41,808 clean rejections and
//! **zero panics**.

use rusty_jpeg::decode::Decoder;
use rusty_jpeg::encode::{ColorType, Encoder, SamplingFactor};

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn seed_jpeg(w: usize, h: usize, sampling: SamplingFactor, optimize: bool) -> Vec<u8> {
    let mut rgb = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let o = (y * w + x) * 3;
            rgb[o] = ((x * 7 + y * 3) % 256) as u8;
            rgb[o + 1] = ((x * 3 + y * 11) % 256) as u8;
            rgb[o + 2] = ((x * 13 + y * 5) % 256) as u8;
        }
    }
    let mut out = Vec::new();
    let mut enc = Encoder::new(&mut out, 85);
    enc.set_sampling_factor(sampling);
    enc.set_optimized_huffman_tables(optimize);
    enc.encode(&rgb, w as u16, h as u16, ColorType::Rgb)
        .expect("seed encode");
    out
}

/// Mutations aimed where a JPEG parser is structurally weakest: marker bytes,
/// segment lengths, and the end of the entropy stream.
fn mutate(src: &[u8], kind: u64, rng: &mut u64) -> Vec<u8> {
    let mut v = src.to_vec();
    if v.len() < 8 {
        return v;
    }
    match kind % 6 {
        0 => {
            let i = (xorshift(rng) as usize) % v.len();
            v[i] ^= 1 << (xorshift(rng) % 8);
        }
        1 => {
            let n = (xorshift(rng) as usize) % v.len();
            v.truncate(n.max(2));
        }
        2 => {
            for i in 2..v.len().saturating_sub(4) {
                if v[i] == 0xFF && !matches!(v[i + 1], 0x00 | 0xD8 | 0xD9) {
                    v[i + 2] = xorshift(rng) as u8;
                    v[i + 3] = xorshift(rng) as u8;
                    break;
                }
            }
        }
        3 => {
            let i = (xorshift(rng) as usize) % (v.len() - 2) + 1;
            v[i] = 0xFF;
            v[i + 1] = (xorshift(rng) % 0xFD) as u8 + 1;
        }
        4 => {
            let i = (xorshift(rng) as usize) % v.len();
            let n = ((xorshift(rng) as usize) % 64).min(v.len() - i);
            for b in &mut v[i..i + n] {
                *b = xorshift(rng) as u8;
            }
        }
        _ => {
            let i = (xorshift(rng) as usize) % v.len();
            let n = ((xorshift(rng) as usize) % 32).min(v.len() - i);
            v[i..i + n].fill(0);
        }
    }
    v
}

/// A real libjpeg progressive file, shipped as a fixture because THIS CRATE
/// CANNOT PRODUCE ONE with the layout that matters: libjpeg emits DHT per scan,
/// so its leading DC-only scan names an AC table defined only later, whereas our
/// encoder writes all four tables up front.
///
/// Its absence from this corpus is exactly why 80,000 mutations over
/// baseline-only seeds found zero panics while a DC-scan `unwrap` was live in
/// two published releases. A fuzz corpus is only as good as the shapes in it.
const LIBJPEG_PROGRESSIVE: &[u8] = include_bytes!("fixtures/progressive_libjpeg.jpg");

#[test]
fn malformed_input_never_panics() {
    let seeds = [
        seed_jpeg(48, 48, SamplingFactor::R_4_2_0, true),
        seed_jpeg(48, 48, SamplingFactor::R_4_4_4, false),
        // Ragged dimensions exercise the partial-MCU edge paths.
        seed_jpeg(37, 23, SamplingFactor::R_4_2_2, true),
        // Progressive, with a scan layout our own encoder never emits.
        LIBJPEG_PROGRESSIVE.to_vec(),
    ];

    // Small images and a modest count so this stays a CI test, not a fuzz run.
    // `fuzz/` is where depth comes from.
    const ITERS: usize = 1500;
    let (mut ok, mut err) = (0u32, 0u32);

    for (s, src) in seeds.iter().enumerate() {
        for i in 0..ITERS {
            let mut rng =
                0x9E3779B97F4A7C15u64 ^ ((s * ITERS + i) as u64).wrapping_mul(0x0F1B_2C3D);
            xorshift(&mut rng);
            let data = mutate(src, i as u64, &mut rng);

            // Both output paths: they assemble results differently and have
            // diverged before.
            let planar = i % 2 == 0;
            let res = std::panic::catch_unwind(|| {
                let mut d = Decoder::new(&data[..]);
                d.set_single_threaded(true);
                d.set_max_decoding_buffer_size(32 * 1024 * 1024);
                if planar {
                    d.decode_planar().map(|p| p.components.len())
                } else {
                    d.decode().map(|p| p.len())
                }
            });
            match res {
                Ok(Ok(_)) => ok += 1,
                Ok(Err(_)) => err += 1,
                Err(_) => panic!(
                    "PANIC on malformed input: seed={s} case={i} path={} bytes={}",
                    if planar { "planar" } else { "packed" },
                    data.len()
                ),
            }
        }
    }

    // Guard the guard: if every mutation were rejected at the header, this test
    // would pass while exercising almost nothing.
    assert!(
        ok > 100,
        "only {ok} of {} mutations decoded ({err} rejected) - the corpus is not \
         reaching the decoder body, so this test is not testing much",
        seeds.len() * ITERS
    );
}

/// Anything our own encoder emits, our own decoder must read back. This is the
/// shape that would have caught the DHT-ordering defect, where the file declared
/// one set of Huffman tables and coded the scan with another.
#[test]
fn our_encoder_output_always_decodes() {
    for &optimize in &[false, true] {
        for &sampling in &[
            SamplingFactor::R_4_4_4,
            SamplingFactor::R_4_2_2,
            SamplingFactor::R_4_2_0,
        ] {
            for &(w, h) in &[(1usize, 1usize), (8, 8), (17, 9), (64, 64), (127, 65)] {
                let jpg = seed_jpeg(w, h, sampling, optimize);
                let out = Decoder::new(&jpg[..]).decode().unwrap_or_else(|e| {
                    panic!("{w}x{h} optimize={optimize} sampling={sampling:?}: {e}")
                });
                assert_eq!(
                    out.len(),
                    w * h * 3,
                    "{w}x{h} optimize={optimize} sampling={sampling:?}: wrong size"
                );
            }
        }
    }
}
