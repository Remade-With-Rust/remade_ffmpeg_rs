//! Core media enumerations: media types, codec identifiers, and raw sample
//! layouts (pixel formats for video, sample formats for audio).
//!
//! These deliberately cover only a small, growing subset of what FFmpeg
//! supports. Add variants as new codecs land — every variant is `#[non_exhaustive]`
//! friendly so downstream `match`es must keep a wildcard arm.

use std::fmt;

/// The broad category of an elementary stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaType {
    Video,
    Audio,
    Subtitle,
    Data,
    Attachment,
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            MediaType::Video => "video",
            MediaType::Audio => "audio",
            MediaType::Subtitle => "subtitle",
            MediaType::Data => "data",
            MediaType::Attachment => "attachment",
        };
        f.write_str(s)
    }
}

/// Identifier for a specific codec (the *what*, independent of any particular
/// encoder/decoder *implementation*).
///
/// Marked `#[non_exhaustive]`: we add codecs over time, and external matchers
/// must handle the open-ended set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CodecId {
    /// Sentinel for "no/unknown codec".
    None,
    /// H.264 / AVC video.
    H264,
    /// Opus audio.
    Opus,
    /// AVIF still image (an AV1 intra frame in a HEIF box).
    Avif,
    /// PNG still image (DEFLATE-compressed RGB/RGBA).
    Png,
    /// JPEG still image (a.k.a. MJPEG).
    Jpeg,
    /// GIF image (first frame; palette-based).
    Gif,
    /// WebP image (VP8 / VP8L).
    Webp,
    /// Linear PCM audio (uncompressed; layout in the stream's sample format).
    Pcm,
    /// Vorbis audio (Ogg Vorbis).
    Vorbis,
    /// FLAC lossless audio.
    Flac,
    /// JPEG XL image.
    Jxl,
    /// AAC audio (Advanced Audio Coding).
    Aac,
    /// VP9 video.
    Vp9,
}

impl Default for CodecId {
    fn default() -> Self {
        CodecId::None
    }
}

impl CodecId {
    /// Short canonical name, matching the token used on the `ffmpeg` command
    /// line (`-c:v h264`, `-c:a opus`, ...).
    pub fn name(self) -> &'static str {
        match self {
            CodecId::None => "none",
            CodecId::H264 => "h264",
            CodecId::Opus => "opus",
            CodecId::Avif => "avif",
            CodecId::Png => "png",
            CodecId::Jpeg => "mjpeg",
            CodecId::Gif => "gif",
            CodecId::Webp => "webp",
            CodecId::Pcm => "pcm",
            CodecId::Vorbis => "vorbis",
            CodecId::Flac => "flac",
            CodecId::Jxl => "jpegxl",
            CodecId::Aac => "aac",
            CodecId::Vp9 => "vp9",
        }
    }

    /// The media category this codec produces.
    pub fn media_type(self) -> MediaType {
        match self {
            CodecId::None => MediaType::Data,
            CodecId::H264 => MediaType::Video,
            CodecId::Opus => MediaType::Audio,
            // Image codecs carry pixel data; we model them as (single-frame) video.
            CodecId::Avif => MediaType::Video,
            CodecId::Png => MediaType::Video,
            CodecId::Jpeg => MediaType::Video,
            CodecId::Gif => MediaType::Video,
            CodecId::Webp => MediaType::Video,
            CodecId::Pcm => MediaType::Audio,
            CodecId::Vorbis => MediaType::Audio,
            CodecId::Flac => MediaType::Audio,
            CodecId::Jxl => MediaType::Video,
            CodecId::Aac => MediaType::Audio,
            CodecId::Vp9 => MediaType::Video,
        }
    }

    /// Look up a codec id from its canonical CLI name.
    pub fn from_name(name: &str) -> Option<CodecId> {
        match name {
            "h264" | "avc" | "libx264" => Some(CodecId::H264),
            "opus" | "libopus" => Some(CodecId::Opus),
            "avif" => Some(CodecId::Avif),
            "png" => Some(CodecId::Png),
            "mjpeg" | "jpeg" | "jpg" => Some(CodecId::Jpeg),
            "gif" => Some(CodecId::Gif),
            "webp" => Some(CodecId::Webp),
            "pcm" | "pcm_s16le" | "pcm_f32le" => Some(CodecId::Pcm),
            "vorbis" => Some(CodecId::Vorbis),
            "flac" => Some(CodecId::Flac),
            "jpegxl" | "jxl" => Some(CodecId::Jxl),
            "aac" => Some(CodecId::Aac),
            "vp9" | "libvpx-vp9" => Some(CodecId::Vp9),
            _ => None,
        }
    }
}

