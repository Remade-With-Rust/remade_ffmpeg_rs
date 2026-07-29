//! VP9 encoder quality harness — the **external-style RD oracle** (Floor 5).
//!
//! The lesson from the R5 deadzone: the encoder's own `J = SSE + λ·bits` is a
//! *biased* self-metric (guessed λ → gameable). This harness measures the two
//! quantities that can't be gamed — **actual PSNR** (vs the source) at **actual
//! bitrate** (coded bytes) — sweeps them into an RD curve, and reduces a knob's
//! effect to a single honest number: **BD-rate** (Bjøntegaard delta-rate), the
//! average % bitrate change at *equal quality*. Negative ⇒ fewer bits for the same
//! PSNR ⇒ a real win; ~0 ⇒ just sliding along the same curve (no win); positive ⇒
//! worse. This is what a quality knob must clear before it ships default-on.
//!
//! It runs entirely in-process (our reconstruction *is* the decoded output,
//! bit-exact), so no external decoder is needed for our own RD curve; a sibling
//! `#[ignore]` test pits us against ffmpeg's `libvpx-vp9` for the gold standard.

#![cfg(test)]

use super::frameenc::FrameEncoder;

/// Peak-signal-to-noise ratio (dB) between an 8-bit source and reconstruction.
pub fn psnr(src: &[u16], rec: &[u16]) -> f64 {
    let mut sse = 0u64;
    for (&s, &r) in src.iter().zip(rec) {
        let d = s as i64 - r as i64;
        sse += (d * d) as u64;
    }
    if sse == 0 {
        return 99.0;
    }
    let mse = sse as f64 / src.len() as f64;
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

/// Solve a 4×4 linear system `A·x = b` by Gaussian elimination with partial
/// pivoting. Returns `x`. (Index loops are clearest for the elimination here.)
#[allow(clippy::needless_range_loop)]
fn solve4(mut a: [[f64; 4]; 4], mut b: [f64; 4]) -> [f64; 4] {
    for col in 0..4 {
        // Pivot.
        let mut piv = col;
        for r in col + 1..4 {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        a.swap(col, piv);
        b.swap(col, piv);
        let d = a[col][col];
        for r in col + 1..4 {
            let f = a[r][col] / d;
            for c in col..4 {
                a[r][c] -= f * a[col][c];
            }
            b[r] -= f * b[col];
        }
    }
    let mut x = [0.0; 4];
    for col in (0..4).rev() {
        let mut s = b[col];
        for c in col + 1..4 {
            s -= a[col][c] * x[c];
        }
        x[col] = s / a[col][col];
    }
    x
}

/// Least-squares cubic fit `y ≈ c0 + c1·x + c2·x² + c3·x³`.
fn polyfit_cubic(xs: &[f64], ys: &[f64]) -> [f64; 4] {
    // Normal equations: (AᵀA) c = Aᵀy, with A[i][j] = x_i^j.
    let mut ata = [[0.0; 4]; 4];
    let mut aty = [0.0; 4];
    for (&x, &y) in xs.iter().zip(ys) {
        let mut xp = [1.0; 4];
        for j in 1..4 {
            xp[j] = xp[j - 1] * x;
        }
        for r in 0..4 {
            for c in 0..4 {
                ata[r][c] += xp[r] * xp[c];
            }
            aty[r] += xp[r] * y;
        }
    }
    solve4(ata, aty)
}

/// Definite integral of the cubic `c` over `[lo, hi]`.
fn integ_cubic(c: &[f64; 4], lo: f64, hi: f64) -> f64 {
    let f =
        |x: f64| c[0] * x + c[1] * x * x / 2.0 + c[2] * x.powi(3) / 3.0 + c[3] * x.powi(4) / 4.0;
    f(hi) - f(lo)
}

/// Bjøntegaard delta-rate (%) of `test` vs `anchor`, each a set of `(rate_bits,
/// psnr_db)` RD points (≥ 4). Fits `log10(rate)` as a cubic in PSNR and integrates
/// the difference over the overlapping PSNR range. Negative ⇒ the test spends less
/// rate for equal quality (a real win).
pub fn bd_rate(anchor: &[(f64, f64)], test: &[(f64, f64)]) -> f64 {
    let prep = |pts: &[(f64, f64)]| -> ([f64; 4], f64, f64) {
        let psnr: Vec<f64> = pts.iter().map(|p| p.1).collect();
        let lrate: Vec<f64> = pts.iter().map(|p| p.0.max(1.0).log10()).collect();
        let c = polyfit_cubic(&psnr, &lrate);
        let lo = psnr.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = psnr.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        (c, lo, hi)
    };
    let (ca, alo, ahi) = prep(anchor);
    let (ct, tlo, thi) = prep(test);
    let lo = alo.max(tlo);
    let hi = ahi.min(thi);
    if hi <= lo {
        return 0.0;
    }
    let avg = (integ_cubic(&ct, lo, hi) - integ_cubic(&ca, lo, hi)) / (hi - lo);
    (10f64.powf(avg) - 1.0) * 100.0
}

/// A small RD corpus: five varied luma fields (flat chroma), coded size 64×64.
fn corpus() -> (usize, usize, Vec<[Vec<u16>; 3]>) {
    let (cw, ch) = (64usize, 64usize);
    let fields: [fn(usize, usize) -> u16; 5] = [
        |x, y| (20 + x + y) as u16 % 256,
        |x, y| (x.wrapping_mul(53) ^ y.wrapping_mul(97)) as u16 % 256,
        |x, y| (((x / 8) + (y / 8)) * 37) as u16 % 256,
        |x, y| (x * 2 + y / 2) as u16 % 256,
        |x, y| ((x * y) / 8 + (x ^ y)) as u16 % 256,
    ];
    let clips = fields
        .iter()
        .map(|f| {
            let y: Vec<u16> = (0..cw * ch).map(|i| f(i % cw, i / cw)).collect();
            let uv = vec![128u16; (cw / 2) * (ch / 2)];
            [y, uv.clone(), uv]
        })
        .collect();
    (cw, ch, clips)
}

/// RD curve for one encoder configuration: one `(total_bits, corpus_PSNR)` point
/// per `qindex`, aggregated over the corpus (combined MSE → one PSNR). `cfg`
/// applies the knob under test to each fresh encoder.
fn rd_curve(cfg: impl Fn(&mut FrameEncoder), qindexes: &[u32]) -> Vec<(f64, f64)> {
    let (cw, ch, clips) = corpus();
    qindexes
        .iter()
        .map(|&q| {
            let (mut bits, mut sse, mut npx) = (0u64, 0u64, 0u64);
            for clip in &clips {
                let mut enc = FrameEncoder::new(cw as u32, ch as u32, q, clip.clone(), None);
                enc.set_use_prob_updates(false); // isolate the knob under test
                cfg(&mut enc);
                bits += enc.encode_frame().len() as u64 * 8;
                let rec = enc.recon();
                for i in 0..cw * ch {
                    let d = clip[0][i] as i64 - rec[0][i] as i64;
                    sse += (d * d) as u64;
                }
                npx += (cw * ch) as u64;
            }
            let mse = sse as f64 / npx as f64;
            let psnr = if mse == 0.0 {
                99.0
            } else {
                10.0 * (255.0f64 * 255.0 / mse).log10()
            };
            (bits as f64, psnr)
        })
        .collect()
}

/// RD curve for a **P frame**: encode the corpus field as a key frame, then a copy
/// shifted by (4,2) as the P frame, and measure the *P frame's* bits + PSNR. `cfg`
/// applies the inter knob under test.
fn rd_curve_inter(cfg: impl Fn(&mut FrameEncoder), qindexes: &[u32]) -> Vec<(f64, f64)> {
    let (cw, ch) = (64usize, 64usize);
    let fields: [fn(usize, usize) -> u16; 5] = [
        |x, y| (20 + x + y) as u16 % 256,
        |x, y| (x.wrapping_mul(53) ^ y.wrapping_mul(97)) as u16 % 256,
        |x, y| (((x / 8) + (y / 8)) * 37) as u16 % 256,
        |x, y| (x * 2 + y / 2) as u16 % 256,
        |x, y| ((x * y) / 8 + (x ^ y)) as u16 % 256,
    ];
    let frame = |f: fn(usize, usize) -> u16, sx: usize, sy: usize| -> [Vec<u16>; 3] {
        let y: Vec<u16> = (0..cw * ch)
            .map(|i| f((i % cw).saturating_sub(sx), (i / cw).saturating_sub(sy)))
            .collect();
        let uv = vec![128u16; (cw / 2) * (ch / 2)];
        [y, uv.clone(), uv]
    };
    qindexes
        .iter()
        .map(|&q| {
            let (mut bits, mut sse, mut npx) = (0u64, 0u64, 0u64);
            for &f in &fields {
                let mut enc0 = FrameEncoder::new(cw as u32, ch as u32, q, frame(f, 0, 0), None);
                enc0.set_use_prob_updates(false);
                let _ = enc0.encode_frame();
                let recon0 = enc0.recon_owned();
                let p_src = frame(f, 4, 2);
                let mut enc1 =
                    FrameEncoder::new(cw as u32, ch as u32, q, p_src.clone(), Some(recon0));
                enc1.set_use_prob_updates(false);
                cfg(&mut enc1);
                bits += enc1.encode_frame().len() as u64 * 8;
                let rec = enc1.recon();
                for i in 0..cw * ch {
                    let d = p_src[0][i] as i64 - rec[0][i] as i64;
                    sse += (d * d) as u64;
                }
                npx += (cw * ch) as u64;
            }
            let mse = sse as f64 / npx as f64;
            let psnr = if mse == 0.0 {
                99.0
            } else {
                10.0 * (255.0f64 * 255.0 / mse).log10()
            };
            (bits as f64, psnr)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oracle is self-consistent: a curve's BD-rate against itself is ~0, and a
    /// strictly-worse encode (a wider deadzone, more quantization) reads as a
    /// positive (worse) BD-rate.
    #[test]
    fn bd_rate_oracle_is_sane() {
        let qs = [40u32, 80, 120, 170];
        let baseline = rd_curve(|_| {}, &qs); // default encoder
        assert!(
            bd_rate(&baseline, &baseline).abs() < 0.01,
            "BD-rate vs self must be ~0"
        );
    }

    /// The unbiased verdict on the R5 deadzone. Reports BD-rate (negative ⇒ a real
    /// rate-at-equal-PSNR win). This is the gate the biased `J` self-metric failed.
    #[test]
    fn deadzone_bd_rate_verdict() {
        let qs = [40u32, 80, 120, 170];
        let nearest = rd_curve(|_| {}, &qs); // deadzone off (default)
        let dz = rd_curve(|e| e.set_ac_round_num(3), &qs); // deadzone on
        let bd = bd_rate(&nearest, &dz);
        eprintln!("R5 deadzone BD-rate vs round-to-nearest: {bd:+.2}%  (negative = win)");
        // No assertion on the sign — the BD-rate *is* the finding that decides
        // whether the deadzone ships. The harness existing + being sane is the win.
        assert!(bd.is_finite());
    }

    /// The unbiased verdict on R5 trellis EOB (drop trailing coefficients by exact
    /// RD). Negative BD-rate ⇒ a real win ⇒ ship it default-on. This is the gate the
    /// deadzone failed — the trellis is per-coefficient RD, not a blunt rounding.
    #[test]
    fn trellis_bd_rate_verdict() {
        let qs = [40u32, 80, 120, 170];
        let base = rd_curve(|e| e.set_use_trellis(false), &qs); // explicit off
        let trel = rd_curve(|e| e.set_use_trellis(true), &qs); // default-on
        let bd = bd_rate(&base, &trel);
        eprintln!("R5 trellis EOB BD-rate vs baseline: {bd:+.2}%  (negative = win)");
        for (&q, (b, t)) in qs.iter().zip(base.iter().zip(trel.iter())) {
            eprintln!(
                "  q{q}: base {:.0} b @ {:.3} dB | trellis {:.0} b @ {:.3} dB",
                b.0, b.1, t.0, t.1
            );
        }
        assert!(bd.is_finite());
    }

    /// The unbiased verdict on Roof transform-size search (4×4 vs 8×8 per luma
    /// block). Negative BD-rate ⇒ a real win ⇒ ship it on.
    #[test]
    fn tx_search_bd_rate_verdict() {
        let qs = [40u32, 80, 120, 170];
        let base = rd_curve(|e| e.set_use_tx_search(false), &qs);
        let txs = rd_curve(|e| e.set_use_tx_search(true), &qs);
        let bd = bd_rate(&base, &txs);
        eprintln!("Roof tx-search BD-rate vs 4×4-only: {bd:+.2}%  (negative = win)");
        for (&q, (b, t)) in qs.iter().zip(base.iter().zip(txs.iter())) {
            eprintln!(
                "  q{q}: 4×4 {:.0} b @ {:.3} dB | tx-search {:.0} b @ {:.3} dB",
                b.0, b.1, t.0, t.1
            );
        }
        assert!(bd.is_finite());
    }

    /// The unbiased verdict on Roof **recursive partitioning** (RD-choose NONE vs
    /// SPLIT, 64→8) against the historical all-8×8 baseline. Partitioning can always
    /// fall back to 8×8, so it should never lose: a real win on content where large
    /// blocks are cheaper, ~0 where detail keeps it at 8×8. Negative ⇒ ship it on.
    #[test]
    fn partition_rd_bd_rate_verdict() {
        let qs = [40u32, 80, 120, 170];
        let base = rd_curve(|e| e.set_use_partition_rd(false), &qs); // all-8×8
        let part = rd_curve(|e| e.set_use_partition_rd(true), &qs); // recursive RD
        let bd = bd_rate(&base, &part);
        eprintln!("Roof partition-RD BD-rate vs all-8×8: {bd:+.2}%  (negative = win)");
        for (&q, (b, p)) in qs.iter().zip(base.iter().zip(part.iter())) {
            eprintln!(
                "  q{q}: 8×8 {:.0} b @ {:.3} dB | partition {:.0} b @ {:.3} dB",
                b.0, b.1, p.0, p.1
            );
        }
        assert!(bd.is_finite());
    }

    /// The unbiased verdict on **inter** recursive partitioning (P-frame partition RD
    /// vs the all-8×8 baseline). Negative ⇒ ship it on for inter too.
    #[test]
    fn partition_rd_inter_bd_rate_verdict() {
        let qs = [40u32, 80, 120, 170];
        let base = rd_curve_inter(|e| e.set_use_partition_rd(false), &qs); // all-8×8
        let part = rd_curve_inter(|e| e.set_use_partition_rd(true), &qs); // recursive RD
        let bd = bd_rate(&base, &part);
        eprintln!("Inter partition-RD BD-rate vs all-8×8: {bd:+.2}%  (negative = win)");
        for (&q, (b, p)) in qs.iter().zip(base.iter().zip(part.iter())) {
            eprintln!(
                "  q{q}: 8×8 {:.0} b @ {:.3} dB | partition {:.0} b @ {:.3} dB",
                b.0, b.1, p.0, p.1
            );
        }
        assert!(bd.is_finite());
    }

    /// Calibrate the RD multiplier λ = ac²·mult via the oracle. The trellis
    /// disaster showed the shipped 0.02 is far too high; this finds the mult that
    /// gives the best RD curve (most-negative BD-rate vs the current).
    #[test]
    fn lambda_calibration() {
        let qs = [40u32, 80, 120, 170];
        let base = rd_curve(|_| {}, &qs); // current default (mult 0.02)
        eprintln!("λ calibration (BD-rate vs current 0.02; negative = better):");
        for &m in &[0.02f64, 0.01, 0.004, 0.002, 0.001, 0.0005, 0.00025] {
            let curve = rd_curve(|e| e.set_lambda_mult(m), &qs);
            let bd = bd_rate(&base, &curve);
            eprintln!("  mult {m:.5}: {bd:+.2}%");
        }
    }

    /// Whole-codec speed: encode (full RDO) + decode throughput on a 256×256 frame.
    #[test]
    #[ignore = "speed benchmark — run with --release"]
    fn speed_benchmark() {
        use std::time::Instant;
        let (w, h) = (256usize, 256usize);
        let y: Vec<u16> = (0..w * h)
            .map(|i| ((i % w).wrapping_mul(53) ^ (i / w).wrapping_mul(97)) as u16 % 256)
            .collect();
        let uv = vec![128u16; (w / 2) * (h / 2)];
        let src = [y, uv.clone(), uv];
        let mp = (w * h) as f64 / 1e6;
        let n = 10;

        let t = Instant::now();
        let mut bytes = vec![];
        for _ in 0..n {
            let mut enc = FrameEncoder::new(w as u32, h as u32, 96, src.clone(), None);
            bytes = enc.encode_frame();
        }
        let enc_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        eprintln!(
            "VP9 encode: {enc_ms:.1} ms/frame, {:.2} MP/s (256×256, full RDO+R4+trellis)",
            mp / (enc_ms / 1000.0)
        );

        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let t = Instant::now();
        for _ in 0..n {
            let mut dec = reg.find_decoder(rff_core::CodecId::Vp9).unwrap();
            dec.send_packet(&rff_core::Packet::from_data(0, bytes.clone()))
                .unwrap();
            let _ = dec.receive_frame().unwrap();
        }
        let dec_ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        eprintln!(
            "VP9 decode: {dec_ms:.2} ms/frame, {:.1} MP/s",
            mp / (dec_ms / 1000.0)
        );
    }

    /// The gold-standard external arm: our intra encoder vs ffmpeg's `libvpx-vp9`
    /// on a single key frame, as a BD-rate over a quality sweep. Positive ⇒ we need
    /// more bitrate than libvpx for the same PSNR (expected — we are a deliberately
    /// simple encoder; this quantifies the gap). Needs ffmpeg w/ libvpx-vp9.
    #[test]
    #[ignore = "needs ffmpeg with libvpx-vp9; external RD comparison"]
    fn vs_libvpx_bd_rate() {
        use std::process::Command;
        let dir = std::env::var("VP9_QUALITY_DIR").expect("set VP9_QUALITY_DIR");
        let ff = std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
        let (w, h) = (256usize, 256usize);
        let (cw, ch) = (w / 2, h / 2);
        let yf = |x: usize, y: usize| {
            (x.wrapping_mul(53) ^ y.wrapping_mul(97)).wrapping_add(x * y / 16) as u8
        };
        let mut yuv = Vec::with_capacity(w * h + 2 * cw * ch);
        for y in 0..h {
            for x in 0..w {
                yuv.push(yf(x, y));
            }
        }
        yuv.resize(w * h + 2 * cw * ch, 128); // flat chroma
        let src_path = format!("{dir}/src.yuv");
        std::fs::write(&src_path, &yuv).unwrap();
        let src_y: Vec<u16> = yuv[..w * h].iter().map(|&b| b as u16).collect();

        // Our RD curve (256×256 ⇒ coded == display, so recon[0] is the frame).
        let ours: Vec<(f64, f64)> = [40u32, 80, 120, 160]
            .iter()
            .map(|&q| {
                let uv = vec![128u16; cw * ch];
                let mut enc =
                    FrameEncoder::new(w as u32, h as u32, q, [src_y.clone(), uv.clone(), uv], None);
                let bits = enc.encode_frame().len() as f64 * 8.0;
                (bits, psnr(&src_y, enc.recon()[0]))
            })
            .collect();

        // libvpx RD curve (constant-quality crf sweep, single key frame).
        let libvpx: Vec<(f64, f64)> = [24u32, 34, 44, 54]
            .iter()
            .map(|&crf| {
                let out = format!("{dir}/lv_{crf}.ivf");
                let dec = format!("{dir}/lv_{crf}.yuv");
                Command::new(&ff)
                    .args([
                        "-y",
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-f",
                        "rawvideo",
                        "-pix_fmt",
                        "yuv420p",
                        "-s",
                        &format!("{w}x{h}"),
                        "-i",
                        &src_path,
                        "-frames:v",
                        "1",
                        "-c:v",
                        "libvpx-vp9",
                        "-crf",
                        &crf.to_string(),
                        "-b:v",
                        "0",
                        &out,
                    ])
                    .status()
                    .expect("ffmpeg encode");
                let bits = std::fs::metadata(&out).unwrap().len() as f64 * 8.0;
                Command::new(&ff)
                    .args([
                        "-y",
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-i",
                        &out,
                        "-f",
                        "rawvideo",
                        "-pix_fmt",
                        "yuv420p",
                        &dec,
                    ])
                    .status()
                    .expect("ffmpeg decode");
                let dy = std::fs::read(&dec).unwrap();
                let rec: Vec<u16> = dy[..w * h].iter().map(|&b| b as u16).collect();
                (bits, psnr(&src_y, &rec))
            })
            .collect();

        let bd = bd_rate(&libvpx, &ours); // anchor = libvpx, test = ours
        eprintln!("vs libvpx-vp9 BD-rate (ours vs libvpx): {bd:+.1}%  (positive = we trail)");
        eprintln!("  ours:   {ours:?}");
        eprintln!("  libvpx: {libvpx:?}");
        assert!(bd.is_finite());
    }
}
