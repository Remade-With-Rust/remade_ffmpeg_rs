//! Video stream metadata that containers carry beside the bitstream: colour
//! description, HDR mastering and light levels, pixel aspect, clean aperture
//! and start timecode.
//!
//! These are *labels*, not conversions. A container that drops them still
//! decodes, but the picture is shown with the wrong colours, the wrong shape
//! or the wrong time, so every container that has a slot for one reads it and
//! writes it back.

/// Colour description as ITU-T H.273 (ISO/IEC 23091-2) code points: the values
/// QuickTime `nclc`, ISO `nclx`, Matroska `Colour`, VP9 `vpcC` and the codecs'
/// own VUI all use. `2` means unspecified in each field.
///
/// The range (limited / full) is [`ColorRange`](crate::ColorRange), which
/// predates this type and sits beside it on the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColorInfo {
    /// `colour_primaries` (1 = BT.709, 9 = BT.2020, 12 = P3-D65, ...).
    pub primaries: u16,
    /// `transfer_characteristics` (1 = BT.709, 16 = PQ, 18 = HLG, ...).
    pub transfer: u16,
    /// `matrix_coefficients` (1 = BT.709, 9 = BT.2020 NCL, 0 = identity/RGB, ...).
    pub matrix: u16,
}

impl ColorInfo {
    /// The H.273 "unspecified" code point.
    pub const UNSPECIFIED_CODE: u16 = 2;

    /// All three fields unspecified.
    pub const UNSPECIFIED: ColorInfo = ColorInfo {
        primaries: 2,
        transfer: 2,
        matrix: 2,
    };

    /// True when no field carries information (all are 2), so a container
    /// has nothing to write.
    pub fn is_unspecified(&self) -> bool {
        *self == ColorInfo::UNSPECIFIED
    }
}

impl Default for ColorInfo {
    fn default() -> ColorInfo {
        ColorInfo::UNSPECIFIED
    }
}

/// SMPTE ST 2086 mastering display colour volume, in the units MP4 `mdcv` and
/// the HEVC/AV1 metadata store it: chromaticity coordinates in 0.00002
/// steps, luminance in 0.0001 cd/m² steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MasteringDisplay {
    /// Red primary (x, y).
    pub red: (u16, u16),
    /// Green primary (x, y).
    pub green: (u16, u16),
    /// Blue primary (x, y).
    pub blue: (u16, u16),
    /// White point (x, y).
    pub white_point: (u16, u16),
    /// Maximum display luminance, 0.0001 cd/m² units.
    pub max_luminance: u32,
    /// Minimum display luminance, 0.0001 cd/m² units.
    pub min_luminance: u32,
}

impl MasteringDisplay {
    /// One chromaticity step, as a fraction.
    pub const CHROMA_UNIT: f64 = 0.00002;
    /// One luminance step, in cd/m².
    pub const LUMA_UNIT: f64 = 0.0001;

    /// The coordinate in [`CHROMA_UNIT`](Self::CHROMA_UNIT) steps for a
    /// fractional chromaticity (Matroska stores floats), clamped to range.
    pub fn chroma_from_f64(v: f64) -> u16 {
        (v / Self::CHROMA_UNIT).round().clamp(0.0, u16::MAX as f64) as u16
    }

    /// The luminance in [`LUMA_UNIT`](Self::LUMA_UNIT) steps for cd/m².
    pub fn luma_from_f64(v: f64) -> u32 {
        (v / Self::LUMA_UNIT).round().clamp(0.0, u32::MAX as f64) as u32
    }
}

/// CTA-861.3 content light level (MP4 `clli`, Matroska `MaxCLL`/`MaxFALL`),
/// in cd/m². `0` means unknown in either field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ContentLightLevel {
    /// Maximum content light level.
    pub max_cll: u16,
    /// Maximum frame-average light level.
    pub max_fall: u16,
}

/// Clean aperture (MP4/QuickTime `clap`): the picture region meant for
/// display, as exact rationals `(numerator, denominator)`. The offsets are of
/// the aperture's centre from the picture's centre, so `(0, 1)` is centred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CleanAperture {
    pub width: (u32, u32),
    pub height: (u32, u32),
    pub horiz_offset: (i32, u32),
    pub vert_offset: (i32, u32),
}

/// A start timecode, as a QuickTime timecode track (`tmcd`) carries it: the
/// frame number of the first frame, counted at the nominal rate.
///
/// The frame number is the source of truth; [`fmt::Display`](std::fmt::Display)
/// renders it as `HH:MM:SS:FF`, with `;` before the frames when drop-frame.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Timecode {
    /// Frame number of the first frame.
    pub start_frame: u32,
    /// Nominal frames per second (30 for 29.97, 24 for 23.976).
    pub frames_per_second: u8,
    /// Drop-frame counting (29.97 / 59.94 NTSC).
    pub drop_frame: bool,
    /// The exact frame rate as `timescale / frame_duration` (30000/1001,
    /// 25/1, ...). A timecode track stores both.
    pub timescale: u32,
    pub frame_duration: u32,
    /// Other QuickTime `tmcd` flag bits (0x2 24-hour max, 0x4 negative times
    /// OK, 0x8 counter); drop-frame is [`drop_frame`](Self::drop_frame).
    pub flags: u32,
    /// The source reel name (`tmcd`'s `name` box), when recorded.
    pub reel_name: Option<String>,
}

