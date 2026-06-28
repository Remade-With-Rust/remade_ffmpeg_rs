//! The brick registry — the canonical, in-code manifest of every encoder brick.
//!
//! This is the single source of truth that [`docs/mp3-encoder-plan.md`] describes
//! in prose. The slice order **is** the execution order, so [`next_unbuilt`]
//! always points at the brick to lay next. Keeping the manifest in typed code
//! means it can't silently drift from the build the way a separate doc would.

use core::fmt;

/// Which floor of the house a brick belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Shared primitives & tables (N*).
    Foundation,
    /// Time → frequency: analysis filterbank + forward MDCT (L*).
    Analysis,
    /// Frequency → bits: Huffman + side-info + framing (B*).
    Coding,
    /// The dumb-but-valid controller — first decodable MP3 (C*).
    Controller,
    /// The psychoacoustic quality brain (Q*).
    Quality,
    /// Stereo, rate modes, conformance (R*).
    Roof,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Foundation => "Foundation",
            Phase::Analysis => "Analysis",
            Phase::Coding => "Coding",
            Phase::Controller => "Controller",
            Phase::Quality => "Quality",
            Phase::Roof => "Roof",
        }
    }
}

/// How a brick's data/algorithm is obtained (mirrors the plan's classification).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Compute from a formula and assert vs known values.
    Gen,
    /// Transcribe a fixed table and validate.
    Tbl,
    /// Transcribe an algorithm and verify.
    Alg,
    /// Wiring/plumbing, no new math.
    Glue,
    /// Already implemented & tested on the decode side (reuse / invert).
    Done,
}

impl Class {
    pub fn tag(self) -> &'static str {
        match self {
            Class::Gen => "[GEN]",
            Class::Tbl => "[TBL]",
            Class::Alg => "[ALG]",
            Class::Glue => "[GLUE]",
            Class::Done => "[done]",
        }
    }
}

/// How a brick is proven correct — the regime it lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    /// Reuses decode-side data/code already proven bit-exact.
    Reuse,
    /// Round-trips bit-exactly through the matching decoder parser.
    RoundTrip,
    /// Time-domain aliasing cancellation: forward→inverse reconstructs the input.
    Tdac,
    /// Compared to an independent float/reference computation.
    Reference,
    /// End-to-end: decodes cleanly in FFmpeg/LAME (a *legal* stream).
    External,
    /// Quality metric under a threshold (the experimental regime — no bit-exact).
    Metric,
}

impl Verify {
    pub fn name(self) -> &'static str {
        match self {
            Verify::Reuse => "reuse",
            Verify::RoundTrip => "round-trip",
            Verify::Tdac => "tdac",
            Verify::Reference => "reference",
            Verify::External => "external",
            Verify::Metric => "metric",
        }
    }

    /// True for the deterministic, provable bricks; false for the experimental
    /// quality bricks where there is no single right answer.
    pub fn is_conformance(self) -> bool {
        !matches!(self, Verify::Metric)
    }
}

/// Build state of a brick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Not started.
    Todo,
    /// A stub or lab-only variant exists, not wired into the production pipeline.
    Stub,
    /// Implemented in the pipeline, not yet verified.
    Impl,
    /// Implemented and passing its verification gate.
    Verified,
}

impl Status {
    pub fn symbol(self) -> &'static str {
        match self {
            Status::Todo => "·",
            Status::Stub => "◐",
            Status::Impl => "●",
            Status::Verified => "✓",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Status::Todo => "todo",
            Status::Stub => "stub",
            Status::Impl => "impl",
            Status::Verified => "verified",
        }
    }
}

/// Performance posture — where hand-vectorised SIMD earns its keep vs. where safe
/// scalar Rust is fast enough. We won't sacrifice speed for safety on the hot
/// kernels: SIMD/Hybrid bricks get a hand-written path behind a feature flag in an
/// isolated `unsafe` accel boundary (the `rusty_h264-accel` / `rav1e` posture),
/// while the scalar Rust path stays as the default **and** the correctness
/// reference. The lab validates each SIMD path against its scalar twin through the
/// same TDAC/round-trip gate, so acceleration can never silently change output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    /// Safe scalar Rust is sufficient — cold or branchy, no asm warranted.
    Scalar,
    /// Scalar Rust is the default + reference; SIMD optional on the hot inner part.
    Hybrid,
    /// Hot DSP kernel — hand-vectorised SIMD worth it, isolated behind `unsafe`.
    Simd,
}

