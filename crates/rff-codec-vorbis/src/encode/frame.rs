//! Vorbis audio-packet assembly.
//!
//! Inverts lewton's decode path step for step: window + forward MDCT → a fitted floor-1
//! spectral envelope ([`super::floor`], brick 3) → forward channel coupling → rate-distortion
//! residue-2 partition/classify/cascade-VQ (brick 4) → the packet bitstream. Brick 5 adds the
//! perceptual (masking-driven) bit allocation on top of the `LAMBDA` / floor-scale knobs.

use rff_core::{Error, Result};

use super::mdct::{apply_window, mdct_forward, vorbis_window};
use super::setup::{Codebook, Floor, Mapping, Residue, SetupTables};
use super::{floor, psy, BitWriter};

/// vorbis `ilog`.
fn ilog(v: u32) -> u32 {
    32 - v.leading_zeros()
}

/// Forward channel coupling: given the two per-channel residues `(m_out, a_out)` that
/// the decoder must reconstruct, return the encoded `(m, a)`. Exact inverse of lewton's
/// `inverse_couple` (verified in tests).
fn forward_couple(m_out: f32, a_out: f32) -> (f32, f32) {
    if m_out > 0.0 {
        if a_out < m_out {
            (m_out, m_out - a_out)
        } else {
            (a_out, m_out - a_out)
        }
    } else if a_out > m_out {
        (m_out, a_out - m_out)
    } else {
        (a_out, a_out - m_out)
    }
}

/// Write the codeword for entry `e` of `book`. Errors if the entry is unused (length 0).
fn write_entry(bw: &mut BitWriter, book: &Codebook, e: u32) -> Result<()> {
    let (cw, len) = book.encode(e);
    if len == 0 {
        return Err(Error::invalid("vorbis encode: tried to emit an unused codebook entry"));
    }
    bw.write(cw, len as u32);
    Ok(())
}

/// VQ-encode one partition segment with a book across one cascade pass: for each `dim`-wide
/// chunk, pick the (rate-distortion) best entry, optionally write its codeword, and subtract
/// the reconstructed vector (so later passes refine the residual — the decode ADDs). Returns
/// the number of codeword bits (written or simulated).
fn vq_pass(seg: &mut [f32], book: &Codebook, lambda: f32, mut bw: Option<&mut BitWriter>) -> Result<u32> {
    let dim = book.dimensions as usize;
    let vq = book.vq.as_ref().expect("residue book must have a VQ lookup");
    let mut bits = 0u32;
    let mut i = 0;
    while i + dim <= seg.len() {
        let e = book.quantize_vector(&seg[i..i + dim], lambda) as usize;
        bits += book.lengths[e] as u32;
        if let Some(bw) = bw.as_deref_mut() {
            write_entry(bw, book, e as u32)?;
        }
        for d in 0..dim {
            seg[i + d] -= vq[e * dim + d];
        }
        i += dim;
    }
    Ok(bits)
}

/// Distortion (residual energy) and bit cost of coding `seg` with class `c`'s full cascade.
/// `work` is a caller-owned scratch buffer (reused across the classify trials to avoid a
/// per-trial allocation); its prior contents are overwritten.
fn cascade_cost(
    seg: &[f32],
    resid: &Residue,
    codebooks: &[Codebook],
    c: usize,
    lambda: f32,
    work: &mut Vec<f32>,
) -> Result<(f32, u32)> {
    work.clear();
    work.extend_from_slice(seg);
    let mut bits = 0u32;
    for pass in 0..8 {
        let book = resid.books[c][pass];
        if book >= 0 {
            bits += vq_pass(work, &codebooks[book as usize], lambda, None)?;
        }
    }
    Ok((work.iter().map(|x| x * x).sum(), bits))
}

