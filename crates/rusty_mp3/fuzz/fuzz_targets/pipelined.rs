//! The two-thread decoder must equal the serial one on ANY input -- the
//! equivalence its documentation promises -- and neither may panic.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let piped = rusty_mp3::decode_pipelined(data);
    let mut dec = rusty_mp3::Mp3Decoder::new();
    dec.push(data);
    dec.flush();
    let mut serial = Vec::new();
    while let Ok(f) = dec.next_frame() {
        serial.push(f);
    }
    assert_eq!(piped.len(), serial.len(), "frame count");
    for (a, b) in piped.iter().zip(&serial) {
        assert_eq!(a.channels, b.channels);
        assert_eq!(a.sample_rate, b.sample_rate);
        assert!(
            a.samples.iter().zip(&b.samples).all(|(x, y)| x.to_bits() == y.to_bits())
                && a.samples.len() == b.samples.len(),
            "pipelined PCM differs from serial"
        );
    }
});
