// Copyright 2006 The Android Open Source Project
// Copyright 2020 Yevhenii Reizner
//
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(feature = "png-format")]
use alloc::vec;
use alloc::vec::Vec;

use core::convert::TryFrom;
use core::num::NonZeroUsize;

use crate::path::IntSize;

use crate::{Color, IntRect};

use crate::color::PremultipliedColorU8;
use crate::geom::{IntSizeExt, ScreenIntRect};

#[cfg(feature = "png-format")]
use crate::color::{premultiply_u8, ALPHA_U8_OPAQUE};

/// Number of bytes per pixel.
pub const BYTES_PER_PIXEL: usize = 4;

/// A container that owns premultiplied RGBA pixels.
///
/// The data is not aligned, therefore width == stride.
#[derive(Clone, PartialEq)]
pub struct Pixmap {
    data: Vec<u8>,
    size: IntSize,
}

impl Pixmap {
    /// Allocates a new pixmap.
    ///
    /// A pixmap is filled with transparent black by default, aka (0, 0, 0, 0).
    ///
    /// Zero size in an error.
    ///
    /// Pixmap's width is limited by i32::MAX/4.
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let size = IntSize::from_wh(width, height)?;
        let data_len = data_len_for_size(size)?;

        // Fallible allocation: an oversized request returns `None` instead of
        // aborting the process (tiny-skia uses `vec![0; len]`).
        Some(Pixmap {
            data: try_zeroed(data_len)?,
            size,
        })
    }

    /// Creates a new pixmap by taking ownership over an image buffer
    /// (premultiplied RGBA pixels).
    ///
    /// The size needs to match the data provided.
    ///
    /// Pixmap's width is limited by i32::MAX/4.
    pub fn from_vec(data: Vec<u8>, size: IntSize) -> Option<Self> {
        let data_len = data_len_for_size(size)?;
        if data.len() != data_len {
            return None;
        }

        Some(Pixmap { data, size })
    }

    /// Decodes a PNG data into a `Pixmap`.
    ///
    /// Only 8-bit images are supported.
    /// Index PNGs are not supported.
    #[cfg(feature = "png-format")]
    pub fn decode_png(data: &[u8]) -> Result<Self, rusty_png::DecodingError> {
        fn make_custom_png_error(msg: &str) -> rusty_png::DecodingError {
            std::io::Error::other(msg).into()
        }

        let mut decoder = rusty_png::Decoder::new(std::io::Cursor::new(data));
        decoder.set_transformations(rusty_png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info()?;
        let output_buffer_size = reader.output_buffer_size();
        let mut img_data = vec![0; output_buffer_size];
        let info = reader.next_frame(&mut img_data)?;

        if info.bit_depth != rusty_png::BitDepth::Eight {
            return Err(make_custom_png_error("unsupported bit depth"));
        }

        let size = IntSize::from_wh(info.width, info.height)
            .ok_or_else(|| make_custom_png_error("invalid image size"))?;
        let data_len =
            data_len_for_size(size).ok_or_else(|| make_custom_png_error("image is too big"))?;

        img_data = match info.color_type {
            rusty_png::ColorType::Rgb => {
                let mut rgba_data = Vec::with_capacity(data_len);
                for rgb in img_data.chunks(3) {
                    rgba_data.push(rgb[0]);
                    rgba_data.push(rgb[1]);
                    rgba_data.push(rgb[2]);
                    rgba_data.push(ALPHA_U8_OPAQUE);
                }

                rgba_data
            }
            rusty_png::ColorType::Rgba => img_data,
            rusty_png::ColorType::Grayscale => {
                let mut rgba_data = Vec::with_capacity(data_len);
                for gray in img_data {
                    rgba_data.push(gray);
                    rgba_data.push(gray);
                    rgba_data.push(gray);
                    rgba_data.push(ALPHA_U8_OPAQUE);
                }

                rgba_data
            }
            rusty_png::ColorType::GrayscaleAlpha => {
                let mut rgba_data = Vec::with_capacity(data_len);
                for slice in img_data.chunks(2) {
                    let gray = slice[0];
                    let alpha = slice[1];
                    rgba_data.push(gray);
                    rgba_data.push(gray);
                    rgba_data.push(gray);
                    rgba_data.push(alpha);
                }

                rgba_data
            }
            rusty_png::ColorType::Indexed => {
                return Err(make_custom_png_error("indexed PNG is not supported"));
            }
        };

        // Premultiply alpha.
        //
        // We cannon use RasterPipeline here, which is faster,
        // because it produces slightly different results.
        // Seems like Skia does the same.
        //
        // Also, in our tests unsafe version (no bound checking)
        // had roughly the same performance. So we keep the safe one.
        for pixel in img_data.as_mut_slice().chunks_mut(BYTES_PER_PIXEL) {
            let a = pixel[3];
            pixel[0] = premultiply_u8(pixel[0], a);
            pixel[1] = premultiply_u8(pixel[1], a);
            pixel[2] = premultiply_u8(pixel[2], a);
        }

        Pixmap::from_vec(img_data, size)
            .ok_or_else(|| make_custom_png_error("failed to create a pixmap"))
    }

    /// Loads a PNG file into a `Pixmap`.
    ///
    /// Only 8-bit images are supported.
    /// Index PNGs are not supported.
    #[cfg(feature = "png-format")]
    pub fn load_png<P: AsRef<std::path::Path>>(path: P) -> Result<Self, rusty_png::DecodingError> {
        // `rusty_png::Decoder` is generic over input, which means that it will instance
        // two copies: one for `&[]` and one for `File`. Which will simply bloat the code.
        // Therefore we're using only one type for input.
        let data = std::fs::read(path)?;
        Self::decode_png(&data)
    }

    /// Encodes pixmap into a PNG data.
    #[cfg(feature = "png-format")]
    pub fn encode_png(&self) -> Result<Vec<u8>, rusty_png::EncodingError> {
        self.as_ref().encode_png()
    }

    /// Saves pixmap as a PNG file.
    #[cfg(feature = "png-format")]
    pub fn save_png<P: AsRef<std::path::Path>>(
        &self,
        path: P,
    ) -> Result<(), rusty_png::EncodingError> {
        self.as_ref().save_png(path)
    }

    /// Returns a container that references Pixmap's data.
    pub fn as_ref(&self) -> PixmapRef<'_> {
        PixmapRef {
            data: &self.data,
            size: self.size,
        }
    }

    /// Returns a container that references Pixmap's data.
    pub fn as_mut(&mut self) -> PixmapMut<'_> {
        PixmapMut {
            data: &mut self.data,
            size: self.size,
        }
    }

    /// Returns pixmap's width.
    #[inline]
    pub fn width(&self) -> u32 {
        self.size.width()
    }

    /// Returns pixmap's height.
    #[inline]
    pub fn height(&self) -> u32 {
        self.size.height()
    }

    /// Returns pixmap's size.
    #[allow(dead_code)]
    pub(crate) fn size(&self) -> IntSize {
        self.size
    }

    /// Fills the entire pixmap with a specified color.
    pub fn fill(&mut self, color: Color) {
        fill_bytes(&mut self.data, color);
    }

    /// Returns the internal data.
    ///
    /// Byteorder: RGBA
    pub fn data(&self) -> &[u8] {
        self.data.as_slice()
    }

    /// Returns the mutable internal data.
    ///
    /// Byteorder: RGBA
    pub fn data_mut(&mut self) -> &mut [u8] {
        self.data.as_mut_slice()
    }

    /// Returns a pixel color.
    ///
    /// Returns `None` when position is out of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> Option<PremultipliedColorU8> {
        self.as_ref().pixel(x, y)
    }

    /// Sets a pixel color. Returns `false` when position is out of bounds.
    ///
    /// Not in tiny-skia, which exposes `pixels_mut()` as a typed slice instead.
    pub fn set_pixel(&mut self, x: u32, y: u32, color: PremultipliedColorU8) -> bool {
        set_pixel_bytes(&mut self.data, self.size, x, y, color)
    }

    /// Returns an iterator over mutable pixels, each as `[r, g, b, a]` bytes.
    ///
    /// tiny-skia returns `&mut [PremultipliedColorU8]`; that reinterpretation
    /// needs `unsafe` (or `bytemuck`), which this crate does not use.
    pub fn pixels_mut(&mut self) -> impl ExactSizeIterator<Item = &mut [u8; 4]> + '_ {
        pixels_mut_bytes(&mut self.data)
    }

    /// Returns an iterator over the pixels.
    ///
    /// tiny-skia returns `&[PremultipliedColorU8]`; this yields the same values.
    pub fn pixels(&self) -> impl ExactSizeIterator<Item = PremultipliedColorU8> + '_ {
        pixels_bytes(&self.data)
    }

    /// Consumes the internal data.
    ///
    /// Byteorder: RGBA
    pub fn take(self) -> Vec<u8> {
        self.data
    }

    /// Consumes the pixmap and returns the internal data as demultiplied RGBA bytes.
    ///
    /// Byteorder: RGBA
    pub fn take_demultiplied(mut self) -> Vec<u8> {
        // Demultiply alpha.
        //
        // RasterPipeline is 15% faster here, but produces slightly different results
        // due to rounding. So we stick with this method for now.
        for pixel in self.data.chunks_exact_mut(BYTES_PER_PIXEL) {
            let c =
                PremultipliedColorU8::from_rgba_unchecked(pixel[0], pixel[1], pixel[2], pixel[3])
                    .demultiply();
            pixel.copy_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        self.data
    }

    /// Returns a copy of the pixmap that intersects the `rect`.
    ///
    /// Returns `None` when `Pixmap`'s rect doesn't contain `rect`.
    pub fn clone_rect(&self, rect: IntRect) -> Option<Pixmap> {
        self.as_ref().clone_rect(rect)
    }
}