impl Accel {
    pub fn tag(self) -> &'static str {
        match self {
            Accel::Scalar => "safe",
            Accel::Hybrid => "hybrid",
            Accel::Simd => "SIMD",
        }
    }
}

/// The performance posture of a brick. The hot set is small and explicit; every
/// brick not listed is fast enough in safe scalar Rust.
///
/// * **SIMD** — `L1`/`L2` (the analysis filterbank + forward MDCT cosine kernels)
///   and `Q2` (the psymodel FFT, the single biggest cycle sink) run inner loops
///   of thousands of MACs per granule and are the classic asm hotspots in LAME.
/// * **Hybrid** — `C2`/`Q6` (the rate/distortion loops: iterative + serial, but
///   their per-call requantize vectorises) and `R1` (per-line M/S stereo).
pub fn accel(id: &str) -> Accel {
    match id {
        "L1" | "L2" | "Q2" => Accel::Simd,
        "C2" | "Q6" | "R1" => Accel::Hybrid,
        _ => Accel::Scalar,
    }
}

/// One brick: a primitive of the encoder, tracked from plan to proof.
#[derive(Debug, Clone, Copy)]
pub struct Brick {
    /// Stable id matching the plan doc (`"N4"`, `"L1"`, …). Runnable lab bricks
    /// dispatch on this.
    pub id: &'static str,
    pub phase: Phase,
    pub class: Class,
    pub verify: Verify,
    pub status: Status,
    /// One-line description.
    pub name: &'static str,
}

impl fmt::Display for Brick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {:<3} {:<10} {:<6} {:<10} {:<6} {}",
            self.status.symbol(),
            self.id,
            self.phase.name(),
            self.class.tag(),
            self.verify.name(),
            accel(self.id).tag(),
            self.name,
        )
    }
}

