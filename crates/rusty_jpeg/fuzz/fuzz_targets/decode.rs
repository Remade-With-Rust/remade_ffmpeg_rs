#![no_main]
//! The packed-output decode path against arbitrary bytes.
//!
//! `Err` is the correct outcome for a corrupt file; a panic is not — in a
//! library it aborts the caller's process. Only panics, hangs and memory errors
//! are findings here.
use libfuzzer_sys::fuzz_target;
use rusty_jpeg::decode::Decoder;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let mut d = Decoder::new(Cursor::new(data));
    // Bound the allocation a malformed header can request, so the fuzzer reports
    // real defects instead of OOMing on a declared 65535x65535 image.
    d.set_max_decoding_buffer_size(64 * 1024 * 1024);
    d.set_single_threaded(true);
    let _ = d.decode();
});
