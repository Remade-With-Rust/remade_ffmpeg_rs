#![allow(clippy::identity_op)]

use alloc::vec::Vec;

use crate::encode::encoder::JpegColorType;

/// Conversion from RGB to YCbCr
#[inline]
pub fn rgb_to_ycbcr(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    // To avoid floating point math this scales everything by 2^16 which gives
    // a precision of approx 4 digits.
    //
    // Non scaled conversion:
    // Y  =  0.29900 * R + 0.58700 * G + 0.11400 * B
    // Cb = -0.16874 * R - 0.33126 * G + 0.50000 * B  + 128
    // Cr =  0.50000 * R - 0.41869 * G - 0.08131 * B  + 128

    let r = r as i32;
    let g = g as i32;
    let b = b as i32;

    let y = 19595 * r + 38470 * g + 7471 * b;
    let cb = -11059 * r - 21709 * g + 32768 * b + (128 << 16);
    let cr = 32768 * r - 27439 * g - 5329 * b + (128 << 16);

    let y = (y + 0x7FFF) >> 16;
    let cb = (cb + 0x7FFF) >> 16;
    let cr = (cr + 0x7FFF) >> 16;

    (y as u8, cb as u8, cr as u8)
}

/// Conversion from CMYK to YCCK (YCbCrK)
#[inline]
pub fn cmyk_to_ycck(c: u8, m: u8, y: u8, k: u8) -> (u8, u8, u8, u8) {
    let (y, cb, cr) = rgb_to_ycbcr(c, m, y);
    (y, cb, cr, 255 - k)
}

/// # Buffer used as input value for image encoding
///
/// Image encoding with [Encoder::encode_image](crate::encode::Encoder::encode_image) needs an ImageBuffer
/// as input for the image data. For convenience the [Encoder::encode](crate::encode::Encoder::encode)
/// function contains implementations for common byte based pixel formats.
/// Users that needs other pixel formats or don't have the data available as byte slices
/// can create their own buffer implementations.
///
/// ## Example: ImageBuffer implementation for RgbImage from the `image` crate
/// ```no_compile
/// use image::RgbImage;
/// use rusty_jpeg::encode::{ImageBuffer, JpegColorType, rgb_to_ycbcr};
///
/// pub struct RgbImageBuffer {
///     image: RgbImage,
/// }
///
/// impl ImageBuffer for RgbImageBuffer {
///     fn get_jpeg_color_type(&self) -> JpegColorType {
///         // Rgb images are encoded as YCbCr in JFIF files
///         JpegColorType::Ycbcr
///     }
///
///     fn width(&self) -> u16 {
///         self.image.width() as u16
///     }
///
///     fn height(&self) -> u16 {
///         self.image.height() as u16
///     }
///
///     fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]){
///         for x in 0..self.width() {
///             let pixel = self.image.get_pixel(x as u32 ,y as u32);
///
///             let (y,cb,cr) = rgb_to_ycbcr(pixel[0], pixel[1], pixel[2]);
///
///             // For YCbCr the 4th buffer is not used
///             buffers[0].push(y);
///             buffers[1].push(cb);
///             buffers[2].push(cr);
///         }
///     }
/// }
///
/// ```
pub trait ImageBuffer {
    /// The color type used in the image encoding
    fn get_jpeg_color_type(&self) -> JpegColorType;

    /// Width of the image
    fn width(&self) -> u16;

    /// Height of the image
    fn height(&self) -> u16;

    /// Add color values for the row to color component buffers
    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]);
}

pub(crate) struct GrayImage<'a>(pub &'a [u8], pub u16, pub u16);

impl<'a> ImageBuffer for GrayImage<'a> {
    fn get_jpeg_color_type(&self) -> JpegColorType {
        JpegColorType::Luma
    }

    fn width(&self) -> u16 {
        self.1
    }

    fn height(&self) -> u16 {
        self.2
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let line = get_line(self.0, y, self.width(), 1);

        for &pixel in line {
            buffers[0].push(pixel);
        }
    }
}

