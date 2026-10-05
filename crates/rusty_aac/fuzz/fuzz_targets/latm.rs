//! LOAS / LATM on hostile bytes: the stateful decoder walking a stream frame by
//! frame (reusing or replacing its `StreamMuxConfig`), plus the stateless frame
//! parser and the sync search. Must never panic or stall.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_aac::latm::{find_sync, parse_loas_frame, LatmDecoder};

fuzz_target!(|data: &[u8]| {
    let _ = parse_loas_frame(data);
    let mut dec = LatmDecoder::new();
    let mut pos = 0;
    while pos < data.len() {
        match dec.decode(&data[pos..], None) {
            Ok((_, used)) if used > 0 => pos += used,
            _ => match find_sync(&data[pos + 1..]) {
                Some(s) => pos += 1 + s,
                None => break,
            },
        }
    }
});
