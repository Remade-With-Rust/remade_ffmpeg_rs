//! `AacDecoder` on hostile packets. The first byte picks the framing: walk the
//! input as an ADTS stream (headers re-parsed per frame, so mid-stream
//! reconfiguration is exercised), or cut it into fixed-size packets that are
//! fed as-is. Must never panic, hang, or read out of bounds.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_aac::{parse_adts, AacDecoder};

fuzz_target!(|data: &[u8]| {
    let Some((&k, bytes)) = data.split_first() else {
        return;
    };
    let mut dec = AacDecoder::new();
    if k & 1 == 0 {
        let mut pos = 0;
        while pos + 7 <= bytes.len() {
            let len = match parse_adts(&bytes[pos..]) {
                Ok(h) if h.frame_length >= 7 => h.frame_length.min(bytes.len() - pos),
                _ => 1 + (k as usize >> 1),
            };
            if let Ok(a) = dec.decode(&bytes[pos..pos + len.min(bytes.len() - pos)], None) {
                assert!(a.channels > 0 || a.samples.is_empty());
            }
            pos += len.max(1);
        }
    } else {
        for c in bytes.chunks(1 + (k as usize >> 1) * 7) {
            let _ = dec.decode(c, None);
        }
    }
});
