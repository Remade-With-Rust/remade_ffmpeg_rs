use super::lossless::LosslessDecoder;
use crate::decoder::DecodingError;
use byteorder_lite::ReadBytesExt;
use std::io::{BufRead, Read};

use crate::alpha_blending::do_alpha_blending;

#[derive(Debug, Clone)]
pub(crate) struct WebPExtendedInfo {
    pub(crate) alpha: bool,

    pub(crate) canvas_width: u32,
    pub(crate) canvas_height: u32,

    #[allow(unused)]
    pub(crate) icc_profile: bool,
    pub(crate) exif_metadata: bool,
    pub(crate) xmp_metadata: bool,
    pub(crate) animation: bool,

    pub(crate) background_color: Option<[u8; 4]>,
    pub(crate) background_color_hint: [u8; 4],
}

/// Composites a frame onto a canvas.
///
/// Starts by filling the rectangle occupied by the previous frame with the background
/// color, if provided. Then copies or blends the frame onto the canvas.
#[allow(clippy::too_many_arguments)]
pub(crate) fn composite_frame(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    clear_color: Option<[u8; 4]>,
    frame: &[u8],
    frame_offset_x: u32,
    frame_offset_y: u32,
    frame_width: u32,
    frame_height: u32,
    frame_has_alpha: bool,
    frame_use_alpha_blending: bool,
    previous_frame_width: u32,
    previous_frame_height: u32,
    previous_frame_offset_x: u32,
    previous_frame_offset_y: u32,
) {
    let frame_is_full_size = frame_offset_x == 0
        && frame_offset_y == 0
        && frame_width == canvas_width
        && frame_height == canvas_height;

    if frame_is_full_size && !frame_use_alpha_blending {
        if frame_has_alpha {
            canvas.copy_from_slice(frame);
        } else {
            for (input, output) in frame.chunks_exact(3).zip(canvas.chunks_exact_mut(4)) {
                output[..3].copy_from_slice(input);
                output[3] = 255;
            }
        }
        return;
    }

    // clear rectangle occupied by previous frame
    if let Some(clear_color) = clear_color {
        match (frame_is_full_size, frame_has_alpha) {
            (true, true) => {
                for pixel in canvas.chunks_exact_mut(4) {
                    pixel.copy_from_slice(&clear_color);
                }
            }
            (true, false) => {
                for pixel in canvas.chunks_exact_mut(3) {
                    pixel.copy_from_slice(&clear_color[..3]);
                }
            }
            (false, true) => {
                for y in 0..previous_frame_height as usize {
                    for x in 0..previous_frame_width as usize {
                        let canvas_index = ((x + previous_frame_offset_x as usize)
                            + (y + previous_frame_offset_y as usize) * canvas_width as usize)
                            * 4;

                        let output = &mut canvas[canvas_index..][..4];
                        output.copy_from_slice(&clear_color);
                    }
                }
            }
            (false, false) => {
                for y in 0..previous_frame_height as usize {
                    for x in 0..previous_frame_width as usize {
                        // let frame_index = (x + y * frame_width as usize) * 4;
                        let canvas_index = ((x + previous_frame_offset_x as usize)
                            + (y + previous_frame_offset_y as usize) * canvas_width as usize)
                            * 3;

                        let output = &mut canvas[canvas_index..][..3];
                        output.copy_from_slice(&clear_color[..3]);
                    }
                }
            }
        }
    }

    let width = frame_width.min(canvas_width.saturating_sub(frame_offset_x)) as usize;
    let height = frame_height.min(canvas_height.saturating_sub(frame_offset_y)) as usize;

    if frame_has_alpha && frame_use_alpha_blending {
        // One row of the frame against one row of the canvas: two slices
        // walked together, instead of two index computations and two
        // bounds-checked sub-slices for every pixel.
        for y in 0..height {
            let frame_index = (y * frame_width as usize) * 4;
            let canvas_index = (frame_offset_x as usize
                + (y + frame_offset_y as usize) * canvas_width as usize)
                * 4;
            let input = &frame[frame_index..][..width * 4];
            let output = &mut canvas[canvas_index..][..width * 4];

            for (src, dst) in input.chunks_exact(4).zip(output.chunks_exact_mut(4)) {
                let src: [u8; 4] = src.try_into().unwrap();
                let under: [u8; 4] = (&*dst).try_into().unwrap();
                dst.copy_from_slice(&do_alpha_blending(src, under));
            }
        }
    } else if frame_has_alpha {
        for y in 0..height {
            let frame_index = (y * frame_width as usize) * 4;
            let canvas_index = (frame_offset_x as usize
                + (y + frame_offset_y as usize) * canvas_width as usize)
                * 4;

            canvas[canvas_index..][..width * 4].copy_from_slice(&frame[frame_index..][..width * 4]);
        }
    } else {
        for y in 0..height {
            let index = (y * frame_width as usize) * 3;
            let canvas_index = (frame_offset_x as usize
                + (y + frame_offset_y as usize) * canvas_width as usize)
                * 4;
            let input = &frame[index..][..width * 3];
            let output = &mut canvas[canvas_index..][..width * 4];

            for (input, output) in input.chunks_exact(3).zip(output.chunks_exact_mut(4)) {
                output[..3].copy_from_slice(input);
                output[3] = 255;
            }
        }
    }
}

