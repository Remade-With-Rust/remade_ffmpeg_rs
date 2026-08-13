//! Standing content-type gates: every content class × bit depth × channel
//! layout must round-trip EXACTLY through (a) our own decoder and (b) the
//! independent claxon oracle, at every compression level.
//!
//! ffmpeg interop (their decoder on our streams, size parity vs their
//! encoder) is the second half of the gate and lives in
//! `tools/flac_gate.ps1`, since it needs the external binary.

use std::io::Cursor;

/// Deterministic xorshift-ish noise.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    fn i32_in(&mut self, bits: u32) -> i32 {
        let v = (self.next() >> 20) as i64;
        let span = 1i64 << bits;
        ((v % span) - span / 2) as i32
    }
}

/// One channel of a named content class, `n` samples at `bps`.
fn content(class: &str, n: usize, bps: u32, seed: u64) -> Vec<i32> {
    let full = (1i64 << (bps - 1)) as f64 - 1.0;
    let mut rng = Rng(seed);
    match class {
        "silence" => vec![0; n],
        "dc" => vec![(full * 0.25) as i32; n],
        "sine" => (0..n)
            .map(|i| ((i as f64 * 0.037).sin() * full * 0.7) as i32)
            .collect(),
        "sweep" => (0..n)
            .map(|i| {
                let t = i as f64 / n as f64;
                ((i as f64 * (0.002 + 0.25 * t)).sin() * full * 0.6) as i32
            })
            .collect(),
        "noise" => (0..n).map(|_| rng.i32_in(bps)).collect(),
        "quiet-noise" => (0..n)
            .map(|_| rng.i32_in(bps.saturating_sub(9).max(2)))
            .collect(),
        "transients" => (0..n)
            .map(|i| {
                let base = ((i as f64 * 0.01).sin() * full * 0.05) as i32;
                if i % 1723 < 6 {
                    base + ((rng.next() as i32) % (full as i32 / 2))
                } else {
                    base
                }
            })
            .collect(),
        "sixteen-in-container" => {
            // Content quantized coarser than the container (wasted bits):
            // a sine on a coarse grid, always inside the container's range.
            let shift = (bps / 4).min(8);
            let amp = full / (1i64 << shift) as f64 * 0.7;
            (0..n)
                .map(|i| (((i as f64 * 0.05).sin() * amp) as i32) << shift)
                .collect()
        }
        _ => panic!("unknown content class {class}"),
    }
}

fn decode_with_claxon(stream: &[u8]) -> Vec<Vec<i32>> {
    let mut reader = claxon::FlacReader::new(Cursor::new(stream)).expect("claxon parse");
    let ch = reader.streaminfo().channels as usize;
    let mut chans = vec![Vec::new(); ch];
    let mut c = 0usize;
    for s in reader.samples() {
        chans[c].push(s.expect("claxon sample"));
        c = (c + 1) % ch;
    }
    chans
}

#[test]
fn content_type_matrix_roundtrips_everywhere() {
    let classes = [
        "silence",
        "dc",
        "sine",
        "sweep",
        "noise",
        "quiet-noise",
        "transients",
        "sixteen-in-container",
    ];
    let n = 12_000usize; // > 2 blocks + a short tail block
    let mut failures: Vec<String> = Vec::new();

    for &bps in &[8u32, 16, 24] {
        for &channels in &[1u32, 2, 6] {
            for class in classes {
                for &level in &[0u32, 5, 8] {
                    let planes: Vec<Vec<i32>> = (0..channels)
                        .map(|c| content(class, n, bps, 0x1234 + c as u64 * 77))
                        .collect();
                    let mut enc = rusty_flac::Encoder::new(44100, channels, bps).unwrap();
                    enc.set_compression_level(level);
                    let refs: Vec<&[i32]> = planes.iter().map(|p| p.as_slice()).collect();
                    enc.push_planar(&refs).unwrap();
                    let stream = enc.finish();

                    let tag = format!("{class}/{bps}bit/{channels}ch/L{level}");

                    // Our decoder.
                    match rusty_flac::decode(&stream) {
                        Ok((info, chans)) => {
                            if info.bits_per_sample != bps || chans != planes {
                                failures.push(format!("{tag}: own-decoder mismatch"));
                            }
                        }
                        Err(e) => failures.push(format!("{tag}: own-decoder error {e}")),
                    }
                    // Independent oracle.
                    let claxon = decode_with_claxon(&stream);
                    if claxon != planes {
                        failures.push(format!("{tag}: claxon mismatch"));
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} gate failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Odd block-tail sizes: totals that leave 1-sample and prime-length final
/// blocks, plus tiny whole streams.
#[test]
fn awkward_lengths_roundtrip() {
    for &n in &[1usize, 2, 5, 4095, 4096, 4097, 8193] {
        let s: Vec<i32> = (0..n)
            .map(|i| ((i as f64 * 0.21).sin() * 7000.0) as i32)
            .collect();
        let mut enc = rusty_flac::Encoder::new(32000, 1, 16).unwrap();
        enc.push_planar(&[&s]).unwrap();
        let stream = enc.finish();
        let (info, chans) = rusty_flac::decode(&stream).unwrap();
        assert_eq!(info.total_samples, n as u64, "n={n}");
        assert_eq!(chans[0], s, "n={n}");
        assert_eq!(decode_with_claxon(&stream)[0], s, "claxon n={n}");
    }
}

/// Every supported sample rate field survives the STREAMINFO round-trip.
#[test]
fn sample_rates_roundtrip() {
    for &rate in &[8_000u32, 22_050, 44_100, 48_000, 96_000, 192_000] {
        let s: Vec<i32> = (0..5000)
            .map(|i| ((i as f64 * 0.1).sin() * 5000.0) as i32)
            .collect();
        let mut enc = rusty_flac::Encoder::new(rate, 1, 16).unwrap();
        enc.push_planar(&[&s]).unwrap();
        let stream = enc.finish();
        let (info, chans) = rusty_flac::decode(&stream).unwrap();
        assert_eq!(info.sample_rate, rate);
        assert_eq!(chans[0], s);
    }
}
