//! Joint-stereo reconstruction: MS (mid/side) and intensity stereo.
//!
//! Active only when the header mode is JointStereo; `mode_extension` says which
//! of MS / intensity is on. MS rotates (mid, side) → (L, R) by `1/√2`:
//! `L = (M+S)/√2`, `R = (M-S)/√2`. Intensity stereo, above the intensity bound,
//! reconstructs the right channel from the left using a per-band position.

use crate::frame::{ChannelMode, GranuleSideInfo, GranuleSpectrum, GRANULE_LINES};
use crate::header::FrameHeader;

/// Convert the two coded channels in place from the joint representation back to
/// independent left/right.
pub fn process(header: &FrameHeader, _gi: &[GranuleSideInfo; 2], spectrum: &mut GranuleSpectrum) {
    let (ms, intensity) = match header.channel_mode {
        ChannelMode::JointStereo {
            ms_stereo,
            intensity_stereo,
        } => (ms_stereo, intensity_stereo),
        _ => return, // plain stereo / mono: nothing to undo
    };

    // brick: intensity stereo — find the intensity bound (first all-zero band of
    // the right channel), then pan each band by its is_pos (the right channel's
    // scalefactor), MPEG-1 tan-weighted. MS below the bound is handled here.
    let _ = intensity;

    if ms {
        let inv_sqrt2 = std::f32::consts::FRAC_1_SQRT_2;
        let (a, b) = spectrum.lines.split_at_mut(1);
        for i in 0..GRANULE_LINES {
            let m = a[0][i];
            let s = b[0][i];
            a[0][i] = (m + s) * inv_sqrt2;
            b[0][i] = (m - s) * inv_sqrt2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::MpegVersion;

    fn joint(ms: bool, is: bool) -> FrameHeader {
        FrameHeader {
            version: MpegVersion::V1,
            crc_protected: false,
            bitrate_kbps: 128,
            sample_rate: 44100,
            padding: false,
            channel_mode: ChannelMode::JointStereo {
                ms_stereo: ms,
                intensity_stereo: is,
            },
            copyright: false,
            original: true,
            emphasis: 0,
        }
    }

    #[test]
    fn ms_stereo_rotation() {
        let mut spec = GranuleSpectrum::default();
        spec.lines[0][0] = 1.0; // M
        spec.lines[1][0] = 1.0; // S
        process(
            &joint(true, false),
            &[GranuleSideInfo::default(), GranuleSideInfo::default()],
            &mut spec,
        );
        // L = (1+1)/√2 = √2, R = (1-1)/√2 = 0.
        assert!((spec.lines[0][0] - 2f32.sqrt()).abs() < 1e-6);
        assert!(spec.lines[1][0].abs() < 1e-6);
    }

    #[test]
    fn plain_stereo_is_untouched() {
        let mut spec = GranuleSpectrum::default();
        spec.lines[0][0] = 0.7;
        spec.lines[1][0] = 0.3;
        let mut h = joint(true, false);
        h.channel_mode = ChannelMode::Stereo;
        process(
            &h,
            &[GranuleSideInfo::default(), GranuleSideInfo::default()],
            &mut spec,
        );
        assert_eq!(spec.lines[0][0], 0.7);
        assert_eq!(spec.lines[1][0], 0.3);
    }
}
