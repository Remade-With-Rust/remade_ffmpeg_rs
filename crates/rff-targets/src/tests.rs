//! Planner unit tests.
//!
//! These build their own registries rather than pulling in the real format and
//! codec crates (which depend on this layer's siblings, not on it). The
//! declarations here mirror the shipping ones; `crates/rff/tests/mux_caps.rs`
//! is the gate that the shipping declarations match their muxers.

use super::*;
use rff_codec::Codec;
use rff_format::Format;

fn never_dec() -> Box<dyn rff_codec::Decoder> {
    unreachable!("planning never instantiates a codec")
}
fn never_enc() -> Box<dyn rff_codec::Encoder> {
    unreachable!("planning never instantiates a codec")
}
fn never_mux(_: rff_format::Output) -> Box<dyn rff_format::Muxer> {
    unreachable!("planning never opens a muxer")
}

/// A codec with both directions available.
fn codec(id: CodecId, name: &'static str) -> Codec {
    Codec {
        id,
        name,
        long_name: name,
        media_type: id.media_type(),
        decoder: Some(never_dec),
        encoder: Some(never_enc),
    }
}

/// A codec we can read but not write (jxl, av2 today).
fn decode_only(id: CodecId, name: &'static str) -> Codec {
    Codec {
        encoder: None,
        ..codec(id, name)
    }
}

