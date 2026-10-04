//! ISO 11172-4 / 13818-4 Layer III conformance vectors, scored against their
//! reference PCM.
//!
//! The vectors are not vendored (their redistribution terms are ISO's, not ours).
//! Fetch them with `scripts/fetch-mp3-vectors.sh`, then:
//!
//! ```text
//!   MP3_ISO_VECTORS=/path/to/minimp3/vectors cargo test -p rusty_mp3 --release --test iso_vectors
//! ```
//!
//! Without the variable the test prints that it was skipped and passes, so it
//! never reads as coverage it did not provide.
//!
//! Pass criterion: every output sample within 1 LSB (s16) of the reference, at the
//! best whole-granule alignment -- decoders legitimately differ in how many
//! undecodable leading frames they emit, and nothing else.

use std::path::Path;

use rusty_mp3::Mp3Decoder;

/// Streams that must decode exactly. Each entry names the path it guards, so a
/// regression reports WHAT broke, not just which file.
const MUST_PASS: &[(&str, &str)] = &[
    (
        "l3-compl",
        "MPEG-1 compliance: every Huffman table, long blocks",
    ),
    ("l3-he_32khz", "32 kHz long/short"),
    ("l3-he_44khz", "44.1 kHz long/short"),
    ("l3-he_48khz", "48 kHz long/short"),
    ("l3-hecommon", "stereo, common side info"),
    (
        "l3-he_mode",
        "MPEG-1 INTENSITY stereo, mode switching, dual mono",
    ),
    ("l3-si", "scfsi, preflag, scalefac_scale"),
    (
        "l3-si_block",
        "MIXED blocks, incl. the mixed flag on Start/Stop",
    ),
    ("l3-si_huff", "Huffman table selection"),
    ("M2L3_bitrate_16_all", "MPEG-2 16 kHz, every bitrate"),
    ("M2L3_bitrate_22_all", "MPEG-2 22.05 kHz, every bitrate"),
    ("M2L3_bitrate_24_all", "MPEG-2 24 kHz, every bitrate"),
    ("M2L3_compl24", "MPEG-2 compliance"),
    ("M2L3_noise", "MPEG-2 noise, intensity stereo"),
    (
        "l3-test45",
        "MPEG-2 INTENSITY: 5-bit positions, illegal positions, top-band rule",
    ),
    ("l3-test46", "MPEG-2 intensity + M/S"),
];

fn decode(bytes: &[u8]) -> (Vec<i32>, usize) {
    let mut dec = Mp3Decoder::new();
    dec.push(bytes);
    dec.flush();
    let (mut out, mut ch) = (Vec::new(), 1usize);
    while let Ok(f) = dec.next_frame() {
        ch = f.channels.max(1) as usize;
        out.extend(
            f.samples
                .iter()
                .map(|s| (s * 32768.0).round().clamp(-32768.0, 32767.0) as i32),
        );
    }
    (out, ch)
}

/// Max |error| at the best whole-granule alignment.
fn max_error(ours: &[i32], refs: &[i32], ch: usize) -> i32 {
    let g = 576 * ch as i64;
    let mut best = i32::MAX;
    for k in -8i64..=8 {
        let off = k * g;
        let (a0, b0) = if off >= 0 {
            (off as usize, 0)
        } else {
            (0, (-off) as usize)
        };
        if a0 >= ours.len() || b0 >= refs.len() {
            continue;
        }
        let n = (ours.len() - a0).min(refs.len() - b0);
        let m = (0..n)
            .map(|i| (ours[a0 + i] - refs[b0 + i]).abs())
            .max()
            .unwrap_or(0);
        best = best.min(m);
    }
    best
}

#[test]
fn iso_layer3_vectors_decode_within_one_lsb() {
    let Ok(dir) = std::env::var("MP3_ISO_VECTORS") else {
        eprintln!("SKIPPED: set MP3_ISO_VECTORS to the vectors dir (scripts/fetch-mp3-vectors.sh)");
        return;
    };
    let dir = Path::new(&dir);
    let mut failures = Vec::new();
    for (stem, guards) in MUST_PASS {
        let bit = std::fs::read(dir.join(format!("{stem}.bit")))
            .unwrap_or_else(|e| panic!("{stem}.bit: {e}"));
        let pcm = std::fs::read(dir.join(format!("{stem}.pcm")))
            .unwrap_or_else(|e| panic!("{stem}.pcm: {e}"));
        let refs: Vec<i32> = pcm
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as i32)
            .collect();
        let (ours, ch) = decode(&bit);
        let e = max_error(&ours, &refs, ch);
        eprintln!("{stem:<22} max|e| {e:>5} LSB   ({guards})");
        if e > 1 {
            failures.push(format!("{stem}: max|e| {e} LSB -- guards {guards}"));
        }
    }
    assert!(
        failures.is_empty(),
        "ISO vectors out of tolerance:\n  {}",
        failures.join("\n  ")
    );
}