/// The full manifest, in execution order. Edit a `status` here as bricks land;
/// `cargo run --example mp3lab -- bricks` renders it.
pub static BRICKS: &[Brick] = &[
    // ── Foundation ─────────────────────────────────────────────────────────
    b(
        "N1",
        Phase::Foundation,
        Class::Done,
        Verify::Reuse,
        Status::Verified,
        "Scalefactor-band offsets (reused from decode tables)",
    ),
    b(
        "N2",
        Phase::Foundation,
        Class::Done,
        Verify::Reuse,
        Status::Verified,
        "scalefac_compress → (slen1,slen2); PRETAB (reused)",
    ),
    b(
        "N3",
        Phase::Foundation,
        Class::Done,
        Verify::Reuse,
        Status::Verified,
        "Huffman codebooks (reused; value→code lookup in B1)",
    ),
    b(
        "N4",
        Phase::Foundation,
        Class::Gen,
        Verify::Reference,
        Status::Verified,
        "Forward quantizer power law x^(3/4) / x^(4/3) (round-trips on the lattice)",
    ),
    b(
        "N5",
        Phase::Foundation,
        Class::Gen,
        Verify::Tdac,
        Status::Verified,
        "Analysis polyphase window C[512] (realised in L1, TDAC-verified)",
    ),
    b(
        "N6",
        Phase::Foundation,
        Class::Gen,
        Verify::Tdac,
        Status::Verified,
        "Analysis cosine matrix M[32][64] (realised in L1, TDAC-verified)",
    ),
    b(
        "N7",
        Phase::Foundation,
        Class::Gen,
        Verify::Tdac,
        Status::Verified,
        "Forward-MDCT cosine basis + 4 window shapes (realised in L2)",
    ),
    b(
        "N8",
        Phase::Foundation,
        Class::Tbl,
        Verify::Reuse,
        Status::Verified,
        "linbits per Huffman table (reused from decode PAIR_TABLES)",
    ),
    b(
        "N9",
        Phase::Foundation,
        Class::Gen,
        Verify::RoundTrip,
        Status::Verified,
        "Region-boundary ↔ (region0/1_count) maps",
    ),
    // ── Floor 1: analysis ──────────────────────────────────────────────────
    b(
        "L1",
        Phase::Analysis,
        Class::Alg,
        Verify::Tdac,
        Status::Verified,
        "Analysis filterbank: 18 passes, fold 512→64, cosine matrix",
    ),
    b(
        "L2",
        Phase::Analysis,
        Class::Alg,
        Verify::Tdac,
        Status::Verified,
        "Forward MDCT (36-pt long / 3×12-pt short) + overlap",
    ),
    b(
        "L3",
        Phase::Analysis,
        Class::Alg,
        Verify::Tdac,
        Status::Verified,
        "End-to-end analysis round-trip (PCM→freq→PCM identity)",
    ),
    // ── Floor 2: coding ────────────────────────────────────────────────────
    b(
        "B1",
        Phase::Coding,
        Class::Alg,
        Verify::RoundTrip,
        Status::Verified,
        "Huffman encode-table builder (invert codebooks)",
    ),
    b(
        "B2",
        Phase::Coding,
        Class::Alg,
        Verify::RoundTrip,
        Status::Verified,
        "estimate_bits(region, table) — inner-loop cost oracle",
    ),
    b(
        "B3",
        Phase::Coding,
        Class::Alg,
        Verify::RoundTrip,
        Status::Verified,
        "Huffman spectrum encode (big_values + count1 + linbits)",
    ),
    b(
        "B4",
        Phase::Coding,
        Class::Alg,
        Verify::RoundTrip,
        Status::Verified,
        "Region + table selection (minimise B2 cost)",
    ),
    b(
        "B5",
        Phase::Coding,
        Class::Alg,
        Verify::RoundTrip,
        Status::Todo,
        "Side-info serializer (inverse of decode/sideinfo)",
    ),
    b(
        "B6",
        Phase::Coding,
        Class::Alg,
        Verify::RoundTrip,
        Status::Todo,
        "Scalefactor serializer (band-major, scfsi reuse)",
    ),
    b(
        "B7",
        Phase::Coding,
        Class::Glue,
        Verify::External,
        Status::Todo,
        "Frame assembly + CRC-16",
    ),
    b(
        "B8",
        Phase::Coding,
        Class::Alg,
        Verify::RoundTrip,
        Status::Todo,
        "Encoder bit reservoir (main_data_begin bookkeeping)",
    ),
    // ── Floor 3: dumb-but-valid controller ─────────────────────────────────
    b(
        "C1",
        Phase::Controller,
        Class::Alg,
        Verify::Metric,
        Status::Todo,
        "Trivial psymodel: always-long, flat threshold",
    ),
    b(
        "C2",
        Phase::Controller,
        Class::Alg,
        Verify::Metric,
        Status::Todo,
        "Rate-only quantizer (inner loop: search global_gain)",
    ),
    b(
        "C3",
        Phase::Controller,
        Class::Glue,
        Verify::External,
        Status::Todo,
        "Encoder::send_frame / receive_packet plumbing",
    ),
    b(
        "C4",
        Phase::Controller,
        Class::Alg,
        Verify::External,
        Status::Todo,
        "Pipeline gate: playable MP3, FFmpeg-accepted",
    ),
    // ── Floor 4: quality brain ─────────────────────────────────────────────
    b(
        "Q1",
        Phase::Quality,
        Class::Tbl,
        Verify::Reference,
        Status::Todo,
        "Psymodel tables: ATH, critical bands, spreading, FFT windows",
    ),
    b(
        "Q2",
        Phase::Quality,
        Class::Alg,
        Verify::Reference,
        Status::Todo,
        "FFT front-end: long/short spectra, energy + unpredictability",
    ),
    b(
        "Q3",
        Phase::Quality,
        Class::Alg,
        Verify::Metric,
        Status::Todo,
        "Masking threshold + SMR per scalefactor band",
    ),
    b(
        "Q4",
        Phase::Quality,
        Class::Alg,
        Verify::Metric,
        Status::Todo,
        "Perceptual entropy → bit demand (reservoir budgeting)",
    ),
    b(
        "Q5",
        Phase::Quality,
        Class::Alg,
        Verify::Metric,
        Status::Todo,
        "Block-type decision (attack detect, pre-echo control)",
    ),
    b(
        "Q6",
        Phase::Quality,
        Class::Alg,
        Verify::Metric,
        Status::Todo,
        "Outer distortion loop (shape noise under threshold)",
    ),
    // ── Roof: stereo, rate modes, conformance ──────────────────────────────
    b(
        "R1",
        Phase::Roof,
        Class::Alg,
        Verify::RoundTrip,
        Status::Todo,
        "Joint-stereo decision (M/S, intensity)",
    ),
    b(
        "R2",
        Phase::Roof,
        Class::Alg,
        Verify::Metric,
        Status::Todo,
        "Bitrate modes: CBR / ABR / VBR",
    ),
    b(
        "R3",
        Phase::Roof,
        Class::Tbl,
        Verify::External,
        Status::Todo,
        "Xing/LAME info header (VBR TOC + delay/padding)",
    ),
    b(
        "R4",
        Phase::Roof,
        Class::Alg,
        Verify::External,
        Status::Todo,
        "Conformance corpus (round-trip + FFmpeg/LAME + quality)",
    ),
    b(
        "R5",
        Phase::Roof,
        Class::Tbl,
        Verify::RoundTrip,
        Status::Todo,
        "MPEG-2/2.5 LSF tables + framing (deferred)",
    ),
];

