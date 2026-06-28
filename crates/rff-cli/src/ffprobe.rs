//! The `ffprobe` binary's logic: inspect one input and print its container +
//! streams, in human-readable form or as JSON (`-of json`).

use std::process::ExitCode;

use rff::probe::MediaInfo;
use rff::Engine;

/// Entry point used by `src/bin/ffprobe.rs`.
pub fn run(args: Vec<String>) -> ExitCode {
    let mut hide_banner = false;
    let mut want_version = false;
    let mut as_json = false;
    let mut input: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-hide_banner" => hide_banner = true,
            "-version" => want_version = true,
            // Presence-only flags we accept for compatibility.
            "-show_format" | "-show_streams" | "-show_entries" => {}
            "-of" | "-print_format" => {
                i += 1;
                as_json = args.get(i).map(|v| v == "json").unwrap_or(false);
            }
            "-v" | "-loglevel" => {
                i += 1; // consume and ignore the level
            }
            "-i" => {
                i += 1;
                input = args.get(i).cloned();
            }
            other if !other.starts_with('-') => input = Some(other.to_string()),
            other => eprintln!("[warning] unrecognized option `{other}` (ignored)"),
        }
        i += 1;
    }

    if !hide_banner {
        eprint!("{}", crate::ffmpeg::banner());
    }
    if want_version {
        crate::ffmpeg::print_version();
        return ExitCode::SUCCESS;
    }

    let Some(input) = input else {
        eprintln!("[error] no input file specified. Usage: ffprobe [options] INPUT");
        return ExitCode::FAILURE;
    };

    let engine = Engine::new();
    match rff::probe::probe(&engine, &input) {
        Ok(info) => {
            if as_json {
                print_json(&input, &info);
            } else {
                print_human(&input, &info);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("[error] {input}: {err}");
            ExitCode::FAILURE
        }
    }
}

/// FFprobe-style human output.
fn print_human(input: &str, info: &MediaInfo) {
    println!("Input #0, {}, from '{}':", info.format_name, input);
    for stream in &info.streams {
        let detail = match stream.media_type {
            rff_core::MediaType::Video => {
                format!(
                    "Video: {}, {}x{}",
                    stream.codec_id, stream.width, stream.height
                )
            }
            rff_core::MediaType::Audio => format!(
                "Audio: {}, {} Hz, {} ch",
                stream.codec_id, stream.sample_rate, stream.channels
            ),
            other => format!("{other}: {}", stream.codec_id),
        };
        println!("  Stream #0:{}: {detail}", stream.index);
    }
}

/// Minimal JSON output (hand-rolled to keep the CLI dependency-light).
fn print_json(input: &str, info: &MediaInfo) {
    println!("{{");
    println!("  \"format\": {{");
    println!("    \"filename\": {},", json_str(input));
    println!("    \"format_name\": {},", json_str(&info.format_name));
    println!("    \"nb_streams\": {}", info.streams.len());
    println!("  }},");
    println!("  \"streams\": [");
    for (idx, stream) in info.streams.iter().enumerate() {
        let comma = if idx + 1 < info.streams.len() {
            ","
        } else {
            ""
        };
        println!("    {{");
        println!("      \"index\": {},", stream.index);
        println!(
            "      \"codec_type\": {},",
            json_str(&stream.media_type.to_string())
        );
        println!(
            "      \"codec_name\": {},",
            json_str(stream.codec_id.name())
        );
        println!("      \"width\": {},", stream.width);
        println!("      \"height\": {},", stream.height);
        println!("      \"sample_rate\": {},", stream.sample_rate);
        println!("      \"channels\": {}", stream.channels);
        println!("    }}{comma}");
    }
    println!("  ]");
    println!("}}");
}

/// Escape a string as a JSON string literal (quotes + backslashes).
fn json_str(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}
