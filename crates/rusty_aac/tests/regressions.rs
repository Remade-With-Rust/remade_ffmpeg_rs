//! Every input a fuzzer ever crashed the decoder with, replayed through the same
//! entry points as the fuzz target that found it (`fuzz/fuzz_targets/`). The file
//! name's prefix picks the target: `adts-`, `config-`, `latm-`. Each must decode
//! to output or a typed error — never a panic (H-27: every past crasher is a
//! regression test).

use rusty_aac::latm::{find_sync, parse_loas_frame, LatmDecoder};
use rusty_aac::{parse_adts, AacDecoder};

fn adts(data: &[u8]) {
    let Some((&k, bytes)) = data.split_first() else {
        return;
    };
    let mut dec = AacDecoder::new();
    if k & 1 == 0 {
        let mut pos = 0;
        while pos + 7 <= bytes.len() {
            let len = match parse_adts(&bytes[pos..]) {
                Ok(h) if h.frame_length >= 7 => h.frame_length.min(bytes.len() - pos),
                _ => 1 + (k as usize >> 1),
            };
            let _ = dec.decode(&bytes[pos..pos + len.min(bytes.len() - pos)], None);
            pos += len.max(1);
        }
    } else {
        for c in bytes.chunks(1 + (k as usize >> 1) * 7) {
            let _ = dec.decode(c, None);
        }
    }
}

fn config(data: &[u8]) {
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
}

fn latm(data: &[u8]) {
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
}

#[test]
fn every_past_fuzz_crasher_decodes_without_panicking() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/regress");
    let mut n = 0;
    for entry in std::fs::read_dir(dir).expect("tests/regress") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let data = std::fs::read(&path).unwrap();
        match name.split('-').next() {
            Some("adts") => adts(&data),
            Some("config") => config(&data),
            Some("latm") => latm(&data),
            _ => panic!("unknown regression prefix: {name}"),
        }
        n += 1;
    }
    assert!(n >= 5, "regression corpus missing ({n} files)");
}
