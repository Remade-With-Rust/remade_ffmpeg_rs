//! Any bytes: no panic, bounded work. Seed it with `../tests/data/*.j*`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut limits = rusty_jpx::Limits::default();
    limits.max_samples = 1 << 18;
    limits.max_structures = 1 << 14;
    for lenient in [false, true] {
        let options = rusty_jpx::DecodeOptions::new()
            .with_limits(limits.clone())
            .with_allow_truncated(lenient)
            .with_reduce(data.last().map_or(0, |b| b & 1));
        let _ = rusty_jpx::decode_jpx_with(data, &options);
    }
    let _ = rusty_jpx::info(data);
});