impl fmt::Display for CodecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Raw pixel layout of a decoded video frame. A small starter subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PixelFormat {
    /// Planar Y'CbCr 4:2:0, 8-bit (the workhorse for H.264).
    Yuv420p,
    /// Planar Y'CbCr 4:2:2, 8-bit.
    Yuv422p,
    /// Planar Y'CbCr 4:4:4, 8-bit.
    Yuv444p,
    /// Planar Y'CbCr 4:2:0, 10-bit (each sample stored as little-endian u16).
    Yuv420p10,
    /// Planar Y'CbCr 4:2:2, 10-bit (little-endian u16 samples).
    Yuv422p10,
    /// Planar Y'CbCr 4:4:4, 10-bit (little-endian u16 samples).
    Yuv444p10,
    /// Planar Y'CbCr 4:2:0, 12-bit (little-endian u16 samples).
    Yuv420p12,
    /// Planar Y'CbCr 4:2:2, 12-bit (little-endian u16 samples).
    Yuv422p12,
    /// Planar Y'CbCr 4:4:4, 12-bit (little-endian u16 samples).
    Yuv444p12,
    /// Packed RGB, 8 bits per channel.
    Rgb24,
    /// Packed RGBA, 8 bits per channel.
    Rgba,
}

impl PixelFormat {
    pub fn name(self) -> &'static str {
        match self {
            PixelFormat::Yuv420p => "yuv420p",
            PixelFormat::Yuv422p => "yuv422p",
            PixelFormat::Yuv444p => "yuv444p",
            PixelFormat::Yuv420p10 => "yuv420p10le",
            PixelFormat::Yuv422p10 => "yuv422p10le",
            PixelFormat::Yuv444p10 => "yuv444p10le",
            PixelFormat::Yuv420p12 => "yuv420p12le",
            PixelFormat::Yuv422p12 => "yuv422p12le",
            PixelFormat::Yuv444p12 => "yuv444p12le",
            PixelFormat::Rgb24 => "rgb24",
            PixelFormat::Rgba => "rgba",
        }
    }

    /// Bits per component sample (8 for the 8-bit formats, 10 for the 10-bit
    /// planar ones).
    pub fn bit_depth(self) -> u32 {
        match self {
            PixelFormat::Yuv420p10 | PixelFormat::Yuv422p10 | PixelFormat::Yuv444p10 => 10,
            PixelFormat::Yuv420p12 | PixelFormat::Yuv422p12 | PixelFormat::Yuv444p12 => 12,
            _ => 8,
        }
    }

    /// Bytes used to store one component sample: 1 for 8-bit, 2 for the 10-bit
    /// formats (whose samples live in the low bits of a little-endian `u16`).
    pub fn bytes_per_sample(self) -> usize {
        if self.bit_depth() > 8 {
            2
        } else {
            1
        }
    }
}

/// Raw audio sample layout. A small starter subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SampleFormat {
    /// Signed 16-bit, interleaved.
    S16,
    /// 32-bit float, interleaved.
    F32,
    /// 32-bit float, planar (one buffer per channel).
    F32Planar,
}

impl SampleFormat {
    pub fn name(self) -> &'static str {
        match self {
            SampleFormat::S16 => "s16",
            SampleFormat::F32 => "flt",
            SampleFormat::F32Planar => "fltp",
        }
    }

    /// Bytes occupied by one sample of one channel.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::S16 => 2,
            SampleFormat::F32 | SampleFormat::F32Planar => 4,
        }
    }
}
