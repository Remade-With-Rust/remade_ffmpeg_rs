//! The public frame-level API on hostile input: `FrameHeader::parse` on the first
//! four bytes, then `Mp3Decode::decode_frame` with the side info and main data
//! split wherever the fuzzer says -- including lengths the header disagrees
//! with. Several frames run through one decoder so the reservoir carries state.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_mp3::decode::Mp3Decode;
use rusty_mp3::header::FrameHeader;

fuzz_target!(|data: &[u8]| {
    let mut dec = Mp3Decode::new();
    let mut rest = data;
    for _ in 0..8 {
        if rest.len() < 6 {
            return;
        }
        let hb = [rest[0], rest[1], rest[2], rest[3]];
        let (si_len, md_len) = (rest[4] as usize, (rest[5] as usize) * 7);
        rest = &rest[6..];
        let si_len = si_len.min(rest.len());
        let (si, tail) = rest.split_at(si_len);
        let md_len = md_len.min(tail.len());
        let (md, tail) = tail.split_at(md_len);
        rest = tail;
        let Ok(h) = FrameHeader::parse(hb) else {
            continue;
        };
        let _ = (h.frame_size(), h.side_info_len());
        let _ = dec.decode_frame(&h, si, md);
    }
});