#[inline(always)]
fn get_line(data: &[u8], y: u16, width: u16, num_colors: usize) -> &[u8] {
    let width = usize::from(width);
    let y = usize::from(y);

    let start = y * width * num_colors;
    let end = start + width * num_colors;

    &data[start..end]
}

macro_rules! ycbcr_image {
    ($name:ident, $num_colors:expr, $o1:expr, $o2:expr, $o3:expr) => {
        pub struct $name<'a>(pub &'a [u8], pub u16, pub u16);

        impl<'a> ImageBuffer for $name<'a> {
            fn get_jpeg_color_type(&self) -> JpegColorType {
                JpegColorType::Ycbcr
            }

            fn width(&self) -> u16 {
                self.1
            }

            fn height(&self) -> u16 {
                self.2
            }

            #[inline(always)]
            fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
                let line = get_line(self.0, y, self.width(), $num_colors);

                // Doing the convertion in chunks allows the compiler to vectorize the code
                // A size of 16 seems optimal for SSE and AVX capable hardware
                const CHUNK_SIZE: usize = 16;

                let mut y_buffer = [0; CHUNK_SIZE];
                let mut cb_buffer = [0; CHUNK_SIZE];
                let mut cr_buffer = [0; CHUNK_SIZE];

                for chuck in line.chunks_exact($num_colors * CHUNK_SIZE) {
                    for i in (0..CHUNK_SIZE) {
                        let (y, cb, cr) = rgb_to_ycbcr(
                            chuck[i * $num_colors + $o1],
                            chuck[i * $num_colors + $o2],
                            chuck[i * $num_colors + $o3],
                        );

                        y_buffer[i] = y;
                        cb_buffer[i] = cb;
                        cr_buffer[i] = cr;
                    }

                    buffers[0].extend_from_slice(&y_buffer);
                    buffers[1].extend_from_slice(&cb_buffer);
                    buffers[2].extend_from_slice(&cr_buffer);
                }

                // Add the remaining pixels in case the number of
                // pixels is not a multiple of CHUNK_SIZE
                let pixel = line.len() / $num_colors;
                for i in pixel / CHUNK_SIZE * CHUNK_SIZE..pixel {
                    let (y, cb, cr) = rgb_to_ycbcr(
                        line[i * $num_colors + $o1],
                        line[i * $num_colors + $o2],
                        line[i * $num_colors + $o3],
                    );

                    buffers[0].push(y);
                    buffers[1].push(cb);
                    buffers[2].push(cr);
                }
            }
        }
    };
}

ycbcr_image!(RgbImage, 3, 0, 1, 2);
ycbcr_image!(RgbaImage, 4, 0, 1, 2);
ycbcr_image!(BgrImage, 3, 2, 1, 0);
ycbcr_image!(BgraImage, 4, 2, 1, 0);

pub(crate) struct YCbCrImage<'a>(pub &'a [u8], pub u16, pub u16);

impl<'a> ImageBuffer for YCbCrImage<'a> {
    fn get_jpeg_color_type(&self) -> JpegColorType {
        JpegColorType::Ycbcr
    }

    fn width(&self) -> u16 {
        self.1
    }

    fn height(&self) -> u16 {
        self.2
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let line = get_line(self.0, y, self.width(), 3);

        for pixel in line.chunks_exact(3) {
            buffers[0].push(pixel[0]);
            buffers[1].push(pixel[1]);
            buffers[2].push(pixel[2]);
        }
    }
}

pub(crate) struct CmykImage<'a>(pub &'a [u8], pub u16, pub u16);

impl<'a> ImageBuffer for CmykImage<'a> {
    fn get_jpeg_color_type(&self) -> JpegColorType {
        JpegColorType::Cmyk
    }

    fn width(&self) -> u16 {
        self.1
    }

    fn height(&self) -> u16 {
        self.2
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let line = get_line(self.0, y, self.width(), 4);

        for pixel in line.chunks_exact(4) {
            buffers[0].push(255 - pixel[0]);
            buffers[1].push(255 - pixel[1]);
            buffers[2].push(255 - pixel[2]);
            buffers[3].push(255 - pixel[3]);
        }
    }
}

pub(crate) struct CmykAsYcckImage<'a>(pub &'a [u8], pub u16, pub u16);

