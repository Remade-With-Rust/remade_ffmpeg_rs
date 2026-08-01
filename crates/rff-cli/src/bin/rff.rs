//! The `rff` executable: a thin shim over [`rff_cli::ffmpeg::run`].
//!
//! This is the DEFAULT transcoder binary name. The FFmpeg-compatible `ffmpeg`
//! name is the same program, built only under the `drop-in-names` feature —
//! installing it shadows a real FFmpeg on `PATH`, so it must be opted into.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    rff_cli::ffmpeg::run(args)
}
