//! The standalone parsers on hostile bytes: AudioSpecificConfig (both the plain
//! and the full `StreamConfig` forms), the SBR/PS signalling probe and the ADTS
//! header. Must never panic.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = rusty_aac::parse_audio_specific_config(data);
    let _ = rusty_aac::config::parse(data);
    let _ = rusty_aac::sbr::parse_sbr_config(data);
    let _ = rusty_aac::is_adts(data);
    let _ = rusty_aac::parse_adts(data);
});
