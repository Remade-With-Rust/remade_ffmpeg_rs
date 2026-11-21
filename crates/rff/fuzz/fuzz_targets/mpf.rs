//! Coverage-guided fuzzing of the JPEG Multi-Picture Format index.
//!
//! `mpf::locate` reads hostile bytes: it must never panic, hang, or hand out
//! an image range outside the input or overlapping another. Whatever it finds
//! must survive `mpf::write` and a second `locate` with every image after the
//! first byte-identical (the first gains the new index). Run with
//! `cargo +nightly fuzz run mpf`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use rff_format_jpeg::mpf::{self, TrailingPolicy, WriteImage};

fuzz_target!(|data: &[u8]| {
    for policy in [TrailingPolicy::Split, TrailingPolicy::Ignore] {
        let Ok(layout) = mpf::locate(data, policy) else {
            return;
        };
        assert!(layout.first < layout.images.len());
        let mut ranges: Vec<_> = layout.images.iter().map(|i| i.range.clone()).collect();
        ranges.sort_by_key(|r| r.start);
        for r in &ranges {
            assert!(r.start < r.end && r.end <= data.len());
        }
        for w in ranges.windows(2) {
            assert!(w[0].end <= w[1].start);
        }

        // Re-mux what was found (types as read) and find it again.
        let images: Vec<WriteImage<'_>> = layout
            .images
            .iter()
            .map(|i| WriteImage {
                data: &data[i.range.clone()],
                mp_type: Some(i.mp_type),
                representative: false,
            })
            .collect();
        if let Ok(file) = mpf::write(&images) {
            let again = mpf::locate(&file, TrailingPolicy::Ignore).expect("our own output");
            assert_eq!(again.images.len(), images.len());
            for (img, src) in again.images.iter().zip(&images).skip(1) {
                assert_eq!(&file[img.range.clone()], src.data);
            }
        }
    }
});