impl<'a> ImageBuffer for CmykAsYcckImage<'a> {
    fn get_jpeg_color_type(&self) -> JpegColorType {
        JpegColorType::Ycck
    }

    fn width(&self) -> u16 {
        self.1
    }

    fn height(&self) -> u16 {
        self.2
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let line = get_line(self.0, y, self.width(), 4);

        for pixel in line.chunks_exact(4) {
            let (y, cb, cr, k) = cmyk_to_ycck(pixel[0], pixel[1], pixel[2], pixel[3]);

            buffers[0].push(y);
            buffers[1].push(cb);
            buffers[2].push(cr);
            buffers[3].push(k);
        }
    }
}

pub(crate) struct YcckImage<'a>(pub &'a [u8], pub u16, pub u16);

impl<'a> ImageBuffer for YcckImage<'a> {
    fn get_jpeg_color_type(&self) -> JpegColorType {
        JpegColorType::Ycck
    }

    fn width(&self) -> u16 {
        self.1
    }

    fn height(&self) -> u16 {
        self.2
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let line = get_line(self.0, y, self.width(), 4);

        for pixel in line.chunks_exact(4) {
            buffers[0].push(pixel[0]);
            buffers[1].push(pixel[1]);
            buffers[2].push(pixel[2]);
            buffers[3].push(pixel[3]);
        }
    }
}

/// A borrowed **planar** Y'CbCr image — the layout every video pipeline already
/// has (`yuv420p`, `yuv422p`, `yuv444p`), fed to the encoder without a detour
/// through interleaved RGB.
///
/// # Why this exists
///
/// Encoding planar 4:2:0 by first converting to RGB costs two colour
/// conversions *and* a chroma resample round-trip: the RGB conversion has to
/// upsample chroma to full resolution, and the encoder then subsamples it back
/// down. That round-trip is lossy — measured at ~29 dB chroma on a real MJPEG
/// clip — and it is pure waste, because the data started out at exactly the
/// resolution the encoder wants.
///
/// # Why it is lossless
///
/// [`ImageBuffer`] hands the encoder full-resolution rows per component and the
/// encoder subsamples with [`get_block`](super::encoder), which **point-samples**
/// (takes every Nth sample; it does not average). So replicating each chroma
/// sample `h`×`v` times here is exactly cancelled by the encoder taking every
/// `h`th/`v`th sample back: chroma arrives at the DCT bit-identical to the
/// source plane, provided the encoder's [`SamplingFactor`](super::SamplingFactor)
/// matches this image's subsampling.
///
/// That last condition is the caller's responsibility — see
/// [`sampling_factor`](PlanarYcbcrImage::sampling_factor), which returns the one
/// that matches.
pub struct PlanarYcbcrImage<'a> {
    y: &'a [u8],
    cb: &'a [u8],
    cr: &'a [u8],
    /// Row stride in bytes for each plane, in `[y, cb, cr]` order.
    strides: [usize; 3],
    width: u16,
    height: u16,
    /// Chroma subsampling: samples of luma per sample of chroma, `(h, v)`.
    /// `(1,1)` = 4:4:4, `(2,1)` = 4:2:2, `(2,2)` = 4:2:0.
    subsampling: (usize, usize),
    /// Measurement knob: use the original per-byte `push` replication. Produces
    /// identical bytes; exists only so the two can be timed head to head inside
    /// one process. See `WHYS.md` D4b.
    push_path: bool,
}

