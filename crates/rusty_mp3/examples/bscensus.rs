//! `bscensus` — a BITSTREAM census: what did the encoder actually decide?
//!
//! ```text
//!   cargo run -p rusty_mp3 --release --example bscensus -- a.mp3 b.mp3 ...
//! ```
//!
//! Reads any MPEG-1/2/2.5 Layer III stream — ours, LAME's, anyone's — and counts
//! the per-granule decisions that the side information and scalefactors record.
//! It runs the real side-info parser and the real scalefactor decoder, so the
//! numbers are what a decoder sees, not what an encoder claims.
//!
//! Two claims about our encoder are countable here, and both are load-bearing for
//! the ~0.7-1.1 ODG gap to LAME:
//!
//! * **"we pick one global gain per granule where LAME shapes noise per band"** —
//!   count granules whose scalefactors are entirely zero (`flat`). A flat granule
//!   applies one gain to all 22 bands; a shaped one does not.
//! * **"the block type that exists to control pre-echo is the one with the
//!   weakest model behind it"** — count the block mix, so "we barely fire short
//!   blocks" stops being an impression.
//!
//! Deterministic: one run, no pinning, no noise floor. See `codec-measurement`
//! §15 — for an effect this structural the counter is the evidence.

use rusty_mp3::decode::{reservoir::Reservoir, scalefactors, sideinfo};
use rusty_mp3::frame::{BlockType, SideInfo};
use rusty_mp3::header::FrameHeader;

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

#[derive(Default)]
struct Census {
    frames: usize,
    /// Granule/channel units carrying main data (the Xing/Info frame has none).
    units: usize,
    // Block mix.
    long: usize,
    start: usize,
    stop: usize,
    short: usize,
    mixed: usize,
    // Shaping.
    flat: usize,
    preflag: usize,
    scalefac_scale: usize,
    scfsi_units: usize,
    sbg_nonzero: usize,
    /// Sum of non-zero scalefactor entries, and of (max - min) spread, over
    /// units that are NOT flat — so the mean describes shaped granules.
    nz_sum: usize,
    spread_sum: usize,
    // Rate/gain.
    gain_sum: usize,
    gain_min: u8,
    gain_max: u8,
    part23_sum: usize,
    /// Histogram of per-granule main-data use, in eighths of the nominal
    /// per-granule share. Whether unspent payload is spread thinly over every
    /// granule or concentrated in a few near-empty ones decides whether the fix
    /// is a finer quantizer knob or a bit-reservoir that reclaims from silence.
    part23_hist: [usize; 9],
    /// Nominal bits available per granule-channel, from the frame geometry.
    nominal: usize,
    /// Granules that spent MORE than their nominal share, and by how much in
    /// total -- i.e. the reservoir actually lending banked bits to hard granules.
    over_nominal: usize,
    over_bits: usize,
    /// Main-data bits the stream physically carries (frame minus header, CRC and
    /// side info). Summed over the file, `part23_sum / avail_sum` is the share of
    /// the paid-for payload the encoder actually spent on audio; the remainder is
    /// stuffing. A coarse rate knob cannot land on the budget exactly, so this is
    /// where a too-coarse quantizer leaks bits.
    avail_sum: usize,
    bytes: usize,
    secs: f64,
}

impl Census {
    fn new() -> Census {
        Census {
            gain_min: u8::MAX,
            ..Default::default()
        }
    }

