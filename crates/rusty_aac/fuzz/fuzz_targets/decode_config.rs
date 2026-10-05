//! The decoder behind an arbitrary AudioSpecificConfig — the surface a container
//! hands over (PCE layouts, SBR/PS signalling, ER AAC-LC/LTP, LD, ELD, 960-sample
//! frames, Main prediction). Input: one length byte, the config, then access
//! units each prefixed by a one-byte length. Must never panic.
#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_aac::AacDecoder;

fuzz_target!(|data: &[u8]| {
    let Some((&n, rest)) = data.split_first() else {
        return;
    };
    let n = (n as usize).min(rest.len());
    let (asc, mut aus) = rest.split_at(n);
    let Ok(mut dec) = AacDecoder::with_config_bytes(asc) else {
        return;
    };
    while let Some((&len, tail)) = aus.split_first() {
        let len = (len as usize * 4).min(tail.len());
        let (au, next) = tail.split_at(len);
        let _ = dec.decode(au, None);
        aus = next;
    }
});