impl<'a> PlanarYcbcrImage<'a> {
    /// Wrap three planes. `subsampling` is `(horizontal, vertical)` luma samples
    /// per chroma sample: `(1,1)` 4:4:4, `(2,1)` 4:2:2, `(2,2)` 4:2:0.
    ///
    /// Returns `None` if a plane is too small for the geometry it claims, or if
    /// the subsampling is not one of the supported ratios — better to reject
    /// than to read out of bounds mid-encode.
    pub fn new(
        y: &'a [u8],
        cb: &'a [u8],
        cr: &'a [u8],
        strides: [usize; 3],
        width: u16,
        height: u16,
        subsampling: (usize, usize),
    ) -> Option<PlanarYcbcrImage<'a>> {
        let (sh, sv) = subsampling;
        if !matches!(subsampling, (1, 1) | (2, 1) | (2, 2)) {
            return None;
        }
        let (w, h) = (usize::from(width), usize::from(height));
        if w == 0 || h == 0 {
            return None;
        }
        let (cw, ch) = (w.div_ceil(sh), h.div_ceil(sv));
        // Last row only needs `width` bytes, not a full stride.
        let need = |stride: usize, rows: usize, cols: usize| {
            stride >= cols && rows > 0 && stride * (rows - 1) + cols > 0
        };
        if !need(strides[0], h, w) || !need(strides[1], ch, cw) || !need(strides[2], ch, cw) {
            return None;
        }
        if y.len() < strides[0] * (h - 1) + w
            || cb.len() < strides[1] * (ch - 1) + cw
            || cr.len() < strides[2] * (ch - 1) + cw
        {
            return None;
        }
        Some(PlanarYcbcrImage {
            y,
            cb,
            cr,
            strides,
            width,
            height,
            subsampling,
            push_path: false,
        })
    }

    /// Measurement-only: select the original per-byte `push` replication.
    pub fn set_push_path(&mut self, push: bool) {
        self.push_path = push;
    }

    /// The [`SamplingFactor`](super::SamplingFactor) the encoder must be set to
    /// for this image to encode losslessly. Using any other one re-samples the
    /// chroma and throws away the advantage of the planar path.
    pub fn sampling_factor(&self) -> super::SamplingFactor {
        use super::SamplingFactor::*;
        match self.subsampling {
            (1, 1) => R_4_4_4,
            (2, 1) => R_4_2_2,
            _ => R_4_2_0,
        }
    }
}