/// Undo the alpha plane's prediction filter and store the result in the
/// alpha byte of every pixel of `rgba` (rows of `width` RGBA pixels).
/// `alpha` holds the filtered values: value `i` is byte `AT` of the `i`-th
/// `STEP`-byte group, so a plane of single bytes is `<1, 0>` and the green
/// channel of a decoded four-byte-per-pixel image is `<4, 1>` (no separate
/// plane has to be extracted first).
///
/// The filter method is fixed for the plane, and its edge rules depend only
/// on the row and on whether the pixel is the first of its row, so the
/// method is matched once and each row runs a plain loop.
/// [`get_alpha_predictor`] is the same rule one pixel at a time, kept as the
/// test oracle.
pub(crate) fn apply_alpha_plane<const STEP: usize, const AT: usize>(
    rgba: &mut [u8],
    width: usize,
    alpha: &[u8],
    filtering_method: FilteringMethod,
) {
    if width == 0 {
        return;
    }
    let stride = width * 4;

    // A row predicted from the left: each pixel adds to the previous result.
    // `first` is the predictor of the row's first pixel.
    fn from_left<const STEP: usize, const AT: usize>(row: &mut [u8], deltas: &[u8], first: u8) {
        let mut prev = first;
        for (px, d) in row.chunks_exact_mut(4).zip(deltas.chunks_exact(STEP)) {
            prev = prev.wrapping_add(d[AT]);
            px[3] = prev;
        }
    }

    match filtering_method {
        FilteringMethod::None => {
            for (px, a) in rgba.chunks_exact_mut(4).zip(alpha.chunks_exact(STEP)) {
                px[3] = a[AT];
            }
        }
        FilteringMethod::Horizontal => {
            // the first pixel of a row is predicted from the pixel above it
            let mut above = 0u8;
            for (row, deltas) in rgba
                .chunks_exact_mut(stride)
                .zip(alpha.chunks_exact(width * STEP))
            {
                from_left::<STEP, AT>(row, deltas, above);
                above = row[3];
            }
        }
        FilteringMethod::Vertical | FilteringMethod::Gradient => {
            let gradient = matches!(filtering_method, FilteringMethod::Gradient);
            let mut rows = alpha.chunks_exact(width * STEP);
            // the first row has no row above: it is predicted from the left
            let Some(deltas) = rows.next() else { return };
            let Some(first_row) = rgba.get_mut(..stride) else {
                return;
            };
            from_left::<STEP, AT>(first_row, deltas, 0);

            let mut done = stride;
            for deltas in rows {
                let (before, rest) = rgba.split_at_mut(done);
                let Some(row) = rest.get_mut(..stride) else {
                    return;
                };
                let top = &before[done - stride..];
                if gradient {
                    // first pixel: the one above; then left + top - top-left
                    let mut left = top[3].wrapping_add(deltas[AT]);
                    row[3] = left;
                    for ((px, t), d) in row[4..]
                        .chunks_exact_mut(4)
                        .zip(top.windows(8).step_by(4))
                        .zip(deltas[STEP..].chunks_exact(STEP))
                    {
                        let predictor = i16::from(left) + i16::from(t[7]) - i16::from(t[3]);
                        left = (predictor.clamp(0, 255) as u8).wrapping_add(d[AT]);
                        px[3] = left;
                    }
                } else {
                    for ((px, t), d) in row
                        .chunks_exact_mut(4)
                        .zip(top.chunks_exact(4))
                        .zip(deltas.chunks_exact(STEP))
                    {
                        px[3] = t[3].wrapping_add(d[AT]);
                    }
                }
                done += stride;
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn get_alpha_predictor(
    x: usize,
    y: usize,
    width: usize,
    filtering_method: FilteringMethod,
    image_slice: &[u8],
) -> u8 {
    match filtering_method {
        FilteringMethod::None => 0,
        FilteringMethod::Horizontal => {
            if x == 0 && y == 0 {
                0
            } else if x == 0 {
                let index = (y - 1) * width + x;
                image_slice[index * 4 + 3]
            } else {
                let index = y * width + x - 1;
                image_slice[index * 4 + 3]
            }
        }
        FilteringMethod::Vertical => {
            if x == 0 && y == 0 {
                0
            } else if y == 0 {
                let index = y * width + x - 1;
                image_slice[index * 4 + 3]
            } else {
                let index = (y - 1) * width + x;
                image_slice[index * 4 + 3]
            }
        }
        FilteringMethod::Gradient => {
            let (left, top, top_left) = match (x, y) {
                (0, 0) => (0, 0, 0),
                (0, y) => {
                    let above_index = (y - 1) * width + x;
                    let val = image_slice[above_index * 4 + 3];
                    (val, val, val)
                }
                (x, 0) => {
                    let before_index = y * width + x - 1;
                    let val = image_slice[before_index * 4 + 3];
                    (val, val, val)
                }
                (x, y) => {
                    let left_index = y * width + x - 1;
                    let left = image_slice[left_index * 4 + 3];
                    let top_index = (y - 1) * width + x;
                    let top = image_slice[top_index * 4 + 3];
                    let top_left_index = (y - 1) * width + x - 1;
                    let top_left = image_slice[top_left_index * 4 + 3];

                    (left, top, top_left)
                }
            };

            let combination = i16::from(left) + i16::from(top) - i16::from(top_left);
            i16::clamp(combination, 0, 255).try_into().unwrap()
        }
    }
}

pub(crate) fn read_extended_header<R: Read>(
    reader: &mut R,
) -> Result<WebPExtendedInfo, DecodingError> {
    let chunk_flags = reader.read_u8()?;

    let icc_profile = chunk_flags & 0b00100000 != 0;
    let alpha = chunk_flags & 0b00010000 != 0;
    let exif_metadata = chunk_flags & 0b00001000 != 0;
    let xmp_metadata = chunk_flags & 0b00000100 != 0;
    let animation = chunk_flags & 0b00000010 != 0;

    // reserved bytes are ignored
    let _reserved_bytes = read_3_bytes(reader)?;

    let canvas_width = read_3_bytes(reader)? + 1;
    let canvas_height = read_3_bytes(reader)? + 1;

    //product of canvas dimensions cannot be larger than u32 max
    if u32::checked_mul(canvas_width, canvas_height).is_none() {
        return Err(DecodingError::ImageTooLarge);
    }

    let info = WebPExtendedInfo {
        icc_profile,
        alpha,
        exif_metadata,
        xmp_metadata,
        animation,
        canvas_width,
        canvas_height,
        background_color_hint: [0; 4],
        background_color: None,
    };

    Ok(info)
}

pub(crate) fn read_3_bytes<R: Read>(reader: &mut R) -> Result<u32, DecodingError> {
    let mut buffer: [u8; 3] = [0; 3];
    reader.read_exact(&mut buffer)?;
    let value: u32 =
        (u32::from(buffer[2]) << 16) | (u32::from(buffer[1]) << 8) | u32::from(buffer[0]);
    Ok(value)
}

#[derive(Debug)]
pub(crate) struct AlphaChunk {
    _preprocessing: bool,
    pub(crate) filtering_method: FilteringMethod,
    pub(crate) data: Vec<u8>,
    /// `data` is a decoded four-byte-per-pixel image with the alpha values
    /// in its green channel, rather than one byte per pixel.
    in_green: bool,
}

impl AlphaChunk {
    /// Unfilter the plane into the alpha byte of every pixel of `rgba`.
    pub(crate) fn apply(&self, rgba: &mut [u8], width: usize) {
        if self.in_green {
            apply_alpha_plane::<4, 1>(rgba, width, &self.data, self.filtering_method);
        } else {
            apply_alpha_plane::<1, 0>(rgba, width, &self.data, self.filtering_method);
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub(crate) enum FilteringMethod {
    None,
    Horizontal,
    Vertical,
    Gradient,
}

pub(crate) fn read_alpha_chunk<R: BufRead>(
    reader: &mut R,
    width: u16,
    height: u16,
) -> Result<AlphaChunk, DecodingError> {
    let info_byte = reader.read_u8()?;

    let preprocessing = (info_byte & 0b00110000) >> 4;
    let filtering = (info_byte & 0b00001100) >> 2;
    let compression = info_byte & 0b00000011;

    let preprocessing = match preprocessing {
        0 => false,
        1 => true,
        _ => return Err(DecodingError::InvalidAlphaPreprocessing),
    };

    let filtering_method = match filtering {
        0 => FilteringMethod::None,
        1 => FilteringMethod::Horizontal,
        2 => FilteringMethod::Vertical,
        3 => FilteringMethod::Gradient,
        _ => unreachable!(),
    };

    let lossless_compression = match compression {
        0 => false,
        1 => true,
        _ => return Err(DecodingError::InvalidCompressionMethod),
    };

    let data = if lossless_compression {
        let mut decoder = LosslessDecoder::new(reader);

        let mut data = vec![0; usize::from(width) * usize::from(height) * 4];
        decoder.decode_frame(u32::from(width), u32::from(height), true, &mut data)?;

        // The alpha values are the green channel of these pixels: they are
        // read from there when the plane is applied.
        data
    } else {
        let mut framedata = vec![0; width as usize * height as usize];
        reader.read_exact(&mut framedata)?;
        framedata
    };

    let chunk = AlphaChunk {
        _preprocessing: preprocessing,
        filtering_method,
        data,
        in_green: lossless_compression,
    };

    Ok(chunk)
}

#[cfg(test)]
mod alpha_tests {
    use super::*;

    /// The row-based unfilter must give what the per-pixel predictor gives,
    /// for every method and for awkward sizes.
    #[test]
    fn alpha_plane_matches_per_pixel_predictor() {
        let mut seed = 0x853c_49e6_748f_ea9bu64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 24) as u8
        };
        for &(width, height) in &[
            (1usize, 1usize),
            (1, 5),
            (5, 1),
            (2, 2),
            (7, 3),
            (16, 9),
            (33, 17),
        ] {
            let alpha: Vec<u8> = (0..width * height).map(|_| next()).collect();
            for method in [
                FilteringMethod::None,
                FilteringMethod::Horizontal,
                FilteringMethod::Vertical,
                FilteringMethod::Gradient,
            ] {
                let image: Vec<u8> = (0..width * height * 4).map(|_| next()).collect();
                let mut want = image.clone();
                for y in 0..height {
                    for x in 0..width {
                        let predictor = get_alpha_predictor(x, y, width, method, &want);
                        let i = y * width + x;
                        want[i * 4 + 3] = predictor.wrapping_add(alpha[i]);
                    }
                }
                let mut got = image.clone();
                apply_alpha_plane::<1, 0>(&mut got, width, &alpha, method);
                assert_eq!(got, want, "{width}x{height}");

                // the same values as the green channel of four-byte pixels
                let pixels: Vec<u8> = alpha
                    .iter()
                    .flat_map(|&a| [next(), a, next(), next()])
                    .collect();
                let mut got = image;
                apply_alpha_plane::<4, 1>(&mut got, width, &pixels, method);
                assert_eq!(got, want, "{width}x{height} from green");
            }
        }
    }
}
