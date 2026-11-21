//! Damaged frames and bytes that are not audio: a frame that cannot be what an
//! encoder wrote is REFUSED (dropped), never decoded to samples far outside
//! full scale.
//!
//! Before these checks, a silent Layer I file with a few flipped bytes decoded
//! to a frame peaking at 13.7 (+23 dB over full scale), rows of an
//! uncompressed picture to 64, and a `QuickTime` movie's bytes to 3e9: a damaged
//! allocation asks for more bits than the frame holds, the missing bits read as
//! zeros, and a zero codeword is the most negative level of every allocated
//! subband. minimp3 drops such frames (`bs_frame->pos > bs_frame->limit`), and
//! refuses a Layer III frame for its side information (`L3_read_side_info`).
//!
//! Every stream here is built in the test. Intact streams decode bit for bit
//! as before: that is what the oracle tests (`iso_vectors`, `layer12`, the
//! FFmpeg hashes in `src/lib.rs`) hold.

use rusty_mp3::header::FrameHeader;
use rusty_mp3::layer12::{Layer12Decode, Layer12Header};
use rusty_mp3::{Mp3Decode, Mp3Decoder, Mp3Encoder, Mp3EncoderConfig, MAX_OUTPUT_AMPLITUDE};

/// Frames out, and their samples, from the stream decoder.
fn decode(bytes: &[u8]) -> (usize, Vec<f32>) {
    let mut dec = Mp3Decoder::new();
    dec.push(bytes);
    dec.flush();
    let (mut frames, mut out) = (0, Vec::new());
    while let Ok(f) = dec.next_frame() {
        frames += 1;
        out.extend_from_slice(&f.samples);
    }
    (frames, out)
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, s| m.max(s.abs()))
}

/// `n` frames of digital silence: `header`, then an all-zero allocation.
fn silence(header: [u8; 4], n: usize) -> Vec<u8> {
    let size = Layer12Header::parse(header).unwrap().frame_size().unwrap();
    let mut out = Vec::new();
    for _ in 0..n {
        out.extend_from_slice(&header);
        out.resize(out.len() + size - 4, 0);
    }
    out
}

/// MPEG-1 Layer I, 32 kbps, 32 kHz, mono: 48-byte frames.
const L1_SILENT: [u8; 4] = [0xFF, 0xFF, 0x18, 0xC0];
/// MPEG-1 Layer II, 192 kbps, 48 kHz, stereo: 576-byte frames.
const L2_SILENT: [u8; 4] = [0xFF, 0xFD, 0xA4, 0x00];

/// The reported file: four frames of Layer I silence with 16 bytes xor-ed with
/// 0x5A, 7 bytes apart, from 30% of the file. The flips land in the
/// allocations of frames 2, 3 and 4, which then ask for 560 bits and more of a
/// frame that holds 352. Each of those frames is refused; the first is silent.
/// (This decoded to one frame peaking at 13.7; minimp3 gives one silent frame.)
#[test]
fn a_bit_flipped_silent_layer1_file_stays_silent() {
    let clean = silence(L1_SILENT, 4);
    assert_eq!(clean.len(), 192);
    let (frames, pcm) = decode(&clean);
    assert_eq!((frames, pcm.len(), peak(&pcm)), (4, 4 * 384, 0.0));

    let mut hurt = clean;
    let at = hurt.len() * 30 / 100;
    for j in 0..16 {
        if let Some(b) = hurt.get_mut(at + j * 7) {
            *b ^= 0x5A;
        }
    }
    // Frame by frame, as a demuxer hands them over.
    let mut dec = Layer12Decode::new();
    let mut kept = Vec::new();
    for (k, frame) in hurt.chunks(48).enumerate() {
        let h = Layer12Header::parse([frame[0], frame[1], frame[2], frame[3]]).unwrap();
        match dec.try_decode_frame(&h, frame) {
            Ok(pcm) => {
                assert_eq!(peak(&pcm), 0.0, "frame {k}");
                kept.push(k);
            }
            Err(e) => assert!(matches!(e, rusty_mp3::Error::InvalidData(_)), "{e}"),
        }
        // The infallible entry point mutes what the fallible one refuses.
        let muted = Layer12Decode::new().decode_frame(&h, frame);
        assert_eq!(muted.len(), 384 * h.channels());
        assert_eq!(peak(&muted), 0.0, "frame {k}");
    }
    assert_eq!(kept, [0], "the undamaged frame, and only it");
    // The stream decoder agrees: the one good frame, as minimp3 gives it.
    let (frames, pcm) = decode(&hurt);
    assert_eq!((frames, pcm.len(), peak(&pcm)), (1, 384, 0.0));
}

