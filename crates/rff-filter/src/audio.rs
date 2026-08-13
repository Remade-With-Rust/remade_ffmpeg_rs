//! Audio filters — the `-af` / `-filter:a` chain.
//!
//! An [`AudioFilterChain`] transforms decoded [`AudioFrame`]s between the
//! decoder and the encoder, mirroring [`crate::FilterChain`] for video. The
//! supported set is deliberately the high-traffic core:
//!
//! * `volume=0.5` / `volume=-6dB` — gain (linear factor or decibels),
//! * `atrim=start=1:end=3.5` (also positional `atrim=1:3.5`, `duration=`) —
//!   sample-accurate trimming against the frame's presentation time,
//! * `aresample=48000` — resampling, surfaced via
//!   [`AudioFilterChain::resample_target`] and performed by the engine's
//!   resampler (the same one implicit rate conversion uses),
//! * `anull` — pass-through.
//!
//! Frames are interleaved `s16`/`f32`; planar layouts are rejected rather than
//! silently mis-sliced.

use rff_core::{AudioFrame, Error, Result, SampleFormat};

enum AFilter {
    Volume(f32),
    ATrim { start: Option<f64>, end: Option<f64> },
}

/// An ordered `-af` chain plus the resample target it requests, if any.
#[derive(Default)]
pub struct AudioFilterChain {
    filters: Vec<AFilter>,
    resample_to: Option<u32>,
}

/// Parse `key=value` or bare-positional arguments of `atrim`.
fn parse_atrim(args: &[&str]) -> Result<AFilter> {
    let (mut start, mut end, mut duration) = (None, None, None);
    let mut positional = 0usize;
    for a in args {
        let a = a.trim();
        if a.is_empty() {
            continue;
        }
        let (key, value) = match a.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => {
                let key = match positional {
                    0 => "start",
                    _ => "end",
                };
                positional += 1;
                (key, a)
            }
        };
        let v: f64 = value
            .parse()
            .map_err(|_| Error::Option(format!("atrim: bad number `{value}`")))?;
        match key {
            "start" => start = Some(v),
            "end" => end = Some(v),
            "duration" => duration = Some(v),
            other => return Err(Error::Option(format!("atrim: unknown key `{other}`"))),
        }
    }
    if let (None, Some(d)) = (end, duration) {
        end = Some(start.unwrap_or(0.0) + d);
    }
    Ok(AFilter::ATrim { start, end })
}

/// Parse a volume value: a linear factor (`0.5`) or decibels (`-6dB`).
fn parse_volume(value: &str) -> Result<f32> {
    let value = value.trim();
    if let Some(db) = value
        .strip_suffix("dB")
        .or_else(|| value.strip_suffix("db"))
    {
        let db: f32 = db
            .trim()
            .parse()
            .map_err(|_| Error::Option(format!("volume: bad dB value `{value}`")))?;
        return Ok(10f32.powf(db / 20.0));
    }
    value
        .parse()
        .map_err(|_| Error::Option(format!("volume: bad value `{value}`")))
}

impl AudioFilterChain {
    /// Parse an FFmpeg-style `-af` spec. Empty/blank yields a pass-through chain.
    pub fn parse(spec: &str) -> Result<AudioFilterChain> {
        let mut chain = AudioFilterChain::default();
        for token in spec.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let (name, args) = token.split_once('=').unwrap_or((token, ""));
            let parts: Vec<&str> = if args.is_empty() {
                Vec::new()
            } else {
                args.split(':').collect()
            };
            match name {
                "volume" => {
                    let v = parts
                        .first()
                        .ok_or_else(|| Error::Option("volume: missing value".into()))?;
                    chain.filters.push(AFilter::Volume(parse_volume(v)?));
                }
                "atrim" => chain.filters.push(parse_atrim(&parts)?),
                "aresample" => {
                    // `aresample=48000` or `aresample=osr=48000`.
                    let v = parts
                        .first()
                        .map(|p| p.trim_start_matches("osr=").trim())
                        .ok_or_else(|| Error::Option("aresample: missing rate".into()))?;
                    let rate: u32 = v
                        .parse()
                        .map_err(|_| Error::Option(format!("aresample: bad rate `{v}`")))?;
                    chain.resample_to = Some(rate);
                }
                "anull" => {}
                other => {
                    return Err(Error::unsupported(format!(
                        "unknown audio filter `{other}` (have: volume, atrim, aresample, anull)"
                    )))
                }
            }
        }
        Ok(chain)
    }

    /// True when no per-frame work is queued (`aresample` alone still counts as
    /// empty here: the engine's resampler performs it).
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    /// The output sample rate an `aresample` in the chain asked for.
    pub fn resample_target(&self) -> Option<u32> {
        self.resample_to
    }

    /// Run the chain on one frame. `t` is the frame's start time in seconds
    /// (output-relative). Returns `None` when trimming consumed the whole frame.
    pub fn apply(&mut self, mut frame: AudioFrame, t: Option<f64>) -> Result<Option<AudioFrame>> {
        for f in &self.filters {
            frame = match f {
                AFilter::Volume(gain) => {
                    apply_volume(&mut frame, *gain)?;
                    frame
                }
                AFilter::ATrim { start, end } => match apply_atrim(frame, t, *start, *end)? {
                    Some(f) => f,
                    None => return Ok(None),
                },
            };
        }
        Ok(Some(frame))
    }
}

