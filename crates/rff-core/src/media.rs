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
        }
    }

    /// The media category this codec produces.
    pub fn media_type(self) -> MediaType {
        match self {
            CodecId::None => MediaType::Data,
            CodecId::H264 => MediaType::Video,
            CodecId::Opus => MediaType::Audio,
            // AVIF carries image data; we model it as a (single-frame) video stream.
            CodecId::Avif => MediaType::Video,
        }
    }

    /// Look up a codec id from its canonical CLI name.
    pub fn from_name(name: &str) -> Option<CodecId> {
        match name {
            "h264" | "avc" | "libx264" => Some(CodecId::H264),
            "opus" | "libopus" => Some(CodecId::Opus),
            "avif" => Some(CodecId::Avif),
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
            PixelFormat::Rgb24 => "rgb24",
            PixelFormat::Rgba => "rgba",
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
