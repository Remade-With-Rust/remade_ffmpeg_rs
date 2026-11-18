//! `Vp9EncoderConfig::keyframe_interval` (#119): a keyframe at least every N
//! shown frames in the default, ALT-REF and two-pass paths, and every stream
//! still decodes.

use rusty_vp9::{Vp9Decoder, Vp9Encoder, Vp9EncoderConfig};

const W: u32 = 64;
const H: u32 = 48;

/// A moving gradient with some texture, so frames differ.
fn frame(i: u32) -> [Vec<u8>; 3] {
    let y: Vec<u8> = (0..W * H)
        .map(|p| {
            let (x, yy) = (p % W, p / W);
            ((x * 3 + yy * 2 + i * 5 + (x * yy) % 7) & 0xFF) as u8
        })
        .collect();
    let c = ((W / 2) * (H / 2)) as usize;
    [y, vec![128u8.wrapping_add(i as u8); c], vec![120; c]]
}

/// Encode `n` frames; return which packets are keyframes and the bytes.
fn encode(cfg: &Vp9EncoderConfig, n: u32) -> (Vec<bool>, Vec<Vec<u8>>) {
    let mut enc = Vp9Encoder::default();
    enc.configure(cfg).unwrap();
    let (mut keys, mut data) = (Vec::new(), Vec::new());
    let mut drain = |enc: &mut Vp9Encoder| {
        while let Ok(p) = enc.next_packet() {
            keys.push(p.keyframe);
            data.push(p.data);
        }
    };
    for i in 0..n {
        let f = frame(i);
        enc.push_frame(
            [&f[0], &f[1], &f[2]],
            [W as usize, (W / 2) as usize, (W / 2) as usize],
            W,
            H,
        )
        .unwrap();
        drain(&mut enc);
    }
    enc.flush();
    drain(&mut enc);
    (keys, data)
}

fn decodes(data: &[Vec<u8>]) -> usize {
    let mut dec = Vp9Decoder::default();
    let mut shown = 0;
    for d in data {
        dec.push(d, None).unwrap();
        while dec.next_frame().is_ok() {
            shown += 1;
        }
    }
    shown
}

fn key_positions(keys: &[bool]) -> Vec<usize> {
    keys.iter()
        .enumerate()
        .filter(|(_, &k)| k)
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn default_path_puts_a_keyframe_every_n_frames() {
    let cfg = Vp9EncoderConfig {
        qindex: Some(120),
        keyframe_interval: Some(10),
        ..Default::default()
    };
    let (keys, data) = encode(&cfg, 35);
    assert_eq!(key_positions(&keys), [0, 10, 20, 30]);
    assert_eq!(decodes(&data), 35);
}

#[test]
fn without_an_interval_only_the_first_frame_is_a_keyframe() {
    for interval in [None, Some(0)] {
        let cfg = Vp9EncoderConfig {
            qindex: Some(120),
            keyframe_interval: interval,
            ..Default::default()
        };
        let (keys, _) = encode(&cfg, 25);
        assert_eq!(key_positions(&keys), [0], "{interval:?}");
    }
}

#[test]
fn alt_ref_groups_start_on_a_keyframe_and_never_overrun() {
    // lag 8 (ALT-REF groups), interval 12: groups of 8 and keys on group starts;
    // interval 5 < lag: groups are cut to 5.
    for (lag, interval) in [(8usize, 12u32), (8, 5), (4, 4)] {
        let cfg = Vp9EncoderConfig {
            qindex: Some(120),
            lag: Some(lag),
            keyframe_interval: Some(interval),
            ..Default::default()
        };
        let n = 40;
        let (_, data) = encode(&cfg, n);
        // Packets here are per group (an ARF superframe, P frames, a
        // show-existing), so count keyframes in display order through the
        // decoder's view: decode, and check every GOP by frame index instead.
        assert_eq!(decodes(&data) as u32, n, "lag {lag} interval {interval}");
        let mut dec = Vp9Decoder::default();
        let mut shown = 0u32;
        let mut last_key = 0u32;
        for d in &data {
            let key = d.first().is_some_and(|&b| b & 0x0c == 0) && d.len() > 1;
            dec.push(d, None).unwrap();
            let mut first = true;
            while dec.next_frame().is_ok() {
                if key && first {
                    last_key = shown;
                }
                first = false;
                shown += 1;
                assert!(
                    shown - last_key <= interval,
                    "lag {lag} interval {interval}: frame {} is {} past its keyframe",
                    shown - 1,
                    shown - 1 - last_key
                );
            }
        }
    }
}

#[test]
fn two_pass_respects_the_interval() {
    let cfg = Vp9EncoderConfig {
        bitrate_bps: Some(200_000.0),
        fps: Some(30.0),
        two_pass: true,
        keyframe_interval: Some(8),
        ..Default::default()
    };
    let (keys, data) = encode(&cfg, 20);
    assert_eq!(key_positions(&keys), [0, 8, 16]);
    assert_eq!(decodes(&data), 20);
}

#[test]
fn environment_switches_do_not_change_the_output() {
    // Without the `env-tuning` feature the `VP9_*` switches are not read.
    let cfg = Vp9EncoderConfig {
        qindex: Some(100),
        ..Default::default()
    };
    let (_, before) = encode(&cfg, 6);
    std::env::set_var("VP9_NO_CHAIN", "1");
    std::env::set_var("VP9_ARF_QSCALE", "1.0");
    let (_, after) = encode(&cfg, 6);
    std::env::remove_var("VP9_NO_CHAIN");
    std::env::remove_var("VP9_ARF_QSCALE");
    if cfg!(not(feature = "env-tuning")) {
        assert_eq!(before, after);
    }
    // ...and the config field reaches the same switch.
    let unchained = Vp9EncoderConfig {
        chain: Some(false),
        ..cfg
    };
    let (_, off) = encode(&unchained, 6);
    assert_ne!(off, before, "chain = false changes P frames");
}
