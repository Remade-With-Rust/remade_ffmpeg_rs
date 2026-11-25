//! Standalone files: every page, strict and lenient.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rusty_jbig2::{DecodeOptions, Jbig2File, Limits};

fuzz_target!(|data: &[u8]| {
    for lenient in [false, true] {
        let limits = Limits::default().with_max_work(1 << 20);
        let opts = DecodeOptions::new()
            .with_limits(limits)
            .with_lenient(lenient);
        if let Ok(f) = Jbig2File::parse_with(data, opts) {
            for i in 0..f.page_count().min(3) {
                if let Ok(b) = f.decode_page(i) {
                    assert_eq!(b.data.len(), b.stride * b.height as usize);
                }
            }
        }
    }
});