    /// Fold one granule/channel's decisions in.
    fn unit(&mut self, gi: &rusty_mp3::frame::GranuleSideInfo, sf: &scalefactors::ScaleFactors) {
        self.units += 1;

        let is_short = gi.window_switching && gi.block_type == BlockType::Short;
        match (is_short, gi.mixed_block, gi.block_type) {
            (true, true, _) => self.mixed += 1,
            (true, false, _) => self.short += 1,
            (_, _, BlockType::Start) => self.start += 1,
            (_, _, BlockType::Stop) => self.stop += 1,
            _ => self.long += 1,
        }

        // Scalefactor shaping. A granule whose scalefactors are all zero applies a
        // single global gain across every band -- no per-band noise shaping. The
        // short grid is per-window, so fold all three windows in.
        let mut vals: Vec<u8> = Vec::with_capacity(64);
        if is_short || gi.block_type == BlockType::Short {
            for w in 0..3 {
                vals.extend_from_slice(&sf.short[w]);
            }
            if gi.mixed_block {
                vals.extend_from_slice(&sf.long[..8]);
            }
        } else {
            vals.extend_from_slice(&sf.long);
        }
        let nz = vals.iter().filter(|&&v| v != 0).count();
        let hi = vals.iter().copied().max().unwrap_or(0);
        let lo = vals.iter().copied().min().unwrap_or(0);
        if nz == 0 && !gi.preflag {
            self.flat += 1;
        } else {
            self.nz_sum += nz;
            self.spread_sum += (hi - lo) as usize;
        }

        if gi.preflag {
            self.preflag += 1;
        }
        if gi.scalefac_scale {
            self.scalefac_scale += 1;
        }
        if gi.subblock_gain.iter().any(|&g| g != 0) {
            self.sbg_nonzero += 1;
        }

        if self.nominal > 0 {
            // Bucket 8 is "at or ABOVE the nominal share" -- a granule can only
            // land there by borrowing banked bits through the reservoir, so its
            // share is the direct measure of whether donation is happening.
            let eighths = gi.part2_3_length as usize * 8 / self.nominal;
            self.part23_hist[eighths.min(8)] += 1;
            if gi.part2_3_length as usize > self.nominal {
                self.over_nominal += 1;
                self.over_bits += gi.part2_3_length as usize - self.nominal;
            }
        }
        self.gain_sum += gi.global_gain as usize;
        self.gain_min = self.gain_min.min(gi.global_gain);
        self.gain_max = self.gain_max.max(gi.global_gain);
        self.part23_sum += gi.part2_3_length as usize;
    }

    fn report(&self, name: &str) {
        let u = self.units.max(1) as f64;
        let shaped = self.units - self.flat;
        let kbps = if self.secs > 0.0 {
            self.bytes as f64 * 8.0 / self.secs / 1000.0
        } else {
            0.0
        };
        println!("{name}");
        println!(
            "  {} frames, {} granule-channels, {} bytes ({kbps:.1} kbps)",
            self.frames, self.units, self.bytes
        );
        println!(
            "  blocks : {:.1}% long  {:.1}% start  {:.1}% stop  {:.1}% short  {:.1}% mixed",
            100.0 * self.long as f64 / u,
            100.0 * self.start as f64 / u,
            100.0 * self.stop as f64 / u,
            100.0 * self.short as f64 / u,
            100.0 * self.mixed as f64 / u,
        );
        println!(
            "  shaping: {:.1}% FLAT (one gain, no per-band shaping), {:.1}% shaped",
            100.0 * self.flat as f64 / u,
            100.0 * shaped as f64 / u,
        );
        if shaped > 0 {
            println!(
                "           shaped granules: {:.1} non-zero sfb, spread {:.1} sf units",
                self.nz_sum as f64 / shaped as f64,
                self.spread_sum as f64 / shaped as f64,
            );
        }
        println!(
            "  flags  : preflag {:.1}%, scalefac_scale {:.1}%, scfsi {:.1}%, subblock_gain {:.1}%",
            100.0 * self.preflag as f64 / u,
            100.0 * self.scalefac_scale as f64 / u,
            100.0 * self.scfsi_units as f64 / u,
            100.0 * self.sbg_nonzero as f64 / u,
        );
        println!(
            "  gain   : mean {:.1}, range [{}, {}]   part2_3 mean {:.0} bits",
            self.gain_sum as f64 / u,
            self.gain_min,
            self.gain_max,
            self.part23_sum as f64 / u,
        );
        let hs: usize = self.part23_hist.iter().sum();
        if hs > 0 {
            let bars: Vec<String> = self
                .part23_hist
                .iter()
                .enumerate()
                .map(|(i, n)| format!("{}/8:{:.0}%", i, 100.0 * *n as f64 / hs as f64))
                .collect();
            println!("  granule fill: {}", bars.join("  "));
            println!(
                "  reservoir  : {:.1}% of granules borrowed past their share ({} bits total, {:.0} avg)",
                100.0 * self.over_nominal as f64 / u,
                self.over_bits,
                self.over_bits as f64 / self.over_nominal.max(1) as f64,
            );
        }
        println!(
            "  payload: {:.1}% of carried main-data bits spent ({} of {}), {} bits/granule unspent",
            100.0 * self.part23_sum as f64 / self.avail_sum.max(1) as f64,
            self.part23_sum,
            self.avail_sum,
            (self.avail_sum.saturating_sub(self.part23_sum)) / self.units.max(1),
        );
    }
}

