//! Instruction-count bench (run under `valgrind --tool=callgrind`): decode ADTS
//! files through the shipping `AacDecoder`, single-threaded, and print the
//! work-parity anchors — frames, samples and an FNV checksum of every output
//! sample's bits. A kernel change that is bit-identical cannot move any anchor;
//! if one moves, the experiment changed, not the code.
//!
//! ```text
//! aacir <file.aac | file.aus>...
//! ```
//!
//! `.aus` carries streams ADTS cannot (ER AAC-LD/ELD, ...): `u32le` ASC length,
//! the `AudioSpecificConfig`, then `u32le` length + access unit until EOF.

use rusty_aac::{parse_adts, AacDecoder};

#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() {
    let (mut frames, mut samples, mut sum) = (0u64, 0u64, 0xcbf2_9ce4_8422_2325u64);
    let mut tally = |samples_out: &[f32]| {
        frames += 1;
        samples += samples_out.len() as u64;
        for v in samples_out {
            sum = (sum ^ u64::from(v.to_bits())).wrapping_mul(0x100_0000_01b3);
        }
    };
    for f in std::env::args().skip(1) {
        let data = std::fs::read(&f).expect("read aac");
        if std::path::Path::new(&f)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("aus"))
        {
            let word = |p: usize| u32::from_le_bytes(data[p..p + 4].try_into().unwrap()) as usize;
            let asc_len = word(0);
            let mut dec = AacDecoder::with_config_bytes(&data[4..4 + asc_len]).expect("asc");
            let mut pos = 4 + asc_len;
            while pos + 4 <= data.len() {
                let len = word(pos);
                if let Ok(a) = dec.decode(&data[pos + 4..pos + 4 + len], None) {
                    tally(&a.samples);
                }
                pos += 4 + len;
            }
            continue;
        }
        let mut dec = AacDecoder::new();
        let mut pos = 0;
        while pos + 7 <= data.len() {
            let Ok(h) = parse_adts(&data[pos..]) else {
                break;
            };
            let len = h.frame_length;
            if len == 0 || pos + len > data.len() {
                break;
            }
            if let Ok(a) = dec.decode(&data[pos..pos + len], None) {
                tally(&a.samples);
            }
            pos += len;
        }
    }
    println!("anchors frames {frames} samples {samples} checksum {sum:016x}");
}
