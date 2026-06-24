//! The `ffmpeg` binary's logic: parse args, print the usual banners/listings,
//! and dispatch transcode jobs into the engine.

use std::process::ExitCode;

use rff::Engine;

use crate::args::{self, Action};

/// Entry point used by `src/bin/ffmpeg.rs`. `args` is everything after argv[0].
pub fn run(args: Vec<String>) -> ExitCode {
    let cli = match args::parse(&args) {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("Error parsing options: {err}");
            return ExitCode::FAILURE;
        }
    };

    // FFmpeg prints its banner to stderr unless -hide_banner.
    if !cli.hide_banner {
        eprint!("{}", banner());
    }
    for warning in &cli.warnings {
        eprintln!("[warning] {warning}");
    }

    let engine = Engine::new();

    match cli.action {
        Action::Version => {
            print_version();
            ExitCode::SUCCESS
        }
        Action::Help => {
            print_help();
            ExitCode::SUCCESS
        }
        Action::ListCodecs => {
            print_codecs(&engine);
            ExitCode::SUCCESS
        }
        Action::ListFormats => {
            print_formats(&engine);
            ExitCode::SUCCESS
        }
        Action::Transcode(spec) => {
            if spec.inputs.is_empty() && spec.outputs.is_empty() {
                print_usage();
                return ExitCode::FAILURE;
            }
            match rff::transcode::run(&engine, &spec) {
                Ok(report) => {
                    eprintln!(
                        "done — {} packet(s) written, {} frame(s) decoded",
                        report.packets_written, report.frames_decoded
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("[error] {err}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// The startup banner. Deliberately NOT branded as "FFmpeg" — see the trademark
/// note in the README.
pub fn banner() -> String {
    format!(
        "remade_ffmpeg_rs {ver} (ffmpeg-compatible CLI) — Remade With Rust, by Mata Network\n\
         Not affiliated with or endorsed by the FFmpeg project. \"FFmpeg\" is a trademark of Fabrice Bellard.\n",
        ver = rff::VERSION,
    )
}

/// `-version` output (to stdout).
pub fn print_version() {
    println!("remade_ffmpeg_rs version {}", rff::VERSION);
    println!("A clean-room, permissively-licensed media toolkit written in Rust.");
    println!("Apache-2.0. https://github.com/Remade-With-Rust/remade_ffmpeg_rs");
}

/// Minimal `-h` text.
fn print_help() {
    println!("{}", banner());
    println!("Usage: ffmpeg [options] [-i input]... [output options] output\n");
    println!("Common options:");
    println!("  -i FILE            add an input file (repeatable)");
    println!("  -f FMT             force container format for the next input/output");
    println!("  -c:v CODEC         video codec for the output (e.g. h264, copy)");
    println!("  -c:a CODEC         audio codec for the output (e.g. opus, copy)");
    println!("  -b:v / -b:a RATE   target bitrate (e.g. 2M, 128k)");
    println!("  -y / -n            overwrite / never overwrite the output");
    println!("  -codecs            list supported codecs");
    println!("  -formats           list supported container formats");
    println!("  -hide_banner       suppress the startup banner");
    println!("  -version           print version and exit");
}

/// One-line usage hint, printed when invoked with nothing to do.
fn print_usage() {
    eprintln!("Usage: ffmpeg [options] [-i input]... output   (try `ffmpeg -h`)");
}

/// `ffmpeg -codecs`: list every registered codec with its capabilities.
fn print_codecs(engine: &Engine) {
    println!("Codecs:");
    println!(" D. = decoding supported");
    println!(" .E = encoding supported");
    println!(" -----");
    let mut codecs: Vec<_> = engine.codecs.iter().collect();
    codecs.sort_by_key(|c| c.name);
    for codec in codecs {
        let d = if codec.can_decode() { 'D' } else { '.' };
        let e = if codec.can_encode() { 'E' } else { '.' };
        println!(
            " {d}{e} {:<9} {:<8} {}",
            codec.media_type.to_string(),
            codec.name,
            codec.long_name
        );
    }
}

/// `ffmpeg -formats`: list every registered container format.
fn print_formats(engine: &Engine) {
    println!("File formats:");
    println!(" D. = demuxing supported");
    println!(" .E = muxing supported");
    println!(" -----");
    let mut formats: Vec<_> = engine.formats.iter().collect();
    formats.sort_by_key(|f| f.name);
    for format in formats {
        let d = if format.can_demux() { 'D' } else { '.' };
        let e = if format.can_mux() { 'E' } else { '.' };
        println!(" {d}{e} {:<8} {}", format.name, format.long_name);
    }
}