/// Walk the stream frame by frame, mirroring the decoder's own entropy loop so the
/// scalefactor bit positions (and `scfsi` reuse across granules) are exact.
fn census(bytes: &[u8]) -> Census {
    let mut c = Census::new();
    let mut res = Reservoir::default();
    let mut pos = 0usize;

    while pos + 4 <= bytes.len() {
        if bytes[pos] != 0xFF || bytes[pos + 1] & 0xE0 != 0xE0 {
            pos += 1;
            continue;
        }
        let hb = [bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]];
        let Ok(header) = FrameHeader::parse(hb) else {
            pos += 1;
            continue;
        };
        let frame_size = header.frame_size();
        if frame_size < 4 {
            pos += 1;
            continue;
        }
        if pos + frame_size > bytes.len() {
            break; // trailing partial frame
        }
        let crc = if header.crc_protected { 2 } else { 0 };
        let si_start = pos + 4 + crc;
        let main_start = si_start + header.side_info_len();
        if main_start > pos + frame_size {
            pos += 1;
            continue;
        }
        let Ok(si) = sideinfo::parse(&header, &bytes[si_start..main_start]) else {
            pos += 1;
            continue;
        };
        let main = res.assemble(si.main_data_begin, &bytes[main_start..pos + frame_size]);

        c.frames += 1;
        c.bytes += frame_size;
        c.secs += header.version.samples_per_frame() as f64 / header.sample_rate as f64;
        let avail = (frame_size - 4 - crc - header.side_info_len()) * 8;
        c.avail_sum += avail;
        c.nominal = avail / (header.version.granules() * header.channel_mode.channels()).max(1);
        fold_frame(&mut c, &header, &si, &main);

        pos += frame_size;
    }
    c
}

fn fold_frame(c: &mut Census, header: &FrameHeader, si: &SideInfo, main: &[u8]) {
    let channels = header.channel_mode.channels();
    let granules = header.version.granules();
    let mut bit_pos = 0usize;
    let mut scalefac: [[scalefactors::ScaleFactors; 2]; 2] = Default::default();

    for gr in 0..granules {
        for ch in 0..channels {
            let gi = &si.granules[gr][ch];
            let part2_3_start = bit_pos;
            let prev = if gr == 1 {
                Some(scalefac[0][ch].clone())
            } else {
                None
            };
            let sf = scalefactors::decode(main, &mut bit_pos, header, si, gr, ch, prev.as_ref());
            scalefac[gr][ch] = sf.clone();
            // Skip the Huffman payload; the census reads decisions, not coefficients.
            bit_pos = part2_3_start + gi.part2_3_length as usize;

            // A granule with no main data is the Xing/Info frame's placeholder --
            // it codes nothing, so counting it would dilute every percentage.
            if gi.part2_3_length == 0 {
                continue;
            }
            if gr == 1 && si.scfsi[ch].iter().any(|&b| b) {
                c.scfsi_units += 1;
            }
            c.unit(gi, &sf);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: bscensus <file.mp3> [more.mp3 ...]");
        std::process::exit(2);
    }
    for path in &args {
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("{path}: cannot read");
            continue;
        };
        census(&bytes).report(path);
        println!();
    }
}
