//! PDF-style streams: the input is split into globals and page.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rusty_jbig2::{decode_jbig2_with, DecodeOptions, Limits};

fuzz_target!(|data: &[u8]| {
    let Some((&n, rest)) = data.split_first() else {
        return;
    };
    let (globals, page) = rest.split_at(rest.len() * usize::from(n) / 256);
    for lenient in [false, true] {
        let limits = Limits::default().with_max_work(1 << 20);
        let opts = DecodeOptions::new()
            .with_limits(limits)
            .with_lenient(lenient);
        if let Ok(b) = decode_jbig2_with(page, Some(globals), &opts) {
            assert_eq!(b.into_pdf_samples().is_empty(), false);
        }
    }
});