/// Encode a residue-2 submap: interleave the channel vectors, partition, classify each
/// partition by rate-distortion, then emit classifications + cascade VQ in lewton's exact order.
fn encode_residue2(
    bw: &mut BitWriter,
    resid: &Residue,
    codebooks: &[Codebook],
    channels: &[Vec<f32>],
    m: usize,
    lambda: f32,
) -> Result<()> {
    let ch = channels.len();
    // Interleave: v[ch*i + j] = channel j at bin i.
    let mut v = vec![0.0f32; ch * m];
    for (j, chan) in channels.iter().enumerate() {
        for i in 0..m {
            v[ch * i + j] = chan[i];
        }
    }

    let begin = (resid.begin as usize).min(v.len());
    let end = (resid.end as usize).min(v.len());
    let psize = resid.partition_size as usize;
    let n_to_read = end - begin;
    let partitions = n_to_read / psize;
    if partitions == 0 {
        return Ok(());
    }
    let classbook = &codebooks[resid.classbook as usize];
    let cpw = classbook.dimensions as usize;
    let nclasses = resid.classifications as u32;

    // 1. Classify each partition by rate-distortion: min (distortion + λ·bits).
    #[cfg(test)]
    let _tc = std::time::Instant::now();
    let mut classes = vec![0u8; partitions];
    let mut work = Vec::with_capacity(psize);
    for (p, cl) in classes.iter_mut().enumerate() {
        let seg = &v[begin + p * psize..begin + (p + 1) * psize];
        let mut best_c = 0usize;
        let mut best_cost = f32::INFINITY;
        for c in 0..resid.classifications as usize {
            let (dist, bits) = cascade_cost(seg, resid, codebooks, c, lambda, &mut work)?;
            let cost = dist + lambda * bits as f32;
            if cost < best_cost {
                best_cost = cost;
                best_c = c;
            }
        }
        *cl = best_c as u8;
    }
    #[cfg(test)]
    prof::CLASSIFY_NS.fetch_add(_tc.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);

    // 2. Emit — exactly mirroring residue_packet_decode_inner's pass/partition order.
    #[cfg(test)]
    let _te = std::time::Instant::now();
    let mut work = v.clone();
    for pass in 0..8 {
        let mut pc = 0;
        while pc < partitions {
            if pass == 0 {
                // One classbook word covers `cpw` partitions (base-`nclasses` digits,
                // most-significant first). Pad past the end with class 0.
                let mut entry = 0u32;
                for i in 0..cpw {
                    let c = if pc + i < partitions { classes[pc + i] as u32 } else { 0 };
                    entry = entry * nclasses + c;
                }
                write_entry(bw, classbook, entry)?;
            }
            for _ in 0..cpw {
                if pc >= partitions {
                    break;
                }
                let c = classes[pc] as usize;
                let book = resid.books[c][pass];
                if book >= 0 {
                    let seg = &mut work[begin + pc * psize..begin + (pc + 1) * psize];
                    vq_pass(seg, &codebooks[book as usize], lambda, Some(bw))?;
                }
                pc += 1;
            }
        }
    }
    #[cfg(test)]
    prof::EMIT_NS.fetch_add(_te.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// Test-only phase counters for profiling the residue coder (the encode hot path).
#[cfg(test)]
pub(crate) mod prof {
    use std::sync::atomic::AtomicU64;
    pub static MDCT_NS: AtomicU64 = AtomicU64::new(0);
    pub static FLOOR_NS: AtomicU64 = AtomicU64::new(0);
    pub static CLASSIFY_NS: AtomicU64 = AtomicU64::new(0);
    pub static EMIT_NS: AtomicU64 = AtomicU64::new(0);
}

/// Encode one long block (mode 1, n=2048) for all channels into a Vorbis audio packet.
/// `ch_blocks[c]` is the raw (un-windowed) 2048-sample block for channel `c`. `quality` in
/// [0, 1] drives the masking threshold + residue rate-distortion `lambda`.
pub fn encode_long_packet(
    setup: &SetupTables,
    ch_blocks: &[Vec<f32>],
    sample_rate: u32,
    quality: f32,
) -> Result<Vec<u8>> {
    let mode_idx = setup
        .modes
        .iter()
        .position(|m| m.blockflag)
        .ok_or_else(|| Error::invalid("vorbis encode: no long-block mode"))?;
    let mode = &setup.modes[mode_idx];
    let mapping: &Mapping = &setup.mappings[mode.mapping as usize];
    let n = ch_blocks[0].len();
    let m = n / 2;
    let window = vorbis_window(n);
    let channels = ch_blocks.len();

    // Window + forward MDCT per channel.
    #[cfg(test)]
    let _tm = std::time::Instant::now();
    let mut spectra = Vec::with_capacity(channels);
    for block in ch_blocks {
        let mut w = block.clone();
        apply_window(&mut w, &window);
        spectra.push(mdct_forward(&w));
    }
    #[cfg(test)]
    prof::MDCT_NS.fetch_add(_tm.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);

    // Bitstream header.
    let mut bw = BitWriter::new();
    bw.write(0, 1); // audio packet
    let mode_bits = ilog(setup.modes.len() as u32 - 1);
    bw.write(mode_idx as u32, mode_bits);
    if mode.blockflag {
        bw.write(1, 1); // previous window flag (all-long)
        bw.write(1, 1); // next window flag
    }

    // Per-channel floor-1: fit the floor to the masking threshold, emit its bits, then
    // residue = spectrum / floor curve.
    #[cfg(test)]
    let _tf = std::time::Instant::now();
    let mut residue = vec![vec![0.0f32; m]; channels];
    for c in 0..channels {
        let submap = mapping.mux[c] as usize;
        let floor_cfg = &setup.floors[mapping.submap_floors[submap] as usize];
        let Floor::One(fl) = floor_cfg else {
            return Err(Error::invalid("vorbis encode: expected floor type 1"));
        };
        let target = psy::masking_curve(&spectra[c], sample_rate, quality);
        let curve = floor::fit_and_encode_floor(&mut bw, &target, fl, &setup.codebooks, m)?;
        for i in 0..m {
            residue[c][i] = if curve[i] > 0.0 {
                spectra[c][i] / curve[i]
            } else {
                0.0
            };
        }
    }
    #[cfg(test)]
    prof::FLOOR_NS.fetch_add(_tf.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);

    // Forward channel coupling (in order; decode inverse-couples in reverse).
    for &(mag, angle) in &mapping.coupling {
        let (mag, angle) = (mag as usize, angle as usize);
        // Borrow the two distinct channel rows simultaneously (mag != angle per spec).
        let (left, right) = residue.split_at_mut(mag.max(angle));
        let (m_row, a_row) = if mag < angle {
            (&mut left[mag], &mut right[0])
        } else {
            (&mut right[0], &mut left[angle])
        };
        for (mv, av) in m_row.iter_mut().zip(a_row.iter_mut()) {
            let (nm, na) = forward_couple(*mv, *av);
            *mv = nm;
            *av = na;
        }
    }

    // Point stereo: collapse the coupling angle above the perceptual cutoff (high-frequency
    // stereo is poorly localized). Saves bits on wide stereo at lower `-q`; a no-op for
    // correlated stereo (the angle is already ~0) and for high `-q` (cutoff ≥ Nyquist).
    let point_bin = psy::point_stereo_bin(quality, m, sample_rate);
    if point_bin < m {
        for &(_, angle) in &mapping.coupling {
            for a in residue[angle as usize][point_bin..].iter_mut() {
                *a = 0.0;
            }
        }
    }

    // Residue per submap (q4 has a single submap covering all channels).
    let resid = &setup.residues[mapping.submap_residues[0] as usize];
    let lambda = psy::lambda_for_quality(quality);
    encode_residue2(&mut bw, resid, &setup.codebooks, &residue, m, lambda)?;

    Ok(bw.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::super::setup::{parse_setup, SETUP_Q4_STEREO};
    use super::super::{write_comment_header, write_ident_header, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL};
    use super::*;

    // --- Minimal Ogg pager (test-only, for the ffmpeg cross-decoder check) ---

    fn ogg_crc(data: &[u8]) -> u32 {
        // Ogg's CRC-32: poly 0x04c11db7, init 0, no reflection, no final xor.
        let mut crc: u32 = 0;
        for &b in data {
            crc ^= (b as u32) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04c1_1db7
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    fn ogg_page(serial: u32, seq: u32, granule: i64, htype: u8, packet: &[u8]) -> Vec<u8> {
        let mut segs = Vec::new();
        let mut l = packet.len();
        while l >= 255 {
            segs.push(255u8);
            l -= 255;
        }
        segs.push(l as u8);
        let mut page = Vec::new();
        page.extend_from_slice(b"OggS");
        page.push(0); // version
        page.push(htype);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&serial.to_le_bytes());
        page.extend_from_slice(&seq.to_le_bytes());
        page.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
        page.push(segs.len() as u8);
        page.extend_from_slice(&segs);
        page.extend_from_slice(packet);
        let crc = ogg_crc(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        page
    }

    /// Build a complete `.ogg` from the 3 headers + audio packets (one packet per page).
    fn build_ogg(headers: &[Vec<u8>], audio: &[Vec<u8>]) -> Vec<u8> {
        let serial = 0xC0FFEE;
        let mut out = Vec::new();
        let mut seq = 0u32;
        // ident on its own BOS page, then comment + setup.
        out.extend(ogg_page(serial, seq, 0, 0x02, &headers[0]));
        seq += 1;
        out.extend(ogg_page(serial, seq, 0, 0x00, &headers[1]));
        seq += 1;
        out.extend(ogg_page(serial, seq, 0, 0x00, &headers[2]));
        seq += 1;
        let mut granule: i64 = 0;
        for (i, pkt) in audio.iter().enumerate() {
            let last = i + 1 == audio.len();
            granule += 1024; // long-block hop
            let htype = if last { 0x04 } else { 0x00 };
            out.extend(ogg_page(serial, seq, granule, htype, pkt));
            seq += 1;
        }
        out
    }

    /// Opt-in: writes a `.ogg` of our encoder's output to `$VORBIS_OGG_OUT` for external
    /// (ffmpeg) validation. Ignored by default (no external dependency in CI).
    #[test]
    #[ignore]
    fn emit_ogg_for_ffmpeg() {
        let Ok(path) = std::env::var("VORBIS_OGG_OUT") else {
            return;
        };
        let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        let headers = vec![
            write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL),
            write_comment_header("remade_ffmpeg_rs", &[]),
            SETUP_Q4_STEREO.to_vec(),
        ];
        let n = 2048usize;
        let hop = n / 2;
        let total = n * 20;
        let signal: Vec<Vec<f32>> = (0..2)
            .map(|ch| {
                (0..total)
                    .map(|i| {
                        let f = if ch == 0 { 0.02 } else { 0.023 };
                        0.5 * (f * i as f32).sin()
                    })
                    .collect()
            })
            .collect();
        let mut audio = Vec::new();
        let mut pos = 0;
        while pos + n <= total {
            let blocks: Vec<Vec<f32>> = (0..2).map(|ch| signal[ch][pos..pos + n].to_vec()).collect();
            audio.push(encode_long_packet(&setup, &blocks, 44_100, 0.5).unwrap());
            pos += hop;
        }
        let ogg = build_ogg(&headers, &audio);
        std::fs::write(&path, &ogg).unwrap();
        eprintln!("wrote {} bytes to {path}", ogg.len());
    }

    /// Best normalized cross-correlation of `got` against `reference` over lags `0..max_lag`.
    fn best_correlation(reference: &[f32], got: &[f32], max_lag: usize) -> f32 {
        let mut best = 0.0f32;
        for lag in 0..max_lag {
            if lag + got.len() > reference.len() {
                break;
            }
            let mut dot = 0.0f32;
            let mut er = 0.0f32;
            let mut eg = 0.0f32;
            for (k, &g) in got.iter().enumerate() {
                let r = reference[lag + k];
                dot += r * g;
                er += r * r;
                eg += g * g;
            }
            if er > 0.0 && eg > 0.0 {
                let c = dot / (er.sqrt() * eg.sqrt());
                if c > best {
                    best = c;
                }
            }
        }
        best
    }

    /// Opt-in: profile the residue coder — classify vs emit vs the rest.
    #[test]
    #[ignore]
    fn profile_residue() {
        use std::sync::atomic::Ordering::Relaxed;
        let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        let n = 2048usize;
        let hop = n / 2;
        let total = n * 80;
        let sig: Vec<Vec<f32>> = (0..2)
            .map(|ch| {
                (0..total)
                    .map(|i| {
                        let t = i as f32;
                        let mut s = 0.0f32;
                        for k in 1..64 {
                            s += 0.5 / (k as f32).sqrt()
                                * (0.02 * k as f32 * t + ch as f32 + 1.3 * k as f32).sin();
                        }
                        s * 0.1
                    })
                    .collect()
            })
            .collect();
        prof::MDCT_NS.store(0, Relaxed);
        prof::FLOOR_NS.store(0, Relaxed);
        prof::CLASSIFY_NS.store(0, Relaxed);
        prof::EMIT_NS.store(0, Relaxed);
        let t0 = std::time::Instant::now();
        let (mut pos, mut nblk) = (0usize, 0usize);
        while pos + n <= total {
            let blocks: Vec<Vec<f32>> = (0..2).map(|c| sig[c][pos..pos + n].to_vec()).collect();
            encode_long_packet(&setup, &blocks, 44_100, 0.6).unwrap();
            pos += hop;
            nblk += 1;
        }
        let tot = t0.elapsed().as_secs_f64();
        let md = prof::MDCT_NS.load(Relaxed) as f64 / 1e9;
        let fl = prof::FLOOR_NS.load(Relaxed) as f64 / 1e9;
        let cl = prof::CLASSIFY_NS.load(Relaxed) as f64 / 1e9;
        let em = prof::EMIT_NS.load(Relaxed) as f64 / 1e9;
        eprintln!(
            "PROFILE {nblk} blocks {tot:.3}s | mdct {md:.3}s ({:.0}%) | floor+psy {fl:.3}s ({:.0}%) | classify {cl:.3}s ({:.0}%) | emit {em:.3}s ({:.0}%)",
            100.0 * md / tot,
            100.0 * fl / tot,
            100.0 * cl / tot,
            100.0 * em / tot
        );
    }

    #[test]
    fn forward_couple_inverts_lewton() {
        // lewton's inverse_couple, verbatim.
        fn inverse_couple(m: f32, a: f32) -> (f32, f32) {
            if m > 0.0 {
                if a > 0.0 { (m, m - a) } else { (m + a, m) }
            } else if a > 0.0 {
                (m, m + a)
            } else {
                (m - a, m)
            }
        }
        for &m in &[-3.0f32, -1.0, 0.0, 2.0, 5.0] {
            for &a in &[-4.0f32, -2.0, 0.0, 1.0, 6.0] {
                let (cm, ca) = forward_couple(m, a);
                let (rm, ra) = inverse_couple(cm, ca);
                assert!((rm - m).abs() < 1e-4 && (ra - a).abs() < 1e-4, "m={m} a={a}");
            }
        }
    }

    /// Encode long-block packets from a test tone, decode them with lewton, and confirm the
    /// output resembles the input (correlation, not bit-exact).
    #[test]
    fn packets_decode_in_lewton() {
        let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        let ident_bytes = write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
        let comment_bytes = write_comment_header("rff", &[]);
        let setup_bytes = SETUP_Q4_STEREO;
        let l_ident = lewton::header::read_header_ident(&ident_bytes).unwrap();
        let _l_comment = lewton::header::read_header_comment(&comment_bytes).unwrap();
        let l_setup =
            lewton::header::read_header_setup(setup_bytes, 2, (BS0_LOG2, BS1_LOG2)).unwrap();

        let n = 2048usize;
        let hop = n / 2;
        let total = n * 6;
        // A steady stereo tone (both channels a low-frequency sine).
        let signal: Vec<Vec<f32>> = (0..2)
            .map(|ch| {
                (0..total)
                    .map(|i| {
                        let f = if ch == 0 { 0.02 } else { 0.023 };
                        0.5 * (f * i as f32).sin()
                    })
                    .collect()
            })
            .collect();

        let mut pwr = lewton::audio::PreviousWindowRight::new();
        let mut decoded: Vec<Vec<f32>> = vec![Vec::new(), Vec::new()];
        let mut pos = 0;
        while pos + n <= total {
            let blocks: Vec<Vec<f32>> = (0..2).map(|ch| signal[ch][pos..pos + n].to_vec()).collect();
            let packet = encode_long_packet(&setup, &blocks, 44_100, 0.5).unwrap();
            let pcm = lewton::audio::read_audio_packet(&l_ident, &l_setup, &packet, &mut pwr)
                .expect("lewton decodes our packet");
            if !pcm.is_empty() && !pcm[0].is_empty() {
                for (ch, chan) in pcm.iter().enumerate() {
                    decoded[ch].extend(chan.iter().map(|&s| s as f32 / 32768.0));
                }
            }
            pos += hop;
        }

        assert!(!decoded[0].is_empty(), "no audio decoded");
        let got = &decoded[0];
        let out_energy: f32 = got.iter().map(|x| x * x).sum::<f32>() / got.len() as f32;
        assert!(out_energy > 1e-4, "decoded audio is basically silent: {out_energy}");
        assert!(out_energy < 1.0, "decoded audio blew up: {out_energy}");

        let best = best_correlation(&signal[0], got, 2 * n);
        eprintln!("CORR ch0 best normalized correlation = {best:.4}");
        assert!(best > 0.8, "decoded audio does not resemble the input (corr={best:.4})");
    }

    /// Brick 3: on a multi-tone signal the fitted floor should reconstruct with high
    /// correlation (a flat floor smears multi-formant spectra).
    #[test]
    fn fitted_floor_reconstructs_multitone() {
        let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        let ident_bytes = write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
        let l_ident = lewton::header::read_header_ident(&ident_bytes).unwrap();
        let l_setup =
            lewton::header::read_header_setup(SETUP_Q4_STEREO, 2, (BS0_LOG2, BS1_LOG2)).unwrap();

        let n = 2048usize;
        let hop = n / 2;
        let total = n * 8;
        // Rich signal: several partials at different amplitudes (a shaped spectrum).
        let signal: Vec<Vec<f32>> = (0..2)
            .map(|_| {
                (0..total)
                    .map(|i| {
                        let t = i as f32;
                        0.4 * (0.03 * t).sin()
                            + 0.25 * (0.08 * t).sin()
                            + 0.15 * (0.17 * t).sin()
                            + 0.08 * (0.31 * t).cos()
                    })
                    .collect()
            })
            .collect();

        let mut pwr = lewton::audio::PreviousWindowRight::new();
        let mut decoded: Vec<f32> = Vec::new();
        let mut pos = 0;
        while pos + n <= total {
            let blocks: Vec<Vec<f32>> = (0..2).map(|ch| signal[ch][pos..pos + n].to_vec()).collect();
            let packet = encode_long_packet(&setup, &blocks, 44_100, 0.5).unwrap();
            let pcm = lewton::audio::read_audio_packet(&l_ident, &l_setup, &packet, &mut pwr)
                .expect("lewton decodes our packet");
            if !pcm.is_empty() && !pcm[0].is_empty() {
                decoded.extend(pcm[0].iter().map(|&s| s as f32 / 32768.0));
            }
            pos += hop;
        }
        assert!(!decoded.is_empty());
        let best = best_correlation(&signal[0], &decoded, 2 * n);
        eprintln!("CORR multitone best correlation = {best:.4}");
        assert!(best > 0.9, "fitted floor multitone reconstruction poor (corr={best:.4})");
    }

    /// Average audio-packet size (bytes) for a stereo signal at a given quality.
    fn avg_packet_bytes(setup: &SetupTables, sig: &[Vec<f32>], q: f32) -> usize {
        let n = 2048usize;
        let hop = n / 2;
        let total = sig[0].len();
        let (mut bytes, mut pkts, mut pos) = (0usize, 0usize, 0usize);
        while pos + n <= total {
            let blocks: Vec<Vec<f32>> = (0..sig.len()).map(|c| sig[c][pos..pos + n].to_vec()).collect();
            bytes += encode_long_packet(setup, &blocks, 44_100, q).unwrap().len();
            pkts += 1;
            pos += hop;
        }
        bytes / pkts.max(1)
    }

    /// Coupling must compress correlated stereo well below decorrelated stereo (the angle
    /// channel vanishes for L=R); point stereo must be gated to low bitrate.
    #[test]
    fn coupling_compresses_correlated_stereo() {
        let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        let n = 2048usize;
        let total = n * 6;
        let broadband = |seed: f32| -> Vec<f32> {
            (0..total)
                .map(|i| {
                    let t = i as f32;
                    let mut s = 0.0f32;
                    for k in 1..120 {
                        s += 0.5 / (k as f32).sqrt()
                            * (0.02 * k as f32 * t + seed * k as f32 * (k as f32 + 1.0)).sin();
                    }
                    s * 0.1
                })
                .collect()
        };
        let l = broadband(1.3);
        let r = broadband(2.7);
        let cor = avg_packet_bytes(&setup, &[l.clone(), l.clone()], 0.5);
        let dec = avg_packet_bytes(&setup, &[l, r], 0.5);
        assert!(
            dec as f32 > cor as f32 * 1.2,
            "coupling should compress correlated stereo: correlated={cor} decorrelated={dec}"
        );
        // Point stereo is a low-bitrate lever: on below q0.55, off (full stereo) above.
        assert!(psy::point_stereo_bin(0.5, 1024, 44_100) < 1024);
        assert_eq!(psy::point_stereo_bin(0.7, 1024, 44_100), 1024);
    }

    /// Minimal WAV reader (PCM s16) → (sample_rate, channels, interleaved f32).
    fn read_wav(path: &str) -> (u32, usize, Vec<f32>) {
        let d = std::fs::read(path).unwrap();
        let rate = u32::from_le_bytes([d[24], d[25], d[26], d[27]]);
        let channels = u16::from_le_bytes([d[22], d[23]]) as usize;
        // Find the "data" chunk.
        let mut i = 12;
        while &d[i..i + 4] != b"data" {
            let sz = u32::from_le_bytes([d[i + 4], d[i + 5], d[i + 6], d[i + 7]]) as usize;
            i += 8 + sz;
        }
        let sz = u32::from_le_bytes([d[i + 4], d[i + 5], d[i + 6], d[i + 7]]) as usize;
        let pcm = &d[i + 8..i + 8 + sz];
        let samples = (0..pcm.len() / 2)
            .map(|k| i16::from_le_bytes([pcm[2 * k], pcm[2 * k + 1]]) as f32 / 32768.0)
            .collect();
        (rate, channels, samples)
    }

    /// Opt-in: encode real audio from `$VORBIS_WAV_IN` at several `-q`, reporting our bitrate
    /// and reconstruction correlation for a side-by-side with ffmpeg's libvorbis.
    #[test]
    #[ignore]
    fn compare_real_audio() {
        let Ok(path) = std::env::var("VORBIS_WAV_IN") else {
            return;
        };
        let (rate, channels, inter) = read_wav(&path);
        assert_eq!(channels, 2, "test expects stereo");
        let n = 2048usize;
        let hop = n / 2;
        let frames = inter.len() / channels;
        let dur = frames.min(rate as usize * 4); // first ~4 s
        let sig: Vec<Vec<f32>> = (0..2)
            .map(|c| (0..dur).map(|i| inter[i * channels + c]).collect())
            .collect();
        let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        let ident = write_ident_header(2, rate, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
        let l_ident = lewton::header::read_header_ident(&ident).unwrap();
        let l_setup =
            lewton::header::read_header_setup(SETUP_Q4_STEREO, 2, (BS0_LOG2, BS1_LOG2)).unwrap();
        for &q in &[0.3f32, 0.5, 0.7, 0.9] {
            let mut pwr = lewton::audio::PreviousWindowRight::new();
            let mut decoded: Vec<f32> = Vec::new();
            let (mut bytes, mut pkts, mut pos) = (0usize, 0usize, 0usize);
            while pos + n <= dur {
                let blocks: Vec<Vec<f32>> =
                    (0..2).map(|c| sig[c][pos..pos + n].to_vec()).collect();
                let packet = encode_long_packet(&setup, &blocks, rate, q).unwrap();
                bytes += packet.len();
                pkts += 1;
                let pcm =
                    lewton::audio::read_audio_packet(&l_ident, &l_setup, &packet, &mut pwr).unwrap();
                if !pcm.is_empty() && !pcm[0].is_empty() {
                    decoded.extend(pcm[0].iter().map(|&s| s as f32 / 32768.0));
                }
                pos += hop;
            }
            let corr = best_correlation(&sig[0], &decoded, 2 * n);
            let kbps = (bytes as f32 / pkts as f32) * (rate as f32 / hop as f32) * 8.0 / 1000.0;
            eprintln!("REAL q={q:.1}  ~{kbps:.0} kb/s  corr={corr:.4}");
        }
    }

    /// Opt-in: `-q` sweep — bitrate must rise monotonically with quality, and correlation
    /// improve. Demonstrates the psychoacoustic `-q` knob.
    #[test]
    #[ignore]
    fn quality_sweep() {
        let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        let ident_bytes = write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
        let l_ident = lewton::header::read_header_ident(&ident_bytes).unwrap();
        let l_setup =
            lewton::header::read_header_setup(SETUP_Q4_STEREO, 2, (BS0_LOG2, BS1_LOG2)).unwrap();
        let n = 2048usize;
        let hop = n / 2;
        let total = n * 8;
        let signal: Vec<Vec<f32>> = (0..2)
            .map(|_| {
                (0..total)
                    .map(|i| {
                        let t = i as f32;
                        let mut s = 0.0f32;
                        for k in 1..65 {
                            s += 0.5 / (k as f32).sqrt()
                                * (0.022 * k as f32 * t + 1.3 * k as f32 * (k as f32 + 1.0)).sin();
                        }
                        s * 0.12
                    })
                    .collect()
            })
            .collect();
        for &q in &[0.1f32, 0.3, 0.5, 0.7, 0.9] {
            let mut pwr = lewton::audio::PreviousWindowRight::new();
            let mut decoded: Vec<f32> = Vec::new();
            let mut bytes = 0usize;
            let mut pkts = 0usize;
            let mut pos = 0;
            while pos + n <= total {
                let blocks: Vec<Vec<f32>> =
                    (0..2).map(|ch| signal[ch][pos..pos + n].to_vec()).collect();
                let packet = encode_long_packet(&setup, &blocks, 44_100, q).unwrap();
                bytes += packet.len();
                pkts += 1;
                let pcm =
                    lewton::audio::read_audio_packet(&l_ident, &l_setup, &packet, &mut pwr).unwrap();
                if !pcm.is_empty() && !pcm[0].is_empty() {
                    decoded.extend(pcm[0].iter().map(|&s| s as f32 / 32768.0));
                }
                pos += hop;
            }
            let corr = best_correlation(&signal[0], &decoded, 2 * n);
            let kbps = (bytes as f32 / pkts as f32) * (44100.0 / 1024.0) * 8.0 / 1000.0;
            eprintln!("QSWEEP q={q:.1}  {} B/pkt  ~{kbps:.0} kb/s  corr={corr:.4}", bytes / pkts);
        }
    }

    /// A dense broadband spectrum (many partials, shaped envelope) — the case a flat floor
    /// smears and a fitted floor should track. Reports SNR so floor tuning is visible.
    #[test]
    fn fitted_floor_reconstructs_broadband() {
        let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        let ident_bytes = write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
        let l_ident = lewton::header::read_header_ident(&ident_bytes).unwrap();
        let l_setup =
            lewton::header::read_header_setup(SETUP_Q4_STEREO, 2, (BS0_LOG2, BS1_LOG2)).unwrap();

        let n = 2048usize;
        let hop = n / 2;
        let total = n * 8;
        // A dense broadband spectrum: 64 partials across the band with a 1/√k envelope and
        // decorrelating phases — many partitions carry medium content (stresses the residue).
        let signal: Vec<Vec<f32>> = (0..2)
            .map(|_| {
                (0..total)
                    .map(|i| {
                        let t = i as f32;
                        let mut s = 0.0f32;
                        for k in 1..65 {
                            let amp = 0.5 / (k as f32).sqrt();
                            let phase = 1.3 * k as f32 * (k as f32 + 1.0);
                            s += amp * (0.022 * k as f32 * t + phase).sin();
                        }
                        s * 0.12
                    })
                    .collect()
            })
            .collect();

        let mut pwr = lewton::audio::PreviousWindowRight::new();
        let mut decoded: Vec<f32> = Vec::new();
        let mut pos = 0;
        while pos + n <= total {
            let blocks: Vec<Vec<f32>> = (0..2).map(|ch| signal[ch][pos..pos + n].to_vec()).collect();
            let packet = encode_long_packet(&setup, &blocks, 44_100, 0.5).unwrap();
            let pcm = lewton::audio::read_audio_packet(&l_ident, &l_setup, &packet, &mut pwr)
                .expect("lewton decodes our packet");
            if !pcm.is_empty() && !pcm[0].is_empty() {
                decoded.extend(pcm[0].iter().map(|&s| s as f32 / 32768.0));
            }
            pos += hop;
        }
        assert!(!decoded.is_empty());
        let best = best_correlation(&signal[0], &decoded, 2 * n);
        eprintln!("CORR broadband = {best:.4}");
        assert!(best > 0.85, "fitted floor broadband reconstruction poor (corr={best:.4})");
    }
}
