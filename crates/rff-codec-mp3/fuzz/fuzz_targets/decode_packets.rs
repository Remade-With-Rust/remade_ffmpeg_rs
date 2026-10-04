//! The adapter's `Decoder` on hostile bytes: split into packets wherever the
//! fuzzer says, drained between packets, then flushed. Every frame it returns
//! must be self-consistent (plane length = samples x channels x 4).
#![no_main]
use libfuzzer_sys::fuzz_target;
use rff_codec::CodecRegistry;
use rff_core::{CodecId, Frame, Packet};

fuzz_target!(|data: &[u8]| {
    let Some((&k, bytes)) = data.split_first() else { return };
    let mut reg = CodecRegistry::new();
    rff_codec_mp3::register(&mut reg);
    let Ok(mut dec) = reg.find_decoder(CodecId::Mp3) else { return };
    let check = |f: Frame| {
        if let Frame::Audio(a) = f {
            assert_eq!(a.planes[0].len(), a.samples * usize::from(a.channels) * 4);
        }
    };
    for p in bytes.chunks(1 + usize::from(k) * 11) {
        let _ = dec.send_packet(&Packet::from_data(0, p.to_vec()));
        for _ in 0..64 {
            match dec.receive_frame() { Ok(f) => check(f), Err(_) => break }
        }
    }
    dec.flush();
    for _ in 0..4096 {
        match dec.receive_frame() { Ok(f) => check(f), Err(_) => break }
    }
});