impl core::fmt::Debug for Pixmap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Pixmap")
            .field("data", &"...")
            .field("width", &self.size.width())
            .field("height", &self.size.height())
            .finish()
    }
}

/// A container that references premultiplied RGBA pixels.
///
/// Can be created from `Pixmap` or from a user provided data.
///
/// The data is not aligned, therefore width == stride.
#[derive(Clone, Copy, PartialEq)]
pub struct PixmapRef<'a> {
    data: &'a [u8],
    size: IntSize,
}

impl<'a> PixmapRef<'a> {
    /// Creates a new `PixmapRef` from bytes.
    ///
    /// The size must be at least `size.width() * size.height() * BYTES_PER_PIXEL`.
    /// Zero size in an error. Width is limited by i32::MAX/4.
    ///
    /// The `data` is assumed to have premultiplied RGBA pixels (byteorder: RGBA).
    pub fn from_bytes(data: &'a [u8], width: u32, height: u32) -> Option<Self> {
        let size = IntSize::from_wh(width, height)?;
        let data_len = data_len_for_size(size)?;
        if data.len() < data_len {
            return None;
        }

        Some(PixmapRef { data, size })
    }

    /// Creates a new `Pixmap` from the current data.
    ///
    /// Clones the underlying data.
    pub fn to_owned(&self) -> Pixmap {
        Pixmap {
            data: self.data.to_vec(),
            size: self.size,
        }
    }

