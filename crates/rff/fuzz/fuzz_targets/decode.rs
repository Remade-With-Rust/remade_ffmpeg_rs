//! Coverage-guided fuzzing of the decoders.
//!
//! Decoders are the second thing untrusted bytes reach (after demuxers) and a
//! far richer surface. The first input byte selects a decoder (from a sorted
//! list, so a crash reproduces deterministically); the rest is fed in as a
//! single packet. Decoding hostile bytes may return an error, but must never
//! panic, hang, or read out of bounds (the `panic=unwind` boundary should
//! contain a malformed-input panic as an `Err`). Run with
//! `cargo +nightly fuzz run decode`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use rff::Engine;
use rff_core::Packet;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let engine = Engine::new();

    // Stable, sorted name list so a given input always hits the same decoder.
    let mut decoders: Vec<&'static str> = engine
        .codecs
        .iter()
        .filter(|c| c.can_decode())
        .map(|c| c.name)
        .collect();
    decoders.sort_unstable();
    if decoders.is_empty() {
        return;
    }

    let name = decoders[data[0] as usize % decoders.len()];
    let Some(codec) = engine.codecs.by_name(name) else {
        return;
    };
    let Ok(mut decoder) = engine.codecs.find_decoder(codec.id) else {
        return;
    };

    let packet = Packet::from_data(0, data[1..].to_vec());
    let _ = decoder.send_packet(&packet);
    // Bounded drain so a decoder that keeps emitting frames can't loop forever.
    for _ in 0..256 {
        if decoder.receive_frame().is_err() {
            break;
        }
    }
});