fn format(name: &'static str, exts: &'static [&'static str], caps: MuxCaps) -> Format {
    Format {
        name,
        long_name: name,
        extensions: exts,
        demuxer: None,
        muxer: Some(never_mux),
        muxer_path: None,
        probe: None,
        mux_caps: caps,
    }
}

/// A registry pair close enough to the shipping engine to exercise the planner.
fn engine() -> (CodecRegistry, FormatRegistry) {
    let mut codecs = CodecRegistry::new();
    for c in [
        codec(CodecId::H264, "h264"),
        codec(CodecId::Vp9, "vp9"),
        codec(CodecId::Aac, "aac"),
        codec(CodecId::Opus, "opus"),
        codec(CodecId::Mp3, "mp3"),
        codec(CodecId::Flac, "flac"),
        codec(CodecId::Pcm, "pcm"),
        codec(CodecId::Vorbis, "vorbis"),
        codec(CodecId::Png, "png"),
        codec(CodecId::Jpeg, "mjpeg"),
        codec(CodecId::Avif, "avif"),
        decode_only(CodecId::Jxl, "jpegxl"),
        decode_only(CodecId::Av2, "av2"),
    ] {
        codecs.register(c);
    }

    let mut formats = FormatRegistry::new();
    for f in [
        format(
            "mp4",
            &["mp4", "mov", "m4a"],
            MuxCaps::container(&[CodecId::H264, CodecId::Avif, CodecId::Aac, CodecId::Opus]),
        ),
        format(
            "matroska",
            &["mkv", "mka"],
            MuxCaps::container(&[
                CodecId::Vp9,
                CodecId::Avif,
                CodecId::H264,
                CodecId::Opus,
                CodecId::Vorbis,
                CodecId::Aac,
                CodecId::Flac,
                CodecId::Mp3,
                CodecId::Pcm,
                CodecId::Subrip,
                CodecId::WebVtt,
            ]),
        ),
        format(
            "webm",
            &["webm"],
            MuxCaps::container(&[
                CodecId::Vp9,
                CodecId::Avif,
                CodecId::Opus,
                CodecId::Vorbis,
                CodecId::WebVtt,
            ]),
        ),
        format("wav", &["wav"], MuxCaps::single(&[CodecId::Pcm])),
        format("mp3", &["mp3"], MuxCaps::single(&[CodecId::Mp3])),
        format("flac", &["flac"], MuxCaps::single(&[CodecId::Flac])),
        format(
            "ogg",
            &["ogg"],
            MuxCaps::single(&[CodecId::Opus, CodecId::Vorbis]),
        ),
        format("png", &["png"], MuxCaps::single(&[CodecId::Png]).image()),
        format(
            "jpeg",
            &["jpg", "jpeg"],
            MuxCaps::single(&[CodecId::Jpeg]).image(),
        ),
        format("jpegxl", &["jxl"], MuxCaps::single(&[CodecId::Jxl]).image()),
        format(
            "ivf",
            &["ivf"],
            MuxCaps::single(&[CodecId::Vp9, CodecId::Av2]),
        ),
        format("srt", &["srt"], MuxCaps::single(&[CodecId::Subrip])),
        format("webvtt", &["vtt"], MuxCaps::single(&[CodecId::WebVtt])),
    ] {
        formats.register(f);
    }
    (codecs, formats)
}

/// h264 video + aac audio, the commonest input there is.
fn h264_aac() -> Vec<SourceStream> {
    vec![
        SourceStream::new(0, MediaType::Video, CodecId::H264),
        SourceStream::new(1, MediaType::Audio, CodecId::Aac),
    ]
}

#[test]
fn mp4_input_copies_into_mp4_and_leads_the_list() {
    let (c, f) = engine();
    let plan = plan(&c, &f, &h264_aac());
    let first = &plan.targets[0];
    assert_eq!(first.format, "mp4");
    assert_eq!(first.fidelity, Fidelity::Copy);
    assert_eq!(first.args, vec!["-c:v", "copy", "-c:a", "copy"]);
}

#[test]
fn webm_rejects_h264_and_aac_so_both_streams_re_encode() {
    let (c, f) = engine();
    let plan = plan(&c, &f, &h264_aac());
    let webm = plan.target("webm").expect("webm is offered");
    assert_eq!(webm.fidelity, Fidelity::Lossy);
    assert_eq!(webm.args, vec!["-c:v", "vp9", "-c:a", "opus"]);
    // ...and it says out loud that this is a second lossy generation.
    assert!(
        webm.notes.iter().any(|n| n.contains("generation loss")),
        "{:?}",
        webm.notes
    );
}

#[test]
fn matroska_copies_h264_but_prefers_opus_over_aac_only_when_it_must() {
    let (c, f) = engine();
    let plan = plan(&c, &f, &h264_aac());
    let mkv = plan.target("matroska").expect("matroska is offered");
    // Matroska accepts both source codecs outright, so nothing is re-encoded.
    assert_eq!(mkv.fidelity, Fidelity::Copy);
    assert_eq!(mkv.args, vec!["-c:v", "copy", "-c:a", "copy"]);
}

#[test]
fn audio_only_container_drops_the_video_stream_and_says_why() {
    let (c, f) = engine();
    let plan = plan(&c, &f, &h264_aac());
    let flac = plan.target("flac").expect("flac is offered");
    let video = &flac.streams[0];
    assert_eq!(
        video.action,
        Action::Drop(DropReason::UnsupportedMedia),
        "flac carries no video"
    );
    // The audio survives, re-encoded losslessly from a lossy source.
    assert_eq!(
        flac.streams[1].action,
        Action::Transcode {
            to: CodecId::Flac,
            lossy: false
        }
    );
    assert_eq!(flac.fidelity, Fidelity::Lossless);
    assert!(flac.notes.iter().any(|n| n.contains("dropped")));
}

#[test]
fn a_single_stream_container_keeps_video_over_audio() {
    let (c, f) = engine();
    let plan = plan(&c, &f, &h264_aac());
    let ivf = plan.target("ivf").expect("ivf is offered");
    assert_eq!(
        ivf.streams[0].action,
        Action::Transcode {
            to: CodecId::Vp9,
            lossy: true
        }
    );
    assert_eq!(
        ivf.streams[1].action,
        Action::Drop(DropReason::UnsupportedMedia)
    );
}

#[test]
fn still_image_targets_say_only_the_first_frame_is_written() {
    let (c, f) = engine();
    let plan = plan(&c, &f, &h264_aac());
    let png = plan.target("png").expect("png is offered");
    assert_eq!(png.kind, TargetKind::Image);
    assert_eq!(png.fidelity, Fidelity::Lossless, "png encode is lossless");
    assert!(png.notes.iter().any(|n| n.contains("first frame")));
}

#[test]
fn a_decode_only_codec_is_never_offered_as_a_target() {
    let (c, f) = engine();
    // jxl and av2 have decoders but no encoders here.
    let plan = plan(&c, &f, &h264_aac());
    assert!(
        plan.target("jpegxl").is_none(),
        "jpegxl has no encoder, so nothing can become one"
    );
}

#[test]
fn a_decode_only_source_can_still_be_converted() {
    let (c, f) = engine();
    let source = vec![SourceStream::new(0, MediaType::Video, CodecId::Jxl)];
    let plan = plan(&c, &f, &source);
    // We can read a .jxl, so png/jpeg/mp4 targets exist...
    let png = plan.target("png").expect("jxl decodes, png encodes");
    assert_eq!(
        png.streams[0].action,
        Action::Transcode {
            to: CodecId::Png,
            lossy: false
        }
    );
    // ...and jxl-to-jxl IS offered, because a remux needs no encoder at all.
    assert_eq!(plan.target("jpegxl").unwrap().fidelity, Fidelity::Copy);
}

#[test]
fn an_undecodable_stream_is_reported_as_no_decoder_not_silently_omitted() {
    let (c, f) = engine();
    // RawVideo has no entry in this registry: nothing can decode it, so it can
    // only ever be copied — and no container here accepts it.
    let source = vec![
        SourceStream::new(0, MediaType::Video, CodecId::RawVideo),
        SourceStream::new(1, MediaType::Audio, CodecId::Aac),
    ];
    let plan = plan(&c, &f, &source);
    let mp4 = plan.target("mp4").expect("the audio still makes an mp4");
    assert_eq!(mp4.streams[0].action, Action::Drop(DropReason::NoDecoder));
    assert_eq!(mp4.streams[1].action, Action::Copy);
    assert_eq!(mp4.kind, TargetKind::Audio, "no video survives");
}

#[test]
fn a_target_that_would_keep_nothing_is_not_offered() {
    let (c, f) = engine();
    // Undecodable, unwritable, uncopyable: there is no honest target at all.
    let source = vec![SourceStream::new(0, MediaType::Video, CodecId::RawVideo)];
    assert!(
        plan(&c, &f, &source).targets.is_empty(),
        "offering a target that drops every stream would be a lie"
    );
}

#[test]
fn subtitles_relabel_between_text_formats_without_a_codec() {
    let (c, f) = engine();
    let source = vec![SourceStream::new(0, MediaType::Subtitle, CodecId::Subrip)];
    let plan = plan(&c, &f, &source);
    assert_eq!(plan.target("srt").unwrap().fidelity, Fidelity::Copy);
    let vtt = plan.target("webvtt").expect("srt converts to vtt");
    assert_eq!(
        vtt.streams[0].action,
        Action::Transcode {
            to: CodecId::WebVtt,
            lossy: false
        }
    );
    assert_eq!(vtt.fidelity, Fidelity::Lossless);
    // A subtitle-only input offers no video or audio container at all.
    assert!(plan.target("mp4").is_none());
    assert!(plan.target("wav").is_none());
}

#[test]
fn audio_only_input_leads_with_audio_targets() {
    let (c, f) = engine();
    let source = vec![SourceStream::new(0, MediaType::Audio, CodecId::Flac)];
    let plan = plan(&c, &f, &source);
    assert_eq!(plan.targets[0].format, "flac", "copy first");
    assert!(
        plan.targets
            .iter()
            .take_while(|t| t.kind == TargetKind::Audio)
            .count()
            >= 4,
        "audio targets come first: {:?}",
        plan.targets.iter().map(|t| t.format).collect::<Vec<_>>()
    );
    // Lossless source -> lossy codec is flagged lossy but NOT generation loss.
    let mp3 = plan.target("mp3").unwrap();
    assert_eq!(mp3.fidelity, Fidelity::Lossy);
    assert!(
        !mp3.notes.iter().any(|n| n.contains("generation loss")),
        "flac is lossless, so the first lossy hop is not a second generation"
    );
}

#[test]
fn still_only_encoders_never_supply_a_moving_picture_container() {
    let (c, f) = engine();
    // A vp9 source into mp4: mp4's video codecs are h264 and avif, and avif
    // encodes stills only, so h264 must be the pick.
    let source = vec![SourceStream::new(0, MediaType::Video, CodecId::Vp9)];
    let plan = plan(&c, &f, &source);
    assert_eq!(
        plan.target("mp4").unwrap().streams[0].action,
        Action::Transcode {
            to: CodecId::H264,
            lossy: true
        }
    );
}

#[test]
fn mp4_defaults_audio_to_aac_even_though_it_accepts_opus() {
    let (c, f) = engine();
    let source = vec![SourceStream::new(0, MediaType::Audio, CodecId::Flac)];
    let plan = plan(&c, &f, &source);
    assert_eq!(plan.target("mp4").unwrap().args, vec!["-c:a", "aac"]);
    // Matroska has no override, so it takes the global preference.
    assert_eq!(
        plan.target("matroska").unwrap().args,
        vec!["-c:a", "copy"],
        "matroska accepts flac outright"
    );
}

#[test]
fn every_target_command_is_runnable() {
    let (c, f) = engine();
    let plan = plan(&c, &f, &h264_aac());
    for t in &plan.targets {
        // Args come in flag/value pairs.
        assert_eq!(t.args.len() % 2, 0, "{}: {:?}", t.format, t.args);
        for pair in t.args.chunks(2) {
            assert!(pair[0].starts_with('-'), "{}: {:?}", t.format, t.args);
            if pair[0].starts_with("-c:") && pair[1] != "copy" {
                assert!(
                    CodecId::from_name(&pair[1]).is_some(),
                    "{} emits `{}`, which the CLI cannot parse",
                    t.format,
                    pair[1]
                );
            }
        }
        assert!(t.command("in.mp4", "out").starts_with("rff -i in.mp4"));
    }
}

#[test]
fn json_is_well_formed_and_escapes_its_strings() {
    let (c, f) = engine();
    let plan = plan(&c, &f, &h264_aac());
    let json = plan.to_json();
    assert!(json.starts_with('{') && json.ends_with('}'));
    assert!(json.contains(r#""format":"mp4""#));
    assert!(json.contains(r#""action":"copy""#));
    assert!(json.contains(r#""action":"transcode""#));
    assert!(json.contains(r#""reason":"unsupported-media""#));
    // Balanced braces and brackets outside of strings.
    let (mut braces, mut brackets, mut in_str, mut escaped) = (0i32, 0i32, false, false);
    for ch in json.chars() {
        match (in_str, escaped, ch) {
            (true, true, _) => escaped = false,
            (true, false, '\\') => escaped = true,
            (true, false, '"') => in_str = false,
            (true, false, _) => {}
            (false, _, '"') => in_str = true,
            (false, _, '{') => braces += 1,
            (false, _, '}') => braces -= 1,
            (false, _, '[') => brackets += 1,
            (false, _, ']') => brackets -= 1,
            _ => {}
        }
        assert!(braces >= 0 && brackets >= 0);
    }
    assert_eq!((braces, brackets, in_str), (0, 0, false));
}

#[test]
fn the_matrix_lists_every_registered_format() {
    let (_, f) = engine();
    let rows = format_matrix(&f);
    assert_eq!(rows.len(), f.len());
    assert_eq!(rows[0].format, "mp4", "sorted by popularity");
    let mp4 = &rows[0];
    assert!(mp4.video.contains(&CodecId::H264));
    assert!(mp4.audio.contains(&CodecId::Aac));
    assert!(mp4.subtitle.is_empty());
    assert!(!mp4.still_image);
}

#[test]
fn an_empty_input_yields_no_targets_rather_than_every_target() {
    let (c, f) = engine();
    assert!(plan(&c, &f, &[]).targets.is_empty());
}