/// A Layer II frame whose side information is all ones asks for far more bits
/// than it holds: it is dropped, and the clean frames around it stay.
#[test]
fn a_frame_that_reads_past_its_end_is_dropped() {
    let mut data = silence(L2_SILENT, 4);
    for b in &mut data[576 + 4..2 * 576] {
        *b = 0xFF;
    }
    let h = Layer12Header::parse(L2_SILENT).unwrap();
    let mut dec = Layer12Decode::new();
    assert!(dec.try_decode_frame(&h, &data[..576]).is_ok());
    assert!(dec.try_decode_frame(&h, &data[576..2 * 576]).is_err());
    let (frames, pcm) = decode(&data);
    assert_eq!((frames, pcm.len(), peak(&pcm)), (3, 3 * 1152 * 2, 0.0));
    // A frame cut short of what its allocation needs is refused the same way
    // (it used to read the missing bits as zeros).
    let mut loud = L2_SILENT.to_vec();
    loud.resize(576, 0xA5);
    let whole = Layer12Decode::new().try_decode_frame(&h, &loud);
    let cut = Layer12Decode::new().try_decode_frame(&h, &loud[..60]);
    assert!(cut.is_err() || whole.is_err());
}

/// Layer I allocation 15 is forbidden (ISO/IEC 11172-3 2.4.2.5). With room for
/// its 16-bit samples it used to decode -- to a subband at negative full scale
/// when the samples are zeros.
#[test]
fn the_forbidden_layer1_allocation_is_refused() {
    // 448 kbps at 32 kHz: 672-byte frames, room for anything.
    let header = [0xFF, 0xFF, 0xE8, 0xC0];
    let h = Layer12Header::parse(header).unwrap();
    assert_eq!(h.frame_size(), Some(672));
    let mut frame = header.to_vec();
    frame.resize(672, 0);
    frame[4] = 0xF0; // subband 0: allocation 15
    assert!(Layer12Decode::new().try_decode_frame(&h, &frame).is_err());
    assert_eq!(peak(&Layer12Decode::new().decode_frame(&h, &frame)), 0.0);
    frame[4] = 0xE0; // allocation 14 is a frame like any other
    let pcm = Layer12Decode::new().try_decode_frame(&h, &frame).unwrap();
    assert!(peak(&pcm) > 0.5 && peak(&pcm) <= MAX_OUTPUT_AMPLITUDE);
}

/// The backstop for damage that is valid syntax: every subband of a Layer I
/// frame at the ceiling of its range (allocation 12, scalefactor index 0, all
/// samples the most negative level) fits its frame and passes every check, and
/// synthesizes to about 64x full scale. No encoder writes it. It is dropped.
#[test]
fn a_frame_beyond_the_output_bound_is_dropped() {
    let header = [0xFF, 0xFF, 0xE8, 0xC0];
    let h = Layer12Header::parse(header).unwrap();
    let mut frame = header.to_vec();
    frame.resize(672, 0);
    frame[4..20].fill(0xCC); // 32 allocations of 12; scalefactors and samples 0
    let mut dec = Layer12Decode::new();
    assert!(dec.try_decode_frame(&h, &frame).is_err());
    assert_eq!(peak(&dec.decode_frame(&h, &frame)), 0.0);
    // In a stream: ten of them between silent frames leave only the silence.
    let quiet = {
        let mut f = header.to_vec();
        f.resize(672, 0);
        f
    };
    let mut stream = Vec::new();
    for k in 0..16 {
        stream.extend_from_slice(if (3..13).contains(&k) { &frame } else { &quiet });
    }
    let (frames, pcm) = decode(&stream);
    assert!(frames <= 6, "{frames}");
    assert_eq!(peak(&pcm), 0.0);
}