    /// Returns pixmap's width.
    #[inline]
    pub fn width(&self) -> u32 {
        self.size.width()
    }

    /// Returns pixmap's height.
    #[inline]
    pub fn height(&self) -> u32 {
        self.size.height()
    }

    /// Returns pixmap's size.
    pub(crate) fn size(&self) -> IntSize {
        self.size
    }

    /// Returns pixmap's rect.
    pub(crate) fn rect(&self) -> ScreenIntRect {
        self.size.to_screen_int_rect(0, 0)
    }

    /// Returns the internal data.
    ///
    /// Byteorder: RGBA
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Returns a pixel color.
    ///
    /// Returns `None` when position is out of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> Option<PremultipliedColorU8> {
        if x >= self.width() || y >= self.height() {
            return None;
        }
        let i = (y as usize * self.width() as usize + x as usize) * BYTES_PER_PIXEL;
        let p = self.data.get(i..i + BYTES_PER_PIXEL)?;
        Some(PremultipliedColorU8::from_rgba_unchecked(
            p[0], p[1], p[2], p[3],
        ))
    }

    /// Returns a slice of pixels.
    pub fn pixels(&self) -> impl ExactSizeIterator<Item = PremultipliedColorU8> + 'a {
        pixels_bytes(self.data)
    }

    // TODO: add rows() iterator

    /// Returns a copy of the pixmap that intersects the `rect`.
    ///
    /// Returns `None` when `Pixmap`'s rect doesn't contain `rect`.
    pub fn clone_rect(&self, rect: IntRect) -> Option<Pixmap> {
        // TODO: to ScreenIntRect?

        let rect = self.rect().to_int_rect().intersect(&rect)?;
        let mut new = Pixmap::new(rect.width(), rect.height())?;
        {
            let row = rect.width() as usize * BYTES_PER_PIXEL;
            let new_data = new.data_mut();
            for y in 0..rect.height() as usize {
                let src = ((y + rect.y() as usize) * self.width() as usize + rect.x() as usize)
                    * BYTES_PER_PIXEL;
                new_data[y * row..(y + 1) * row].copy_from_slice(&self.data[src..src + row]);
            }
        }

        Some(new)
    }

    /// Encodes pixmap into a PNG data.
    #[cfg(feature = "png-format")]
    pub fn encode_png(&self) -> Result<Vec<u8>, rusty_png::EncodingError> {
        // Skia uses skcms here, which is somewhat similar to RasterPipeline.

        // Sadly, we have to copy the pixmap here, because of demultiplication.
        // Not sure how to avoid this.
        // TODO: remove allocation
        let demultiplied_data = self.to_owned().take_demultiplied();

        let mut data = Vec::new();
        {
            let mut encoder = rusty_png::Encoder::new(&mut data, self.width(), self.height());
            encoder.set_color(rusty_png::ColorType::Rgba);
            encoder.set_depth(rusty_png::BitDepth::Eight);
            let mut writer = encoder.write_header()?;
            writer.write_image_data(&demultiplied_data)?;
        }

        Ok(data)
    }

    /// Saves pixmap as a PNG file.
    #[cfg(feature = "png-format")]
    pub fn save_png<P: AsRef<std::path::Path>>(
        &self,
        path: P,
    ) -> Result<(), rusty_png::EncodingError> {
        let data = self.encode_png()?;
        std::fs::write(path, data)?;
        Ok(())
    }
}

