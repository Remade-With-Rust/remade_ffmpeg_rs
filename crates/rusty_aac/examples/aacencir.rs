//! Instruction-count bench for the ENCODER (run under `valgrind --tool=callgrind`):
//! encode raw interleaved `f32le` PCM through the shipping `AacEncoder` with the
//! default configuration and print the work-parity anchors — packets, bitstream
//! bytes and an FNV checksum of every output byte. A change that leaves the
//! bitstream byte-identical cannot move any anchor.
//!
//! ```text
//! aacencir <file.f32> <channels> <sample_rate>
//! ```
//!
//! Frames encode in parallel on `available_parallelism()` threads with a static
//! partition and blocking joins, so the instruction total is fixed for a given
//! machine; check the same-binary spread before trusting a delta.

use rusty_aac::{AacEncoder, AacEncoderConfig};

#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [file, channels, rate] = &args[..] else {
        panic!("usage: aacencir <file.f32> <channels> <sample_rate>");
    };
    let (channels, rate): (u16, u32) = (channels.parse().unwrap(), rate.parse().unwrap());
    let raw = std::fs::read(file).expect("read pcm");
    let pcm: Vec<f32> = raw
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let mut enc = AacEncoder::new(AacEncoderConfig::default());
    enc.push_pcm(&pcm, channels, rate).expect("push");
    enc.finish();
    let (mut packets, mut bytes, mut sum) = (0u64, 0u64, 0xcbf2_9ce4_8422_2325u64);
    while let Ok(p) = enc.next_packet() {
        packets += 1;
        bytes += p.data.len() as u64;
        for &b in &p.data {
            sum = (sum ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    }
    println!("anchors packets {packets} bytes {bytes} checksum {sum:016x}");
}