/// A second of a tone with clicks (so the encoder uses its bit reservoir), as
/// MPEG-1 Layer III at 128 kbps: the stream, and where each frame starts.
fn layer3_stream() -> (Vec<u8>, Vec<usize>) {
    let pcm: Vec<f32> = (0..2 * 44100)
        .map(|i| {
            let click = if i % 4000 < 40 { 0.4 } else { 0.0 };
            0.3 * (i as f32 * 0.05).sin() + click * (i as f32 * 1.3).sin()
        })
        .collect();
    let mut enc = Mp3Encoder::new(Mp3EncoderConfig {
        bitrate_kbps: 128,
        vbr_quality: None,
    });
    enc.push_pcm_f32(&pcm, 1, 44100).unwrap();
    enc.finish();
    let mut mp3 = Vec::new();
    while let Ok(p) = enc.next_packet() {
        mp3.extend_from_slice(&p);
    }
    let (mut at, mut starts) = (0, Vec::new());
    while at + 4 <= mp3.len() {
        let h = FrameHeader::parse([mp3[at], mp3[at + 1], mp3[at + 2], mp3[at + 3]]).unwrap();
        starts.push(at);
        at += h.frame_size();
    }
    (mp3, starts)
}

/// Layer III side information that cannot be: more than 288 `big_values`
/// pairs, or granules longer than the main data the frame can reach. The
/// frame is refused (minimp3 and FFmpeg refuse it too), its neighbours decode,
/// and nothing leaves the stream's range.
#[test]
fn layer3_frames_with_invalid_side_info_are_dropped() {
    let (mp3, starts) = layer3_stream();
    let (clean_frames, clean) = decode(&mp3);
    let clean_peak = peak(&clean);
    assert!(clean_peak > 0.2 && clean_peak < 1.0, "{clean_peak}");
    let k = starts[20];
    let header = FrameHeader::parse([mp3[k], mp3[k + 1], mp3[k + 2], mp3[k + 3]]).unwrap();
    let side = k + 4..k + 4 + header.side_info_len();
    let end = k + header.frame_size();

    // MPEG-1 mono: main_data_begin(9) private(5) scfsi(4), then granule 0's
    // part2_3_length(12) and big_values(9), bits 30..39.
    let mut big = mp3.clone();
    big[side.start + 3] |= 0x03;
    big[side.start + 4] |= 0xFE;
    // part2_3_length of both granules (bits 18..30 and, 59 bits on, 77..89):
    // 8190 bits, more than the frame's 396 bytes and all 511 a back-pointer
    // can reach hold together.
    let mut long = mp3.clone();
    long[side.start + 2] |= 0x3F;
    long[side.start + 3] |= 0xFC;
    long[side.start + 9] |= 0x07;
    long[side.start + 10] = 0xFF;
    long[side.start + 11] |= 0x80;
    for (name, hurt) in [("big_values", &big), ("part2_3_length", &long)] {
        let mut frame = Mp3Decode::new();
        let got = frame.decode_frame(&header, &hurt[side.clone()], &hurt[side.end..end]);
        assert!(got.is_err(), "{name}: the frame decoder refuses it");
        let (frames, pcm) = decode(hurt);
        assert_eq!(frames, clean_frames - 1, "{name}: one frame dropped");
        assert!(
            peak(&pcm) <= 1.5 * clean_peak,
            "{name}: peak {}",
            peak(&pcm)
        );
    }
}

/// A Layer III stream cut ahead of a frame that reaches back into the bit
/// reservoir: the granules whose main data is not there are silent, as in
/// FFmpeg, instead of being Huffman-decoded from the wrong bytes.
#[test]
fn a_layer3_stream_cut_ahead_of_its_reservoir_starts_silent() {
    let (mp3, starts) = layer3_stream();
    let (_, clean) = decode(&mp3);
    let clean_peak = peak(&clean);
    // The first frame (past the tag frame and the start) that reaches back.
    let reach = |k: usize| {
        let h = FrameHeader::parse([mp3[k], mp3[k + 1], mp3[k + 2], mp3[k + 3]]).unwrap();
        let si = rusty_mp3::decode::sideinfo::parse(&h, &mp3[k + 4..k + 4 + h.side_info_len()]);
        si.map_or(0, |si| si.main_data_begin)
    };
    let cut = starts[10..]
        .iter()
        .copied()
        .find(|&k| reach(k) > 0)
        .expect("the encoder used its bit reservoir");
    let (frames, pcm) = decode(&mp3[cut..]);
    assert!(frames > 10);
    assert!(
        pcm[..576].iter().all(|&s| s == 0.0),
        "the granule without its main data is silent"
    );
    assert!(peak(&pcm) <= 1.5 * clean_peak, "peak {}", peak(&pcm));
    assert!(pcm.iter().all(|s| s.abs() <= MAX_OUTPUT_AMPLITUDE));
}