impl ImageBuffer for PlanarYcbcrImage<'_> {
    fn get_jpeg_color_type(&self) -> JpegColorType {
        JpegColorType::Ycbcr
    }

    fn width(&self) -> u16 {
        self.width
    }

    fn height(&self) -> u16 {
        self.height
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let w = usize::from(self.width);
        let row = usize::from(y);
        let (sh, sv) = self.subsampling;

        let y_start = row * self.strides[0];
        buffers[0].extend_from_slice(&self.y[y_start..y_start + w]);

        // Replicate each chroma sample `sh` times across, and reuse the same
        // chroma row for `sv` consecutive luma rows. The encoder's point-sampling
        // undoes both exactly.
        let crow = row / sv;
        let cw = w.div_ceil(sh);
        for (plane, stride, buf) in [
            (self.cb, self.strides[1], 1usize),
            (self.cr, self.strides[2], 2usize),
        ] {
            let start = crow * stride;
            let src = &plane[start..start + cw];
            let dst = &mut buffers[buf];
            if sh == 1 {
                dst.extend_from_slice(src);
                continue;
            }
            if self.push_path {
                for &c in src {
                    for _ in 0..sh {
                        dst.push(c);
                    }
                }
                dst.truncate(dst.len() - (cw * sh - w));
                continue;
            }
            // Size the destination once, then write into a slice.
            //
            // The obvious form is `for &c in src { for _ in 0..sh { dst.push(c) } }`,
            // which is a capacity check and a length update per *byte* — about
            // 16.6M of them for one 4K frame. Resizing once and filling chunks
            // hoists all of that out: `chunks_mut` also handles the ragged final
            // chunk when width is not a multiple of `sh`, which removes the
            // trailing `truncate` the push form needed.
            let old = dst.len();
            dst.resize(old + w, 0);
            let out = &mut dst[old..];
            match sh {
                2 => {
                    // Overwhelmingly the common case (4:2:0 / 4:2:2); giving the
                    // compiler a constant chunk width lets it emit a plain
                    // 2-byte store instead of a variable-length fill.
                    let mut it = src.iter();
                    for chunk in out.chunks_exact_mut(2) {
                        let c = *it.next().expect("sized from src");
                        chunk[0] = c;
                        chunk[1] = c;
                    }
                    if let Some(last) = out.chunks_exact_mut(2).into_remainder().first_mut() {
                        *last = *it.next().expect("sized from src");
                    }
                }
                _ => {
                    for (chunk, &c) in out.chunks_mut(sh).zip(src) {
                        chunk.fill(c);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::encode::rgb_to_ycbcr;

    fn assert_rgb_to_ycbcr(rgb: [u8; 3], ycbcr: [u8; 3]) {
        let (y, cb, cr) = rgb_to_ycbcr(rgb[0], rgb[1], rgb[2]);
        assert_eq!([y, cb, cr], ycbcr);
    }

    #[test]
    fn test_rgb_to_ycbcr() {
        assert_rgb_to_ycbcr([0, 0, 0], [0, 128, 128]);
        assert_rgb_to_ycbcr([255, 255, 255], [255, 128, 128]);
        assert_rgb_to_ycbcr([255, 0, 0], [76, 85, 255]);
        assert_rgb_to_ycbcr([0, 255, 0], [150, 44, 21]);
        assert_rgb_to_ycbcr([0, 0, 255], [29, 255, 107]);

        // Values taken from libjpeg for a common image

        assert_rgb_to_ycbcr([59, 109, 6], [82, 85, 111]);
        assert_rgb_to_ycbcr([29, 60, 11], [45, 109, 116]);
        assert_rgb_to_ycbcr([57, 114, 26], [87, 94, 107]);
        assert_rgb_to_ycbcr([30, 60, 6], [45, 106, 117]);
        assert_rgb_to_ycbcr([41, 75, 11], [58, 102, 116]);
        assert_rgb_to_ycbcr([145, 184, 108], [164, 97, 115]);
        assert_rgb_to_ycbcr([33, 85, 7], [61, 98, 108]);
        assert_rgb_to_ycbcr([61, 90, 40], [76, 108, 118]);
        assert_rgb_to_ycbcr([75, 127, 45], [102, 96, 109]);
        assert_rgb_to_ycbcr([30, 56, 14], [43, 111, 118]);
        assert_rgb_to_ycbcr([106, 142, 81], [124, 104, 115]);
        assert_rgb_to_ycbcr([35, 59, 11], [46, 108, 120]);
        assert_rgb_to_ycbcr([170, 203, 123], [184, 94, 118]);
        assert_rgb_to_ycbcr([45, 87, 16], [66, 100, 113]);
        assert_rgb_to_ycbcr([59, 109, 21], [84, 92, 110]);
        assert_rgb_to_ycbcr([100, 167, 36], [132, 74, 105]);
        assert_rgb_to_ycbcr([17, 53, 5], [37, 110, 114]);
        assert_rgb_to_ycbcr([226, 244, 220], [236, 119, 121]);
        assert_rgb_to_ycbcr([192, 214, 120], [197, 85, 125]);
        assert_rgb_to_ycbcr([63, 107, 22], [84, 93, 113]);
        assert_rgb_to_ycbcr([44, 78, 19], [61, 104, 116]);
        assert_rgb_to_ycbcr([72, 106, 54], [90, 108, 115]);
        assert_rgb_to_ycbcr([99, 123, 73], [110, 107, 120]);
        assert_rgb_to_ycbcr([188, 216, 148], [200, 99, 120]);
        assert_rgb_to_ycbcr([19, 46, 7], [33, 113, 118]);
        assert_rgb_to_ycbcr([56, 95, 40], [77, 107, 113]);
        assert_rgb_to_ycbcr([81, 120, 56], [101, 103, 114]);
        assert_rgb_to_ycbcr([9, 30, 0], [20, 117, 120]);
        assert_rgb_to_ycbcr([90, 118, 46], [101, 97, 120]);
        assert_rgb_to_ycbcr([24, 52, 0], [38, 107, 118]);
        assert_rgb_to_ycbcr([32, 69, 9], [51, 104, 114]);
        assert_rgb_to_ycbcr([74, 134, 33], [105, 88, 106]);
        assert_rgb_to_ycbcr([37, 74, 7], [55, 101, 115]);
        assert_rgb_to_ycbcr([69, 119, 31], [94, 92, 110]);
        assert_rgb_to_ycbcr([63, 112, 21], [87, 91, 111]);
        assert_rgb_to_ycbcr([90, 148, 17], [116, 72, 110]);
        assert_rgb_to_ycbcr([50, 97, 30], [75, 102, 110]);
        assert_rgb_to_ycbcr([99, 129, 72], [114, 105, 118]);
        assert_rgb_to_ycbcr([161, 196, 57], [170, 64, 122]);
        assert_rgb_to_ycbcr([10, 26, 1], [18, 118, 122]);
        assert_rgb_to_ycbcr([87, 128, 68], [109, 105, 112]);
        assert_rgb_to_ycbcr([111, 155, 73], [132, 94, 113]);
        assert_rgb_to_ycbcr([33, 75, 11], [55, 103, 112]);
        assert_rgb_to_ycbcr([70, 122, 51], [98, 101, 108]);
        assert_rgb_to_ycbcr([22, 74, 3], [50, 101, 108]);
        assert_rgb_to_ycbcr([88, 142, 45], [115, 89, 109]);
        assert_rgb_to_ycbcr([66, 107, 40], [87, 101, 113]);
        assert_rgb_to_ycbcr([18, 45, 0], [32, 110, 118]);
        assert_rgb_to_ycbcr([163, 186, 88], [168, 83, 124]);
        assert_rgb_to_ycbcr([47, 104, 4], [76, 88, 108]);
        assert_rgb_to_ycbcr([147, 211, 114], [181, 90, 104]);
        assert_rgb_to_ycbcr([42, 77, 18], [60, 104, 115]);
        assert_rgb_to_ycbcr([37, 72, 6], [54, 101, 116]);
        assert_rgb_to_ycbcr([84, 140, 55], [114, 95, 107]);
        assert_rgb_to_ycbcr([46, 98, 25], [74, 100, 108]);
        assert_rgb_to_ycbcr([48, 97, 20], [74, 98, 110]);
        assert_rgb_to_ycbcr([189, 224, 156], [206, 100, 116]);
        assert_rgb_to_ycbcr([36, 83, 0], [59, 94, 111]);
        assert_rgb_to_ycbcr([159, 186, 114], [170, 97, 120]);
        assert_rgb_to_ycbcr([75, 118, 46], [97, 99, 112]);
        assert_rgb_to_ycbcr([193, 233, 158], [212, 97, 114]);
        assert_rgb_to_ycbcr([76, 116, 48], [96, 101, 114]);
        assert_rgb_to_ycbcr([108, 157, 79], [133, 97, 110]);
        assert_rgb_to_ycbcr([180, 208, 155], [194, 106, 118]);
        assert_rgb_to_ycbcr([74, 126, 53], [102, 100, 108]);
        assert_rgb_to_ycbcr([72, 123, 46], [99, 98, 109]);
        assert_rgb_to_ycbcr([71, 123, 34], [97, 92, 109]);
        assert_rgb_to_ycbcr([130, 184, 72], [155, 81, 110]);
        assert_rgb_to_ycbcr([30, 61, 17], [47, 111, 116]);
        assert_rgb_to_ycbcr([27, 71, 0], [50, 100, 112]);
        assert_rgb_to_ycbcr([45, 73, 24], [59, 108, 118]);
        assert_rgb_to_ycbcr([139, 175, 93], [155, 93, 117]);
        assert_rgb_to_ycbcr([11, 38, 0], [26, 114, 118]);
        assert_rgb_to_ycbcr([34, 87, 15], [63, 101, 107]);
        assert_rgb_to_ycbcr([43, 76, 35], [61, 113, 115]);
        assert_rgb_to_ycbcr([18, 35, 7], [27, 117, 122]);
        assert_rgb_to_ycbcr([69, 97, 48], [83, 108, 118]);
        assert_rgb_to_ycbcr([139, 176, 50], [151, 71, 120]);
        assert_rgb_to_ycbcr([21, 51, 7], [37, 111, 117]);
        assert_rgb_to_ycbcr([209, 249, 189], [230, 105, 113]);
        assert_rgb_to_ycbcr([32, 66, 14], [50, 108, 115]);
        assert_rgb_to_ycbcr([100, 143, 67], [121, 97, 113]);
        assert_rgb_to_ycbcr([40, 96, 14], [70, 96, 107]);
        assert_rgb_to_ycbcr([88, 130, 64], [110, 102, 112]);
        assert_rgb_to_ycbcr([52, 112, 14], [83, 89, 106]);
        assert_rgb_to_ycbcr([49, 72, 25], [60, 108, 120]);
        assert_rgb_to_ycbcr([144, 193, 75], [165, 77, 113]);
        assert_rgb_to_ycbcr([49, 94, 1], [70, 89, 113]);
    }
}

#[cfg(test)]
mod planar_tests {
    use super::*;

    /// The whole point of the planar path: replication here must cancel exactly
    /// against the encoder's point-sampling, so chroma reaches the DCT untouched.
    #[test]
    fn chroma_replication_is_exactly_undone_by_point_sampling() {
        // 4x4 luma, 2x2 chroma (4:2:0), distinct values so any mix-up shows.
        let y: Vec<u8> = (0..16).collect();
        let cb: Vec<u8> = vec![10, 20, 30, 40];
        let cr: Vec<u8> = vec![50, 60, 70, 80];
        let img = PlanarYcbcrImage::new(&y, &cb, &cr, [4, 2, 2], 4, 4, (2, 2)).unwrap();

        for row in 0..4u16 {
            let mut buffers: [Vec<u8>; 4] = Default::default();
            img.fill_buffers(row, &mut buffers);
            assert_eq!(buffers[0].len(), 4, "luma row {row}");
            assert_eq!(buffers[1].len(), 4, "cb row {row}");

            // Point-sample every 2nd column, as get_block does, and we must
            // recover the source chroma row verbatim.
            let crow = usize::from(row) / 2;
            let picked: Vec<u8> = buffers[1].iter().step_by(2).copied().collect();
            assert_eq!(picked, cb[crow * 2..crow * 2 + 2].to_vec(), "cb row {row}");
            let picked: Vec<u8> = buffers[2].iter().step_by(2).copied().collect();
            assert_eq!(picked, cr[crow * 2..crow * 2 + 2].to_vec(), "cr row {row}");
        }
    }

    #[test]
    fn odd_width_truncates_replication_to_the_image() {
        // 3-wide 4:2:0 -> 2 chroma samples, replicated to 4, trimmed back to 3.
        let y: Vec<u8> = (0..9).collect();
        let cb: Vec<u8> = vec![1, 2, 3, 4];
        let cr: Vec<u8> = vec![5, 6, 7, 8];
        let img = PlanarYcbcrImage::new(&y, &cb, &cr, [3, 2, 2], 3, 3, (2, 2)).unwrap();
        let mut buffers: [Vec<u8>; 4] = Default::default();
        img.fill_buffers(0, &mut buffers);
        assert_eq!(buffers[0], vec![0, 1, 2]);
        assert_eq!(buffers[1], vec![1, 1, 2]);
        assert_eq!(buffers[2], vec![5, 5, 6]);
    }

    #[test]
    fn sampling_factor_matches_the_planes() {
        let y = vec![0u8; 16];
        let c444 = vec![0u8; 16];
        let c420 = vec![0u8; 4];
        assert_eq!(
            PlanarYcbcrImage::new(&y, &c444, &c444, [4, 4, 4], 4, 4, (1, 1))
                .unwrap()
                .sampling_factor(),
            crate::encode::SamplingFactor::R_4_4_4
        );
        assert_eq!(
            PlanarYcbcrImage::new(&y, &c420, &c420, [4, 2, 2], 4, 4, (2, 2))
                .unwrap()
                .sampling_factor(),
            crate::encode::SamplingFactor::R_4_2_0
        );
    }

    #[test]
    fn rejects_undersized_planes_and_bad_ratios() {
        let y = vec![0u8; 16];
        let small = vec![0u8; 2];
        assert!(PlanarYcbcrImage::new(&y, &small, &small, [4, 2, 2], 4, 4, (2, 2)).is_none());
        let ok = vec![0u8; 16];
        assert!(PlanarYcbcrImage::new(&y, &ok, &ok, [4, 4, 4], 4, 4, (4, 4)).is_none());
    }
}
