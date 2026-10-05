//! Instruction-count bench (run under `valgrind --tool=callgrind`): decode ADTS
//! files through the shipping `AacDecoder`, single-threaded, and print the
//! work-parity anchors — frames, samples and an FNV checksum of every output
//! sample's bits. A kernel change that is bit-identical cannot move any anchor;
//! if one moves, the experiment changed, not the code.
//!
//! ```text
//! aacir <file.aac>...
//! ```

use rusty_aac::{parse_adts, AacDecoder};

#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() {
    let (mut frames, mut samples, mut sum) = (0u64, 0u64, 0xcbf2_9ce4_8422_2325u64);
    for f in std::env::args().skip(1) {
        let data = std::fs::read(&f).expect("read aac");
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
                frames += 1;
                samples += a.samples.len() as u64;
                for v in &a.samples {
                    sum = (sum ^ v.to_bits() as u64).wrapping_mul(0x100_0000_01b3);
                }
            }
            pos += len;
        }
    }
    println!("anchors frames {frames} samples {samples} checksum {sum:016x}");
}
