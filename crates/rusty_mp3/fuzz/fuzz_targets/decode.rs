//! The stream decoder on hostile bytes: `Mp3Decoder::push` in caller-chosen
//! chunks, draining between pushes, then `flush`. Must never panic, hang, or
//! read out of bounds. The first byte picks the chunk size, so frame sync,
//! split frames and the reservoir across pushes are all exercised.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&k, bytes)) = data.split_first() else {
        return;
    };
    let chunk = 1 + (k as usize) * 13;
    let mut dec = rusty_mp3::Mp3Decoder::new();
    for c in bytes.chunks(chunk) {
        dec.push(c);
        while let Ok(f) = dec.next_frame() {
            assert_eq!(f.samples.len() % f.channels.max(1) as usize, 0);
        }
    }
    dec.flush();
    while dec.next_frame().is_ok() {}
});
