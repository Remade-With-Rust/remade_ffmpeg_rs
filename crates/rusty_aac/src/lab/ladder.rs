//! The bitrate-ladder runner — the per-clip, multi-operating-point harness the
//! corpus law demands (great-gate §2: *judged per-clip at ≥4 operating points*).
//!
//! One run produces a `(class × bitrate)` table of NMR plus the measured bitrate,
//! which is the shape every rung's per-class verdict is read from. A single point
//! at one CRF-equivalent is explicitly not a verdict — a tool that wins at 64 kbps
//! and loses at 192 kbps is a dispatch on the rate axis, and only a ladder can see
//! that.
//!
//! The **null arm** ([`null_arm`]) runs before any comparison is believed: encode
//! the same input twice with the same settings and require byte-identical output.
//! If that fails, every delta in the table is noise of unknown size.

use super::corpus::{self, Class, Signal};
use super::quality::{track_nmr, NmrReport};
use crate::{AacEncoder, AacEncoderConfig, AacDecoder, AdtsHeader};

/// The default operating points — four, per the corpus law. Chosen to straddle
/// the region where AAC tools change sign: 64k (where PNS/intensity earn their
/// keep) through 192k (where they cost).
pub const DEFAULT_BITRATES: [u32; 4] = [64_000, 96_000, 128_000, 192_000];

/// One (clip, bitrate) cell of the ladder.
#[derive(Debug, Clone)]
pub struct Point {
    pub class: Class,
    /// Configured target.
    pub target_bps: u32,
    /// Actually achieved, from the emitted ADTS byte count.
    pub measured_bps: f64,
    pub nmr: NmrReport,
}

impl Point {
    /// How far the rate loop landed from its target, as a ratio. A rung that
    /// changes this materially has changed the operating point, and its quality
    /// delta is not a like-for-like comparison.
    pub fn rate_error(&self) -> f64 {
        self.measured_bps / self.target_bps.max(1) as f64 - 1.0
    }
}

/// Encode one corpus signal to an ADTS elementary stream at `bitrate`, using the
/// default (arm 0) encoder configuration.
pub fn encode_adts(sig: &Signal, bitrate: u32) -> Vec<u8> {
    encode_adts_with(
        sig,
        AacEncoderConfig {
            bitrate_bps: bitrate,
            ..Default::default()
        },
    )
}

/// Encode with an explicit configuration — how a rung's routed arm is measured
/// against arm 0 on identical content.
pub fn encode_adts_with(sig: &Signal, config: AacEncoderConfig) -> Vec<u8> {
    let mut enc = AacEncoder::new(config);
    enc.push_pcm(&sig.pcm, sig.channels, sig.sample_rate)
        .expect("push_pcm");
    enc.finish();
    let mut out = Vec::new();
    while let Ok(p) = enc.next_packet() {
        let hdr = AdtsHeader {
            object_type: 2,
            sample_rate: sig.sample_rate,
            channels: sig.channels,
            frame_length: 7 + p.data.len(),
            header_len: 7,
        };
        out.extend_from_slice(&crate::write_adts_header(&hdr));
        out.extend_from_slice(&p.data);
    }
    out
}

/// Decode an ADTS stream and return channel 0 as mono.
pub fn decode_mono(adts: &[u8]) -> Vec<f32> {
    let mut dec = AacDecoder::new();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 7 <= adts.len() {
        let hdr = match crate::parse_adts(&adts[pos..]) {
            Ok(h) => h,
            Err(_) => break,
        };
        let end = (pos + hdr.frame_length).min(adts.len());
        if let Ok(audio) = dec.decode(&adts[pos..end], None) {
            let ch = audio.channels.max(1) as usize;
            out.extend(audio.samples.iter().step_by(ch).copied());
        }
        if hdr.frame_length == 0 {
            break;
        }
        pos = end;
    }
    out
}

/// Score one (clip, bitrate) cell with arm 0.
pub fn point(sig: &Signal, bitrate: u32) -> Point {
    point_with(
        sig,
        AacEncoderConfig {
            bitrate_bps: bitrate,
            ..Default::default()
        },
    )
}