/// Const-fn brick constructor, to keep the manifest above terse.
const fn b(
    id: &'static str,
    phase: Phase,
    class: Class,
    verify: Verify,
    status: Status,
    name: &'static str,
) -> Brick {
    Brick {
        id,
        phase,
        class,
        verify,
        status,
        name,
    }
}

/// Look up a brick by its id (`"N4"`).
pub fn by_id(id: &str) -> Option<&'static Brick> {
    BRICKS.iter().find(|b| b.id.eq_ignore_ascii_case(id))
}

/// `[todo, stub, impl, verified]` counts.
pub fn counts() -> [usize; 4] {
    let mut c = [0usize; 4];
    for brick in BRICKS {
        c[match brick.status {
            Status::Todo => 0,
            Status::Stub => 1,
            Status::Impl => 2,
            Status::Verified => 3,
        }] += 1;
    }
    c
}

/// The next brick to lay: first one in execution order not yet built.
pub fn next_unbuilt() -> Option<&'static Brick> {
    BRICKS
        .iter()
        .find(|b| matches!(b.status, Status::Todo | Status::Stub))
}

/// Render the whole manifest as a status table (used by the `mp3lab` CLI).
pub fn table() -> String {
    let mut s = String::new();
    let mut last = None;
    for brick in BRICKS {
        if last != Some(brick.phase) {
            s.push_str(&format!("\n── {} ──\n", brick.phase.name()));
            last = Some(brick.phase);
        }
        s.push_str(&format!("{brick}\n"));
    }
    let [todo, stub, imp, ver] = counts();
    s.push_str(&format!(
        "\n{} bricks — {ver} verified ✓ · {imp} impl ● · {stub} stub ◐ · {todo} todo ·\n",
        BRICKS.len()
    ));
    let simd = BRICKS.iter().filter(|b| accel(b.id) == Accel::Simd).count();
    let hybrid = BRICKS
        .iter()
        .filter(|b| accel(b.id) == Accel::Hybrid)
        .count();
    s.push_str(&format!(
        "accel — {simd} SIMD · {hybrid} hybrid · {} safe scalar Rust\n",
        BRICKS.len() - simd - hybrid
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        for (i, a) in BRICKS.iter().enumerate() {
            for b in &BRICKS[i + 1..] {
                assert_ne!(a.id, b.id, "duplicate brick id {}", a.id);
            }
        }
    }

    #[test]
    fn counts_sum_to_total() {
        assert_eq!(counts().iter().sum::<usize>(), BRICKS.len());
    }

    #[test]
    fn every_phase_is_present() {
        for p in [
            Phase::Foundation,
            Phase::Analysis,
            Phase::Coding,
            Phase::Controller,
            Phase::Quality,
            Phase::Roof,
        ] {
            assert!(
                BRICKS.iter().any(|b| b.phase == p),
                "no bricks for {}",
                p.name()
            );
        }
    }

    #[test]
    fn next_unbuilt_is_first_incomplete() {
        // Foundation + analysis + the Huffman coding bricks are verified; next is B5.
        assert_eq!(next_unbuilt().map(|b| b.id), Some("B5"));
    }

    #[test]
    fn hot_kernels_are_simd_control_stays_scalar() {
        // The transforms + psymodel FFT are the asm hotspots…
        for id in ["L1", "L2", "Q2"] {
            assert_eq!(accel(id), Accel::Simd, "{id} should be SIMD");
        }
        // …while framing/bit-coding stays safe scalar Rust.
        for id in ["B5", "B7", "C3", "R3"] {
            assert_eq!(accel(id), Accel::Scalar, "{id} should be scalar");
        }
    }
}