impl core::fmt::Debug for PixmapRef<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PixmapRef")
            .field("data", &"...")
            .field("width", &self.size.width())
            .field("height", &self.size.height())
            .finish()
    }
}

/// A container that references mutable premultiplied RGBA pixels.
///
/// Can be created from `Pixmap` or from a user provided data.
///
/// The data is not aligned, therefore width == stride.
#[derive(PartialEq)]
pub struct PixmapMut<'a> {
    data: &'a mut [u8],
    size: IntSize,
}

impl<'a> PixmapMut<'a> {
    /// Creates a new `PixmapMut` from bytes.
    ///
    /// The size must be at least `size.width() * size.height() * BYTES_PER_PIXEL`.
    /// Zero size in an error. Width is limited by i32::MAX/4.
    ///
    /// The `data` is assumed to have premultiplied RGBA pixels (byteorder: RGBA).
    pub fn from_bytes(data: &'a mut [u8], width: u32, height: u32) -> Option<Self> {
        let size = IntSize::from_wh(width, height)?;
        let data_len = data_len_for_size(size)?;
        if data.len() < data_len {
            return None;
        }

        Some(PixmapMut { data, size })
    }

    /// Creates a new `Pixmap` from the current data.
    ///
    /// Clones the underlying data.
    pub fn to_owned(&self) -> Pixmap {
        Pixmap {
            data: self.data.to_vec(),
            size: self.size,
        }
    }