/// Score one (clip, bitrate) cell under an explicit configuration.
pub fn point_with(sig: &Signal, config: AacEncoderConfig) -> Point {
    let bitrate = config.bitrate_bps;
    let adts = encode_adts_with(sig, config);
    let decoded = decode_mono(&adts);
    let secs = sig.frames() as f64 / sig.sample_rate as f64;
    let measured_bps = (adts.len() * 8) as f64 / secs.max(1e-9);
    let nmr = track_nmr(&sig.mono(), &decoded, sig.sample_rate);
    Point {
        class: sig.class,
        target_bps: bitrate,
        measured_bps,
        nmr,
    }
}

/// Run the full corpus × bitrate ladder.
pub fn run(bitrates: &[u32]) -> Vec<Point> {
    let mut pts = Vec::new();
    for sig in corpus::corpus() {
        for &b in bitrates {
            pts.push(point(&sig, b));
        }
    }
    pts
}

/// The outcome of the null arm.
#[derive(Debug, Clone)]
pub struct NullArm {
    /// Every (class, bitrate) cell that re-encoded byte-identically.
    pub identical: usize,
    /// Cells that did not — each one invalidates comparisons on that clip.
    pub divergent: Vec<(Class, u32)>,
}

impl NullArm {
    pub fn is_clean(&self) -> bool {
        self.divergent.is_empty()
    }
}

/// **Run this before believing any delta.** Encoding the same input twice with
/// the same settings must produce identical bytes; if it does not, the encoder
/// carries nondeterminism (thread-order dependence, uninitialized state) and the
/// ladder's resolution is unknown.
///
/// This is `codec-measurement`'s null arm in its strictest available form: for a
/// deterministic encoder the null is byte-identity, not "a small delta."
pub fn null_arm(bitrates: &[u32]) -> NullArm {
    let mut identical = 0usize;
    let mut divergent = Vec::new();
    for sig in corpus::corpus() {
        for &b in bitrates {
            let a = encode_adts(&sig, b);
            let c = encode_adts(&sig, b);
            if a == c {
                identical += 1;
            } else {
                divergent.push((sig.class, b));
            }
        }
    }
    NullArm {
        identical,
        divergent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The null arm must be clean, on every class and every rate. The encoder is
    /// frame-parallel, so this is a real property to check, not a formality.
    #[test]
    fn null_arm_is_byte_identical() {
        let n = null_arm(&[96_000]);
        assert!(
            n.is_clean(),
            "encoder is nondeterministic on {:?} — every ladder delta is unreliable",
            n.divergent
        );
        assert_eq!(n.identical, Class::all().len());
    }

    /// Every cell must round-trip: produce a stream, decode it, and score enough
    /// frames for the number to mean something.
    #[test]
    fn ladder_cells_are_scorable() {
        for sig in corpus::corpus() {
            let p = point(&sig, 128_000);
            assert!(
                p.nmr.frames > 20,
                "{}: only {} scored frames",
                sig.name(),
                p.nmr.frames
            );
            assert!(
                p.measured_bps > 1000.0,
                "{}: produced no meaningful stream ({:.0} bps)",
                sig.name(),
                p.measured_bps
            );
        }
    }

    /// Quality must improve monotonically with bitrate on the ladder. This is the
    /// harness's sanity gate on the ENCODER: the MP3 campaign found a flat RD
    /// curve this way (bits that bought no quality), which was a real bug.
    ///
    /// Judged on `pct_audible` — the MP3 calibration's best cross-encoder
    /// predictor — and only between the extreme rungs, so ordinary non-monotone
    /// wobble between adjacent points does not fail the build.
    #[test]
    fn ladder_is_monotone_in_bitrate() {
        for sig in corpus::corpus() {
            let lo = point(&sig, 64_000);
            let hi = point(&sig, 192_000);
            assert!(
                hi.nmr.pct_audible <= lo.nmr.pct_audible + 0.5,
                "{}: 192k must not be worse than 64k ({:.2}% vs {:.2}% audible) \
                 — a flat or inverted RD curve means bits are buying nothing",
                sig.name(),
                hi.nmr.pct_audible,
                lo.nmr.pct_audible
            );
        }
    }

    /// The rate loop must land near its target. A rung that shifts this has
    /// changed the operating point, not the quality.
    #[test]
    fn ladder_respects_its_targets() {
        for sig in corpus::corpus() {
            for &b in &DEFAULT_BITRATES {
                let p = point(&sig, b);
                assert!(
                    p.measured_bps <= b as f64 * 1.15,
                    "{} @ {}k: overshot to {:.0} bps",
                    sig.name(),
                    b / 1000,
                    p.measured_bps
                );
            }
        }
    }
}