impl Timecode {
    /// A non-drop timecode at an integer rate starting at `start_frame`.
    pub fn new(start_frame: u32, frames_per_second: u8) -> Timecode {
        let fps = u32::from(frames_per_second.max(1));
        Timecode {
            start_frame,
            frames_per_second: frames_per_second.max(1),
            drop_frame: false,
            timescale: fps,
            frame_duration: 1,
            flags: 0,
            reel_name: None,
        }
    }

    /// Parse `HH:MM:SS:FF` (or `HH:MM:SS;FF` for drop-frame) at the given
    /// nominal rate. Drop-frame needs a rate that is a multiple of 30.
    pub fn parse(text: &str, frames_per_second: u8) -> Option<Timecode> {
        let drop = text.contains(';');
        let parts: Vec<u32> = text
            .split([':', ';', '.'])
            .map(|p| p.trim().parse().ok())
            .collect::<Option<Vec<u32>>>()?;
        let [h, m, s, f] = parts[..] else { return None };
        let fps = u32::from(frames_per_second.max(1));
        if m > 59 || s > 59 || f >= fps || (drop && fps % 30 != 0) {
            return None;
        }
        let mut frame = ((h * 60 + m) * 60 + s) * fps + f;
        if drop {
            let per_min = 2 * (fps / 30);
            let minutes = h * 60 + m;
            frame -= per_min * (minutes - minutes / 10);
        }
        let mut tc = Timecode::new(frame, frames_per_second);
        tc.drop_frame = drop;
        if drop {
            tc.timescale = fps * 1000;
            tc.frame_duration = 1001;
        }
        Some(tc)
    }

    /// `(hours, minutes, seconds, frames)` of [`start_frame`](Self::start_frame).
    pub fn components(&self) -> (u32, u32, u32, u32) {
        let fps = u64::from(self.frames_per_second.max(1));
        let mut f = u64::from(self.start_frame);
        if self.drop_frame && fps % 30 == 0 {
            // SMPTE 12M: drop `d` frame numbers each minute except every tenth.
            let d = 2 * (fps / 30);
            let per_10min = fps * 600 - 9 * d;
            let per_min = fps * 60 - d;
            let tens = f / per_10min;
            let rem = f % per_10min;
            f += 9 * d * tens
                + if rem > d {
                    d * ((rem - d) / per_min)
                } else {
                    0
                };
        }
        let frames = f % fps;
        let secs = f / fps;
        (
            (secs / 3600 % 24) as u32,
            (secs / 60 % 60) as u32,
            (secs % 60) as u32,
            frames as u32,
        )
    }
}

impl std::fmt::Display for Timecode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (h, m, s, fr) = self.components();
        let sep = if self.drop_frame { ';' } else { ':' };
        write!(f, "{h:02}:{m:02}:{s:02}{sep}{fr:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_drop_timecode_round_trips_through_text() {
        let tc = Timecode::parse("01:00:00:00", 25).unwrap();
        assert_eq!(tc.start_frame, 90_000);
        assert_eq!(tc.to_string(), "01:00:00:00");
        let tc = Timecode::parse("10:59:59:23", 24).unwrap();
        assert_eq!(tc.to_string(), "10:59:59:23");
    }

    #[test]
    fn drop_frame_skips_two_numbers_a_minute_except_every_tenth() {
        // 00:01:00;02 is the first label after the skip at minute 1.
        let tc = Timecode::parse("00:01:00;02", 30).unwrap();
        assert_eq!(tc.start_frame, 1800);
        assert_eq!(tc.to_string(), "00:01:00;02");
        // Minute 10 keeps its ;00 and ;01.
        let tc = Timecode::parse("00:10:00;00", 30).unwrap();
        assert_eq!(tc.start_frame, 17_982);
        assert_eq!(tc.to_string(), "00:10:00;00");
        // One hour of 29.97 drop-frame is 107,892 frames.
        assert_eq!(
            Timecode::parse("01:00:00;00", 30).unwrap().start_frame,
            107_892
        );
        for frame in [
            0u32, 1799, 1800, 17_981, 17_982, 107_891, 107_892, 1_000_003,
        ] {
            let mut tc = Timecode::new(frame, 30);
            tc.drop_frame = true;
            let back = Timecode::parse(&tc.to_string(), 30).unwrap();
            assert_eq!(back.start_frame, frame, "{tc}");
        }
    }

    #[test]
    fn invalid_timecodes_are_refused() {
        assert!(Timecode::parse("00:00:00:25", 25).is_none());
        assert!(Timecode::parse("00:60:00:00", 25).is_none());
        assert!(Timecode::parse("00:00:00;00", 25).is_none());
        assert!(Timecode::parse("00:00:00", 25).is_none());
    }

    #[test]
    fn colour_defaults_to_unspecified() {
        assert!(ColorInfo::default().is_unspecified());
        assert_eq!(MasteringDisplay::chroma_from_f64(0.3127), 15_635);
        assert_eq!(MasteringDisplay::luma_from_f64(1000.0), 10_000_000);
    }
}