    /// Returns a container that references Pixmap's data.
    pub fn as_ref(&self) -> PixmapRef<'_> {
        PixmapRef {
            data: self.data,
            size: self.size,
        }
    }

    /// Returns pixmap's width.
    #[inline]
    pub fn width(&self) -> u32 {
        self.size.width()
    }

    /// Returns pixmap's height.
    #[inline]
    pub fn height(&self) -> u32 {
        self.size.height()
    }

    /// Returns pixmap's size.
    pub(crate) fn size(&self) -> IntSize {
        self.size
    }

    /// Fills the entire pixmap with a specified color.
    pub fn fill(&mut self, color: Color) {
        fill_bytes(self.data, color);
    }

    /// Returns the mutable internal data.
    ///
    /// Byteorder: RGBA
    pub fn data_mut(&mut self) -> &mut [u8] {
        self.data
    }

    /// Returns an iterator over mutable pixels, each as `[r, g, b, a]` bytes.
    ///
    /// See [`Pixmap::pixels_mut`] for why this is not a typed slice.
    pub fn pixels_mut(&mut self) -> impl ExactSizeIterator<Item = &mut [u8; 4]> + '_ {
        pixels_mut_bytes(self.data)
    }

    /// Sets a pixel color. Returns `false` when position is out of bounds.
    pub fn set_pixel(&mut self, x: u32, y: u32, color: PremultipliedColorU8) -> bool {
        set_pixel_bytes(self.data, self.size, x, y, color)
    }

    /// Creates `SubPixmapMut` that contains the whole `PixmapMut`.
    pub(crate) fn as_subpixmap(&mut self) -> SubPixmapMut<'_> {
        SubPixmapMut {
            size: self.size(),
            real_width: self.width() as usize,
            data: self.data,
        }
    }

    /// Returns a mutable reference to the pixmap region that intersects the `rect`.
    ///
    /// Returns `None` when `Pixmap`'s rect doesn't contain `rect`.
    pub(crate) fn subpixmap(&mut self, rect: IntRect) -> Option<SubPixmapMut<'_>> {
        let rect = self.size.to_int_rect(0, 0).intersect(&rect)?;
        let row_bytes = self.width() as usize * BYTES_PER_PIXEL;
        let offset = rect.top() as usize * row_bytes + rect.left() as usize * BYTES_PER_PIXEL;

        Some(SubPixmapMut {
            size: rect.size(),
            real_width: self.width() as usize,
            data: &mut self.data[offset..],
        })
    }
}

impl core::fmt::Debug for PixmapMut<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PixmapMut")
            .field("data", &"...")
            .field("width", &self.size.width())
            .field("height", &self.size.height())
            .finish()
    }
}

/// A `PixmapMut` subregion.
///
/// Unlike `PixmapMut`, contains `real_width` which references the parent `PixmapMut` width.
/// This way we can operate on a `PixmapMut` subregion without reallocations.
/// Primarily required because of `DrawTiler`.
///
/// We cannot implement it in `PixmapMut` directly, because it will brake `fill`, `data_mut`
/// `pixels_mut` and other similar methods.
/// This is because `SubPixmapMut.data` references more "data" than it actually allowed to access.
/// On the other hand, `PixmapMut.data` can access all it's data and it's stored linearly.
pub struct SubPixmapMut<'a> {
    pub data: &'a mut [u8],
    pub size: IntSize,
    pub real_width: usize,
}

/// Below this size a refused allocation is ordinary out-of-memory, not a
/// hostile-geometry request, so the fallibility probe is skipped.
const PROBE_MIN_LEN: usize = 1 << 16;