fn apply_volume(frame: &mut AudioFrame, gain: f32) -> Result<()> {
    match frame.format {
        SampleFormat::F32 => {
            for chunk in frame.planes[0].chunks_exact_mut(4) {
                let v = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) * gain;
                chunk.copy_from_slice(&v.to_le_bytes());
            }
            Ok(())
        }
        SampleFormat::S16 => {
            for chunk in frame.planes[0].chunks_exact_mut(2) {
                let v = i16::from_le_bytes([chunk[0], chunk[1]]) as f32 * gain;
                let v = v.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                chunk.copy_from_slice(&v.to_le_bytes());
            }
            Ok(())
        }
        other => Err(Error::unsupported(format!(
            "volume: sample format `{}` (interleaved s16/f32 only)",
            other.name()
        ))),
    }
}

/// Sample-accurate trim: keep the intersection of this frame with
/// `[start, end)`. The frame's start time `t` must be known. Public because
/// the engine's `-ss`/`-t` window uses the same slice for audio — an audio
/// "frame" can be arbitrarily large (a WAV decodes as one), so keep/drop at
/// frame granularity would cut wildly wrong.
pub fn trim_audio_frame(
    frame: AudioFrame,
    t: Option<f64>,
    start: Option<f64>,
    end: Option<f64>,
) -> Result<Option<AudioFrame>> {
    apply_atrim(frame, t, start, end)
}

fn apply_atrim(
    frame: AudioFrame,
    t: Option<f64>,
    start: Option<f64>,
    end: Option<f64>,
) -> Result<Option<AudioFrame>> {
    let t = t.ok_or_else(|| Error::unsupported("atrim: input has no timestamps"))?;
    let rate = frame.sample_rate.max(1) as f64;
    let n = frame.samples;
    let s0 = match start {
        Some(s) if s > t => (((s - t) * rate).round() as usize).min(n),
        _ => 0,
    };
    let s1 = match end {
        Some(e) => {
            let upto = ((e - t) * rate).round();
            if upto <= 0.0 {
                0
            } else {
                (upto as usize).min(n)
            }
        }
        None => n,
    };
    if s1 <= s0 {
        return Ok(None);
    }
    if s0 == 0 && s1 == n {
        return Ok(Some(frame));
    }
    let bytes_per = frame.format.bytes_per_sample() * frame.channels.max(1) as usize;
    if frame.format == SampleFormat::F32Planar {
        return Err(Error::unsupported("atrim: planar audio not supported"));
    }
    let plane = &frame.planes[0][s0 * bytes_per..s1 * bytes_per];
    Ok(Some(AudioFrame {
        sample_rate: frame.sample_rate,
        channels: frame.channels,
        format: frame.format,
        samples: s1 - s0,
        planes: vec![plane.to_vec()],
        pts: frame.pts, // pts unit is the caller's; the engine tracks time via `t`
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s16_frame(samples: &[i16], rate: u32, channels: u16) -> AudioFrame {
        AudioFrame {
            sample_rate: rate,
            channels,
            format: SampleFormat::S16,
            samples: samples.len() / channels.max(1) as usize,
            planes: vec![samples.iter().flat_map(|s| s.to_le_bytes()).collect()],
            pts: Some(0),
        }
    }

    #[test]
    fn volume_scales_and_clamps_s16() {
        let mut chain = AudioFilterChain::parse("volume=2.0").unwrap();
        let f = s16_frame(&[100, -100, 30000], 48_000, 1);
        let out = chain.apply(f, Some(0.0)).unwrap().unwrap();
        let got: Vec<i16> = out.planes[0]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(got, vec![200, -200, 32767]); // clamped, not wrapped
    }

    #[test]
    fn volume_parses_decibels() {
        let g = parse_volume("-6dB").unwrap();
        assert!((g - 0.501).abs() < 0.01, "got {g}");
        assert_eq!(parse_volume("0dB").unwrap(), 1.0);
    }

    #[test]
    fn atrim_slices_sample_accurately() {
        // 1 kHz-rate mono frame starting at t=1.0 with 1000 samples (1 s).
        let samples: Vec<i16> = (0..1000).map(|i| i as i16).collect();
        let mut chain = AudioFilterChain::parse("atrim=start=1.25:end=1.5").unwrap();
        let out = chain
            .apply(s16_frame(&samples, 1000, 1), Some(1.0))
            .unwrap()
            .unwrap();
        assert_eq!(out.samples, 250);
        let first = i16::from_le_bytes([out.planes[0][0], out.planes[0][1]]);
        assert_eq!(first, 250);
    }

    #[test]
    fn atrim_consumes_frames_outside_the_window() {
        let mut chain = AudioFilterChain::parse("atrim=end=0.5").unwrap();
        let out = chain
            .apply(s16_frame(&[1, 2, 3, 4], 8000, 1), Some(2.0))
            .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn aresample_is_surfaced_not_applied() {
        let chain = AudioFilterChain::parse("aresample=44100").unwrap();
        assert_eq!(chain.resample_target(), Some(44_100));
        assert!(chain.is_empty()); // engine's resampler does the work
    }

    #[test]
    fn unknown_filters_error() {
        assert!(AudioFilterChain::parse("acompressor").is_err());
    }
}
