//! The `ffprobe` binary's logic: inspect one input and print its container +
//! streams, in human-readable form or as JSON (`-of json`).
//!
//! `-show_targets` adds the other half of the question: not just what the file
//! *is*, but every format this build can convert it *into*, and what each
//! conversion would cost. It is computed from the engine's own registries by
//! `rff::targets`, so it never promises an output the transcoder would refuse.
//! `-show_chapters` lists the chapters (JSON: ffprobe's `chapters` array).

use std::process::ExitCode;

use rff::format::Chapter;
use rff::probe::MediaInfo;
use rff::Engine;
use rff_core::Dictionary;

/// Entry point used by `src/bin/ffprobe.rs`.
pub fn run(args: Vec<String>) -> ExitCode {
    let mut hide_banner = false;
    let mut want_version = false;
    let mut as_json = false;
    let mut show_targets = false;
    let mut show_chapters = false;
    let mut input: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-hide_banner" => hide_banner = true,
            "-version" => want_version = true,
            // Presence-only flags we accept for compatibility.
            "-show_format" | "-show_streams" | "-show_entries" => {}
            "-show_targets" => show_targets = true,
            "-show_chapters" => show_chapters = true,
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
    if show_targets {
        return match rff::targets::targets(&engine, &input) {
            Ok(plan) => {
                if as_json {
                    println!("{}", plan.to_json());
                } else {
                    print_targets(&input, &plan);
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("[error] {input}: {err}");
                ExitCode::FAILURE
            }
        };
    }
    match rff::probe::probe_with_chapters(&engine, &input) {
        Ok((info, tags, chapters)) => {
            let chapters = show_chapters.then_some(&chapters[..]);
            if as_json {
                print_json(&input, &info, &tags, chapters);
            } else {
                print_human(&input, &info, &tags, chapters);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("[error] {input}: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `-show_targets`: every output this build can write for the input, grouped by
/// kind, with the command that produces each one.
///
/// `!` lines are the caveats (a dropped stream, a second lossy generation);
/// `$` lines are runnable as printed.
fn print_targets(input: &str, plan: &rff::targets::Plan) {
    let stem = std::path::Path::new(input)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("out");

    println!(
        "Input #0, {}, from '{input}':",
        plan.source_format.as_deref().unwrap_or("?")
    );
    for s in &plan.source {
        println!("  Stream #0:{}: {}: {}", s.index, s.media_type, s.codec_id);
    }

    println!(
        "
Convert to ({} targets):",
        plan.targets.len()
    );
    let mut kind = None;
    for t in &plan.targets {
        if kind != Some(t.kind) {
            println!(
                "
  {}",
                t.kind
            );
            kind = Some(t.kind);
        }
        println!(
            "    .{:<6} {:<9} {}",
            t.extension,
            t.fidelity,
            t.stream_summary()
        );
        for note in &t.notes {
            println!("               ! {note}");
        }
        println!("               $ {}", t.command(input, stem));
    }
}

/// FFprobe-style human output.
fn print_human(input: &str, info: &MediaInfo, tags: &[Dictionary], chapters: Option<&[Chapter]>) {
    println!("Input #0, {}, from '{}':", info.format_name, input);
    if let Some(list) = chapters.filter(|c| !c.is_empty()) {
        println!("  Chapters:");
        for (i, c) in list.iter().enumerate() {
            let (start, end) = (seconds(c.start, c), seconds(c.end, c));
            println!("    Chapter #0:{i}: start {start}, end {end}");
            if !c.metadata.is_empty() {
                println!("      Metadata:");
                for (k, v) in c.metadata.iter() {
                    println!("        {k:<16}: {v}");
                }
            }
        }
    }
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
        if let Some(md) = tags.get(stream.index).filter(|d| !d.is_empty()) {
            println!("    Metadata:");
            for (k, v) in md.iter() {
                println!("      {k:<16}: {v}");
            }
        }
    }
}

/// Minimal JSON output (hand-rolled to keep the CLI dependency-light).
fn print_json(input: &str, info: &MediaInfo, tags: &[Dictionary], chapters: Option<&[Chapter]>) {
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
        match tags.get(stream.index).filter(|d| !d.is_empty()) {
            Some(md) => {
                println!("      \"channels\": {},", stream.channels);
                println!("      \"tags\": {{");
                let n = md.len();
                for (i, (k, v)) in md.iter().enumerate() {
                    let sep = if i + 1 < n { "," } else { "" };
                    println!("        {}: {}{sep}", json_str(k), json_str(v));
                }
                println!("      }}");
            }
            None => println!("      \"channels\": {}", stream.channels),
        }
        println!("    }}{comma}");
    }
    match chapters {
        Some(list) => {
            println!("  ],");
            print_json_chapters(list);
        }
        None => println!("  ]"),
    }
    println!("}}");
}

/// The `chapters` array, with ffprobe's fields.
fn print_json_chapters(list: &[Chapter]) {
    println!("  \"chapters\": [");
    for (i, c) in list.iter().enumerate() {
        println!("    {{");
        // ffprobe prints the id as a signed number.
        println!("      \"id\": {},", c.id as i64);
        println!(
            "      \"time_base\": \"{}/{}\",",
            c.time_base.num, c.time_base.den
        );
        println!("      \"start\": {},", c.start);
        println!("      \"start_time\": \"{}\",", seconds(c.start, c));
        println!("      \"end\": {},", c.end);
        print!("      \"end_time\": \"{}\"", seconds(c.end, c));
        if c.metadata.is_empty() {
            println!();
        } else {
            println!(",\n      \"tags\": {{");
            let n = c.metadata.len();
            for (k, (key, value)) in c.metadata.iter().enumerate() {
                let sep = if k + 1 < n { "," } else { "" };
                println!("        {}: {}{sep}", json_str(key), json_str(value));
            }
            println!("      }}");
        }
        println!("    }}{}", if i + 1 < list.len() { "," } else { "" });
    }
    println!("  ]");
}

/// A chapter time in seconds, as ffprobe prints it.
fn seconds(v: i64, c: &Chapter) -> String {
    format!("{:.6}", v as f64 * c.time_base.as_f64())
}

/// Escape a string as a JSON string literal: quotes, backslashes and control
/// characters.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
