//! `aacenc` — WAV in, ADTS out, with every encoder arm exposed as a flag.
//!
//! The shipped CLI deliberately does not expose the experimental arms, but the
//! comparison harness has to be able to switch them one at a time to measure
//! them. This is that switch.
//!
//! ```text
//! cargo run -p rusty_aac --features lab --release --example aacenc -- \
//!     in.wav out.aac 128000 [arm ...]
//! ```
//!
//! Arms: `a1` short-block psy, `a2` tonality SMR, `a3` TNS, `a6` PNS,
//! `a7` intensity stereo, `a9` level-invariant transients, `a13` demand-based
//! stereo bit split, `kbd`/`autoshape` window shape. With no arms it is byte-identical to the shipped defaults.

use rusty_aac::lab::wav;
use rusty_aac::{AacEncoder, AacEncoderConfig, AdtsHeader, WindowShape};

#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 3 {
        eprintln!("usage: aacenc <in.wav> <out.aac> <bitrate_bps> [arms...]");
        std::process::exit(2);
    }
    let (src, dst) = (&a[0], &a[1]);
    let bitrate: u32 = a[2].parse().expect("bitrate");
    let arms: Vec<&str> = a[3..].iter().map(|s| s.as_str()).collect();
    let on = |k: &str| arms.iter().any(|x| *x == k);

    // ADDITIVE from the shipped defaults. Listing every field explicitly would
    // silently DISABLE arms that are now default-on, which is exactly how an
    // arm sweep once reported the percussive class regressing by 1.7 ODG: the
    // sweep had turned off the default it was supposed to be measuring on top of.
    // Use `noXX` to switch a default-on arm off.
    let mut cfg = AacEncoderConfig {
        bitrate_bps: bitrate,
        ..Default::default()
    };
    if on("a1") { cfg.short_block_psy = true; }
    if on("a2") { cfg.tonality_smr = true; }
    if on("a3") { cfg.tns = true; }
    if on("a6") { cfg.pns = true; }
    if on("a7") { cfg.intensity = true; }
    if on("a9") { cfg.relative_transients = true; }
    if on("a13") { cfg.stereo_bit_split = true; }
    if on("noa9") { cfg.relative_transients = false; }
    if on("noa13") { cfg.stereo_bit_split = false; }
    if on("kbd") { cfg.window_shape = WindowShape::Kbd; }
    if on("autoshape") { cfg.window_shape = WindowShape::Auto; }

    let w = wav::read(src).expect("read wav");
    let mut enc = AacEncoder::new(cfg);
    enc.push_pcm(&w.samples, w.channels, w.sample_rate)
        .expect("push_pcm");
    enc.finish();

    let mut out = Vec::new();
    while let Ok(p) = enc.next_packet() {
        out.extend_from_slice(&rusty_aac::write_adts_header(&AdtsHeader {
            object_type: 2,
            sample_rate: w.sample_rate,
            channels: w.channels,
            frame_length: 7 + p.data.len(),
            header_len: 7,
        }));
        out.extend_from_slice(&p.data);
    }
    std::fs::write(dst, &out).expect("write out");
}
