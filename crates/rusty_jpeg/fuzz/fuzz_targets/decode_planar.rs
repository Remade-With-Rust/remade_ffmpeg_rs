#![no_main]
//! The planar-output path, which assembles its result differently from
//! `decode()` and has diverged from it before — hence a target of its own.
use libfuzzer_sys::fuzz_target;
use rusty_jpeg::decode::Decoder;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let mut d = Decoder::new(Cursor::new(data));
    d.set_max_decoding_buffer_size(64 * 1024 * 1024);
    d.set_single_threaded(true);
    if let Ok(img) = d.decode_planar() {
        // The geometry the planes claim must actually be backed by their data;
        // a plane shorter than its own stride x height would be a real defect.
        for c in &img.components {
            let need = c.stride * c.height.saturating_sub(1) + c.width;
            assert!(c.data.len() >= need, "plane smaller than its geometry");
        }
    }
});
