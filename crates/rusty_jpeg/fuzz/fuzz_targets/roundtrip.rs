#![no_main]
//! Encode arbitrary pixels, then decode the result.
//!
//! Our own encoder is the one producer whose output we fully control, so
//! anything it emits that we cannot read back is unambiguously our bug. This
//! target is what would have caught the DHT-ordering defect, where the file
//! declared one set of Huffman tables and coded the scan with another.
use libfuzzer_sys::fuzz_target;
use rusty_jpeg::decode::Decoder;
use rusty_jpeg::encode::{ColorType, Encoder, SamplingFactor};
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    // Derive a small, valid geometry from the input rather than trusting it.
    let w = (data[0] as usize % 40) + 1;
    let h = (data[1] as usize % 40) + 1;
    let quality = data[2] % 101;
    let sampling = match data[3] % 3 {
        0 => SamplingFactor::R_4_4_4,
        1 => SamplingFactor::R_4_2_2,
        _ => SamplingFactor::R_4_2_0,
    };
    let optimize = data[4] & 1 == 1;

    let px = &data[5..];
    let need = w * h * 3;
    let mut rgb = vec![0u8; need];
    for (i, b) in rgb.iter_mut().enumerate() {
        *b = px[i % px.len()];
    }

    let mut jpg = Vec::new();
    {
        let mut enc = Encoder::new(&mut jpg, quality);
        enc.set_sampling_factor(sampling);
        enc.set_optimized_huffman_tables(optimize);
        if enc.encode(&rgb, w as u16, h as u16, ColorType::Rgb).is_err() {
            return;
        }
    }

    let mut d = Decoder::new(Cursor::new(&jpg));
    d.set_single_threaded(true);
    let out = d.decode().expect("our own encoder produced a file we cannot decode");
    assert_eq!(out.len(), w * h * 3, "round-trip changed the image size");
});
