//! `wasmcheck` — decode a bundled MP3 and print a hash of the PCM.
//!
//! The point is that this runs IDENTICALLY on the host and inside a wasm runtime,
//! so "does the codec work on wasm" stops being a compile check and becomes a
//! bit-exactness comparison: same bitstream in, same FNV-1a of the decoded samples
//! out, or the port is wrong.
//!
//! ```text
//!   cargo run -p rusty_mp3 --release --example wasmcheck            # host
//!   cargo build -p rusty_mp3 --release --target wasm32-wasip1 \
//!       --example wasmcheck
//!   wasmtime target/wasm32-wasip1/release/examples/wasmcheck.wasm
//! ```
//!
//! The stream is embedded with `include_bytes!` rather than read from a path so
//! the wasm run needs no filesystem capability -- a browser build has none, and
//! the hash must not depend on how the bytes arrived.
//!
//! Encoding is exercised too: a wasm build that can only decode is half a port,
//! and the encoder is where the float-heavy work lives.

use rusty_mp3::{Mp3Decoder, Mp3Encoder, Mp3EncoderConfig};

/// FNV-1a over the raw sample bits — the same gate `decprof` prints, so the two
/// numbers are directly comparable.
fn fnv1a_f32(h: &mut u64, samples: &[f32]) {
    for s in samples {
        for b in s.to_bits().to_le_bytes() {
            *h ^= b as u64;
            *h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

fn fnv1a_bytes(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// A short deterministic signal — no file I/O, no RNG, identical on every target.
fn tone(n: usize, rate: u32) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = i as f32 / rate as f32;
            0.4 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                + 0.2 * (2.0 * std::f32::consts::PI * 1109.0 * t).sin()
        })
        .collect()
}

fn main() {
    // ---- decode ----------------------------------------------------------
    let mp3: &[u8] = include_bytes!("wasmcheck_fixture.mp3");
    let mut dec = Mp3Decoder::new();
    dec.push(mp3);
    dec.flush();
    let (mut dh, mut frames, mut samples, mut rate, mut ch) = (0xcbf2_9ce4_8422_2325u64, 0, 0, 0, 0);
    while let Ok(f) = dec.next_frame() {
        frames += 1;
        ch = f.channels;
        rate = f.sample_rate;
        samples += f.samples.len() / f.channels.max(1) as usize;
        fnv1a_f32(&mut dh, &f.samples);
    }
    println!("decode: {frames} frames, {samples} samples/ch @ {rate} Hz, {ch} ch");
    println!("decode fnv1a: {dh:#018x}");

    // ---- encode ----------------------------------------------------------
    let sr = 44100u32;
    let pcm = tone(sr as usize / 2, sr); // 0.5 s
    let mut enc = Mp3Encoder::new(Mp3EncoderConfig {
        bitrate_kbps: 192,
        vbr_quality: None,
    });
    enc.push_pcm_f32(&pcm, 1, sr).expect("push pcm");
    enc.finish();
    let mut out = Vec::new();
    while let Ok(p) = enc.next_packet() {
        out.extend_from_slice(&p);
    }
    let mut eh = 0xcbf2_9ce4_8422_2325u64;
    fnv1a_bytes(&mut eh, &out);
    println!("encode: {} bytes", out.len());
    println!("encode fnv1a: {eh:#018x}");

    // ---- round trip ------------------------------------------------------
    // Decoding our own output exercises both halves against each other, which is
    // the check that catches a port where each half is self-consistently wrong.
    let mut rt = Mp3Decoder::new();
    rt.push(&out);
    rt.flush();
    let mut n = 0usize;
    let mut peak = 0f32;
    while let Ok(f) = rt.next_frame() {
        n += f.samples.len();
        for s in &f.samples {
            peak = peak.max(s.abs());
        }
    }
    println!("roundtrip: {n} samples, peak {peak:.4}");
    println!("OK");
}
