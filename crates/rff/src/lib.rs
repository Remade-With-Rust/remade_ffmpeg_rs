//! `rff` — the Remade FFmpeg (Rust) engine facade.
//!
//! This is the crate you depend on to *use* the project as a library. It pulls
//! the layered crates together:
//! * re-exports the core vocabulary ([`core`], [`codec`], [`format`]),
//! * builds an [`Engine`] with every built-in codec and container registered,
//! * exposes the high-level [`transcode`] and [`probe`] APIs that the CLI and
//!   the HTTP server are both thin wrappers over.
//!
//! "API first": the CLI (`ffmpeg`/`ffprobe`) and the server expose *this* API.
//! There is no logic in those front-ends that isn't reachable programmatically.

pub use rff_codec as codec;
pub use rff_core as core;
pub use rff_format as format;

pub mod probe;
pub mod transcode;

use rff_codec::CodecRegistry;
use rff_format::FormatRegistry;

/// A fully-wired engine: every built-in codec and container, ready to use.
///
/// Construct one with [`Engine::new`] and hand it to [`transcode`] / [`probe`],
/// or query its registries directly (e.g. to implement `ffmpeg -codecs`).
pub struct Engine {
    pub codecs: CodecRegistry,
    pub formats: FormatRegistry,
}

impl Engine {
    /// Build an engine with all built-in codecs and formats registered.
    pub fn new() -> Engine {
        let mut codecs = CodecRegistry::new();
        register_builtin_codecs(&mut codecs);

        let mut formats = FormatRegistry::new();
        register_builtin_formats(&mut formats);

        Engine { codecs, formats }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

/// Register every codec compiled into this build. New codec crates get one line
/// here.
fn register_builtin_codecs(codecs: &mut CodecRegistry) {
    rff_codec_h264::register(codecs);
    rff_codec_opus::register(codecs);
    rff_codec_avif::register(codecs);
    rff_codec_png::register(codecs);
    rff_codec_jpeg::register(codecs);
    rff_codec_gif::register(codecs);
    rff_codec_webp::register(codecs);
    rff_codec_pcm::register(codecs);
    rff_codec_vorbis::register(codecs);
    rff_codec_flac::register(codecs);
    rff_codec_jxl::register(codecs);
    rff_codec_aac::register(codecs);
    rff_codec_vp9::register(codecs);
    rff_codec_mp3::register(codecs);
    // H.264 defaults to the in-house pure-Rust `rusty_h264` (registered above).
    // The `h264-openh264` feature swaps in Cisco's C/FFI openh264 instead — a
    // cross-check / fallback option, off by default.
    #[cfg(feature = "h264-openh264")]
    rff_codec_openh264::register(codecs);
}

/// Register every container format compiled into this build.
fn register_builtin_formats(formats: &mut FormatRegistry) {
    rff_format_avi::register(formats);
    rff_format_avif::register(formats);
    rff_format_png::register(formats);
    rff_format_jpeg::register(formats);
    rff_format_gif::register(formats);
    rff_format_webp::register(formats);
    rff_format_wav::register(formats);
    rff_format_ogg::register(formats);
    rff_format_flac::register(formats);
    rff_format_jxl::register(formats);
    rff_format_mp4::register(formats);
    rff_format_mkv::register(formats);
    rff_format_ts::register(formats);
    rff_format_flv::register(formats);
}

/// The crate version, surfaced in the CLI/server banners.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