/// A zeroed buffer, or `None` when the allocator refuses the request.
///
/// The buffer comes from `vec![0; len]`, which lowers to the allocator's
/// zeroed allocation: on large requests that is fresh OS pages with no writes,
/// where `resize(len, 0)` wrote every byte (issue #198, ~100x slower `new`
/// under the system allocator). Fallibility is kept by first asking for the
/// same size with `try_reserve_exact` and releasing it; an oversized request
/// still returns `None` instead of aborting.
pub(crate) fn try_zeroed(len: usize) -> Option<Vec<u8>> {
    if len >= PROBE_MIN_LEN {
        let mut probe: Vec<u8> = Vec::new();
        probe.try_reserve_exact(len).ok()?;
    }
    Some(vec![0; len])
}

fn fill_bytes(data: &mut [u8], color: Color) {
    let c = color.premultiply().to_color_u8();
    let c = [c.red(), c.green(), c.blue(), c.alpha()];
    for p in data.chunks_exact_mut(BYTES_PER_PIXEL) {
        p.copy_from_slice(&c);
    }
}

fn pixels_bytes(data: &[u8]) -> impl ExactSizeIterator<Item = PremultipliedColorU8> + '_ {
    data.chunks_exact(BYTES_PER_PIXEL)
        .map(|p| PremultipliedColorU8::from_rgba_unchecked(p[0], p[1], p[2], p[3]))
}

fn pixels_mut_bytes(data: &mut [u8]) -> impl ExactSizeIterator<Item = &mut [u8; 4]> + '_ {
    data.chunks_exact_mut(BYTES_PER_PIXEL)
        .map(|p| match p.try_into() {
            Ok(a) => a,
            Err(_) => unreachable!(),
        })
}

fn set_pixel_bytes(
    data: &mut [u8],
    size: IntSize,
    x: u32,
    y: u32,
    color: PremultipliedColorU8,
) -> bool {
    if x >= size.width() || y >= size.height() {
        return false;
    }
    let i = (y as usize * size.width() as usize + x as usize) * BYTES_PER_PIXEL;
    match data.get_mut(i..i + BYTES_PER_PIXEL) {
        Some(p) => {
            p.copy_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
            true
        }
        None => false,
    }
}

/// Returns minimum bytes per row as usize.
///
/// Pixmap's maximum value for row bytes must fit in 31 bits.
fn min_row_bytes(size: IntSize) -> Option<NonZeroUsize> {
    let w = i32::try_from(size.width()).ok()?;
    let w = w.checked_mul(BYTES_PER_PIXEL as i32)?;
    NonZeroUsize::new(w as usize)
}

/// Returns storage size required by pixel array.
fn compute_data_len(size: IntSize, row_bytes: usize) -> Option<usize> {
    let h = size.height().checked_sub(1)?;
    let h = (h as usize).checked_mul(row_bytes)?;

    let w = (size.width() as usize).checked_mul(BYTES_PER_PIXEL)?;

    h.checked_add(w)
}

fn data_len_for_size(size: IntSize) -> Option<usize> {
    let row_bytes = min_row_bytes(size)?;
    compute_data_len(size, row_bytes.get())
}

#[cfg(test)]
mod tests {
    use super::try_zeroed;

    /// `try_zeroed` hands back exactly `len` zero bytes that the `Vec` owns
    /// (length and capacity `len`), an empty buffer for 0, and `None` for
    /// requests no allocator can meet.
    #[test]
    fn try_zeroed_is_an_owned_zeroed_buffer() {
        for len in [1usize, 3, 4096, 3 * 1024 * 1024 + 7] {
            let mut v = try_zeroed(len).unwrap();
            assert_eq!((v.len(), v.capacity()), (len, len));
            assert!(v.iter().all(|&b| b == 0), "len {len}");
            v.push(9);
            assert_eq!(v[len], 9);
        }
        assert!(try_zeroed(0).unwrap().is_empty());
        assert!(try_zeroed(isize::MAX as usize).is_none());
        assert!(try_zeroed(usize::MAX).is_none());
    }
}
