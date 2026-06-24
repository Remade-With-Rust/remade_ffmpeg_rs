//! The `ffprobe` executable: a thin shim over [`rff_cli::ffprobe::run`].

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    rff_cli::ffprobe::run(args)
}
