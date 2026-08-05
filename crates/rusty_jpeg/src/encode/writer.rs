use crate::encode::encoder::Component;
use crate::encode::huffman::{CodingClass, HuffmanTable};
use crate::encode::marker::{Marker, SOFType};
use crate::encode::quantization::QuantizationTable;
use crate::encode::EncodingError;

/// Represents the pixel density of an image
///
/// For example, a 300 DPI image is represented by:
///
/// ```rust
/// # use rusty_jpeg::encode::{PixelDensity, PixelDensityUnit};
/// let hdpi = PixelDensity::dpi(300);
/// assert_eq!(hdpi, PixelDensity {density: (300,300), unit: PixelDensityUnit::Inches})
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelDensity {
    /// A couple of values for (Xdensity, Ydensity)
    pub density: (u16, u16),
    /// The unit in which the density is measured
    pub unit: PixelDensityUnit,
}

impl PixelDensity {
    /// Creates the most common pixel density type:
    /// the horizontal and the vertical density are equal,
    /// and measured in pixels per inch.
    #[must_use]
    pub fn dpi(density: u16) -> Self {
        PixelDensity {
            density: (density, density),
            unit: PixelDensityUnit::Inches,
        }
    }
}

impl Default for PixelDensity {
    /// Returns a pixel density with a pixel aspect ratio of 1
    fn default() -> Self {
        PixelDensity {
            density: (1, 1),
            unit: PixelDensityUnit::PixelAspectRatio,
        }
    }
}

/// Represents a unit in which the density of an image is measured
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelDensityUnit {
    /// Represents the absence of a unit, the values indicate only a
    /// [pixel aspect ratio](https://en.wikipedia.org/wiki/Pixel_aspect_ratio)
    PixelAspectRatio,

    /// Pixels per inch (2.54 cm)
    Inches,

    /// Pixels per centimeter
    Centimeters,
}

/// Zig-zag sequence of quantized DCT coefficients
///
/// Figure A.6
pub static ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

const BUFFER_SIZE: usize = core::mem::size_of::<usize>() * 8;

/// A no_std alternative for `std::io::Write`
///
/// An implementation of a subset of `std::io::Write` necessary to use the encoder without `std`.
/// This trait is implemented for `std::io::Write` if the `std` feature is enabled.
pub trait JfifWrite {
    /// Writes the whole buffer. The behavior must be identical to std::io::Write::write_all
    /// # Errors
    ///
    /// Return an error if the data can't be written
    fn write_all(&mut self, buf: &[u8]) -> Result<(), EncodingError>;
}

#[cfg(not(feature = "std"))]
impl<W: JfifWrite + ?Sized> JfifWrite for &mut W {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), EncodingError> {
        (**self).write_all(buf)
    }
}

#[cfg(not(feature = "std"))]
impl JfifWrite for alloc::vec::Vec<u8> {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), EncodingError> {
        self.extend_from_slice(buf);
        Ok(())
    }
}

#[cfg(feature = "std")]
impl<W: std::io::Write + ?Sized> JfifWrite for W {
    #[inline(always)]
    fn write_all(&mut self, buf: &[u8]) -> Result<(), EncodingError> {
        self.write_all(buf)?;
        Ok(())
    }
}

pub(crate) struct JfifWriter<W: JfifWrite> {
    w: W,
    bit_buffer: usize,
    free_bits: i8,
}

impl<W: JfifWrite> JfifWriter<W> {
    pub fn new(w: W) -> Self {
        JfifWriter {
            w,
            bit_buffer: 0,
            free_bits: BUFFER_SIZE as i8,
        }
    }

    /// No-op: output goes straight to the sink.
    ///
    /// A 64 KiB staging buffer was tried here, on the theory that flushing the
    /// bit buffer 8 bytes at a time meant ~625k tiny `write_all` calls per 5 MB
    /// image. It measured neutral-to-negative and was reverted: the sinks that
    /// matter are already buffered (`Vec<u8>`, whose `write_all` is just
    /// `extend_from_slice`, and `BufWriter` for files), so staging only added a
    /// second copy. Kept as a no-op so the call site documents the finding.
    pub fn flush_output(&mut self) -> Result<(), EncodingError> {
        Ok(())
    }

    #[inline(always)]
    pub fn write(&mut self, buf: &[u8]) -> Result<(), EncodingError> {
        self.w.write_all(buf)
    }

    #[inline(always)]
    pub fn write_u8(&mut self, value: u8) -> Result<(), EncodingError> {
        self.w.write_all(&[value])
    }

    #[inline(always)]
    pub fn write_u16(&mut self, value: u16) -> Result<(), EncodingError> {
        self.w.write_all(&value.to_be_bytes())
    }

    pub fn finalize_bit_buffer(&mut self) -> Result<(), EncodingError> {
        self.write_bits(0x7F, 7)?;
        self.flush_bit_buffer()?;
        self.bit_buffer = 0;
        self.free_bits = BUFFER_SIZE as i8;

        Ok(())
    }

    pub fn flush_bit_buffer(&mut self) -> Result<(), EncodingError> {
        while self.free_bits <= (BUFFER_SIZE as i8 - 8) {
            self.flush_byte_from_bit_buffer(self.free_bits)?;
            self.free_bits += 8;
        }

        Ok(())
    }

    #[inline(always)]
    fn flush_byte_from_bit_buffer(&mut self, free_bits: i8) -> Result<(), EncodingError> {
        let value = (self.bit_buffer >> (BUFFER_SIZE as i8 - 8 - free_bits)) & 0xFF;

        self.write_u8(value as u8)?;

        if value == 0xFF {
            self.write_u8(0x00)?;
        }

        Ok(())
    }

    #[inline(always)]
    #[allow(overflowing_literals)]
    fn write_bit_buffer(&mut self) -> Result<(), EncodingError> {
        crate::prof::bump(crate::prof::Count::BufferFlushes, 1);
        if (self.bit_buffer
            & 0x8080808080808080
            & !(self.bit_buffer.wrapping_add(0x0101010101010101)))
            != 0
        {
            crate::prof::bump(crate::prof::Count::StuffedFlushes, 1);
            for i in 0..(BUFFER_SIZE / 8) {
                self.flush_byte_from_bit_buffer((i * 8) as i8)?;
            }
            Ok(())
        } else {
            self.w.write_all(&self.bit_buffer.to_be_bytes())
        }
    }

    /// Inlined deliberately: this runs once per Huffman symbol — ~663k times
    /// per 1080p frame — and is a handful of shifts around one predictable
    /// branch. Left out-of-line it was a call plus a `Result` check per symbol,
    /// against a body barely larger than the call sequence itself.
    #[inline]
    pub fn write_bits(&mut self, value: u32, size: u8) -> Result<(), EncodingError> {
        crate::prof::bump(crate::prof::Count::BitWrites, 1);
        crate::prof::bump(crate::prof::Count::Bits, size as u64);
        let size = size as i8;
        let value = value as usize;

        let free_bits = self.free_bits - size;

        if free_bits < 0 {
            self.bit_buffer = (self.bit_buffer << (size + free_bits)) | (value >> -free_bits);
            self.write_bit_buffer()?;
            self.bit_buffer = value;
            self.free_bits = free_bits + BUFFER_SIZE as i8;
        } else {
            self.free_bits = free_bits;
            self.bit_buffer = (self.bit_buffer << size) | value;
        }
        Ok(())
    }

    pub fn write_marker(&mut self, marker: Marker) -> Result<(), EncodingError> {
        self.write(&[0xFF, marker.into()])
    }

    pub fn write_segment(&mut self, marker: Marker, data: &[u8]) -> Result<(), EncodingError> {
        self.write_marker(marker)?;
        self.write_u16(data.len() as u16 + 2)?;
        self.write(data)?;

        Ok(())
    }

    pub fn write_header(&mut self, density: &PixelDensity) -> Result<(), EncodingError> {
        self.write_marker(Marker::APP(0))?;
        self.write_u16(16)?;

        self.write(b"JFIF\0")?;
        self.write(&[0x01, 0x02])?;

        match density.unit {
            PixelDensityUnit::PixelAspectRatio => {
                self.write_u8(0x00)?;
            }
            PixelDensityUnit::Inches => {
                self.write_u8(0x01)?;
            }
            PixelDensityUnit::Centimeters => {
                self.write_u8(0x02)?;
            }
        }
        let (x, y) = density.density;
        self.write_u16(x)?;
        self.write_u16(y)?;

        self.write(&[0x00, 0x00])
    }

    /// Append huffman table segment
    ///
    /// - `class`: 0 for DC or 1 for AC
    /// - `dest`: 0 for luma or 1 for chroma tables
    ///
    /// Layout:
    /// ```txt
    /// |--------|---------------|--------------------------|--------------------|--------|
    /// | 0xFFC4 | 16 bit length | 4 bit class / 4 bit dest |  16 byte num codes | values |
    /// |--------|---------------|--------------------------|--------------------|--------|
    /// ```
    ///
    pub fn write_huffman_segment(
        &mut self,
        class: CodingClass,
        destination: u8,
        table: &HuffmanTable,
    ) -> Result<(), EncodingError> {
        assert!(destination < 4, "Bad destination: {}", destination);

        self.write_marker(Marker::DHT)?;
        self.write_u16(2 + 1 + 16 + table.values().len() as u16)?;

        self.write_u8(((class as u8) << 4) | destination)?;
        self.write(table.length())?;
        self.write(table.values())?;

        Ok(())
    }

    /// Append a quantization table
    ///
    /// - `precision`: 0 which means 1 byte per value.
    /// - `dest`: 0 for luma or 1 for chroma tables
    ///
    /// Layout:
    /// ```txt
    /// |--------|---------------|------------------------------|--------|--------|-----|--------|
    /// | 0xFFDB | 16 bit length | 4 bit precision / 4 bit dest | V(0,0) | V(0,1) | ... | V(7,7) |
    /// |--------|---------------|------------------------------|--------|--------|-----|--------|
    /// ```
    ///
    pub fn write_quantization_segment(
        &mut self,
        destination: u8,
        table: &QuantizationTable,
    ) -> Result<(), EncodingError> {
        assert!(destination < 4, "Bad destination: {}", destination);

        self.write_marker(Marker::DQT)?;
        self.write_u16(2 + 1 + 64)?;

        self.write_u8(destination)?;

        for &v in ZIGZAG.iter() {
            self.write_u8(table.get(v as usize))?;
        }

        Ok(())
    }

    pub fn write_dri(&mut self, restart_interval: u16) -> Result<(), EncodingError> {
        self.write_marker(Marker::DRI)?;
        self.write_u16(4)?;
        self.write_u16(restart_interval)
    }

    #[inline]
    pub fn huffman_encode(&mut self, val: u8, table: &HuffmanTable) -> Result<(), EncodingError> {
        crate::prof::bump(crate::prof::Count::Symbols, 1);
        let &(size, code) = table.get_for_value(val);
        self.write_bits(code as u32, size)
    }

    #[inline]
    pub fn huffman_encode_value(
        &mut self,
        size: u8,
        symbol: u8,
        value: u16,
        table: &HuffmanTable,
    ) -> Result<(), EncodingError> {
        crate::prof::bump(crate::prof::Count::Symbols, 1);
        let &(num_bits, code) = table.get_for_value(symbol);

        let mut temp = value as u32;
        temp |= (code as u32) << size;
        let size = size + num_bits;

        self.write_bits(temp, size)
    }

    pub fn write_block(
        &mut self,
        block: &[i16; 64],
        prev_dc: i16,
        dc_table: &HuffmanTable,
        ac_table: &HuffmanTable,
    ) -> Result<(), EncodingError> {
        self.write_dc(block[0], prev_dc, dc_table)?;
        self.write_ac_block(block, 1, 64, ac_table)
    }

    /// Count the symbols [`write_block`](Self::write_block) *would* emit,
    /// without emitting them and without needing a table.
    ///
    /// This is what lets optimized Huffman tables be built in a streaming pass
    /// instead of materializing every block first (218 MB at 4K). It must stay a
    /// faithful mirror of `write_dc` + `write_ac_block` — same run-length rules,
    /// same ZRL and EOB handling — so it lives directly beside them; change one,
    /// change the other. `streaming_matches_materialized_histogram` in the
    /// encoder's tests is the gate that they still agree.
    pub fn count_block(block: &[i16; 64], prev_dc: i16, dc_freq: &mut [u32], ac_freq: &mut [u32]) {
        // Mirrors `write_dc`: the DC symbol is the bit-length of the difference.
        let (size, _) = get_code(block[0] - prev_dc);
        dc_freq[size as usize] += 1;

        // Mirrors `write_ac_block` over the same 1..64 range.
        let mut zero_run: u8 = 0;
        for &value in &block[1..64] {
            if value == 0 {
                zero_run += 1;
            } else {
                while zero_run > 15 {
                    ac_freq[0xF0] += 1;
                    zero_run -= 16;
                }
                let (size, _) = get_code(value);
                ac_freq[usize::from((zero_run << 4) | size)] += 1;
                zero_run = 0;
            }
        }
        if zero_run > 0 {
            ac_freq[0x00] += 1;
        }
    }

    #[inline]
    pub fn write_dc(
        &mut self,
        value: i16,
        prev_dc: i16,
        dc_table: &HuffmanTable,
    ) -> Result<(), EncodingError> {
        let diff = value - prev_dc;
        let (size, value) = get_code(diff);

        self.huffman_encode_value(size, size, value, dc_table)?;

        Ok(())
    }

    pub fn write_ac_block(
        &mut self,
        block: &[i16; 64],
        start: usize,
        end: usize,
        ac_table: &HuffmanTable,
    ) -> Result<(), EncodingError> {
        let mut mask = nonzero_mask(block);
        // Restrict to [start, end). Progressive scans use sub-ranges.
        mask &= u64::MAX << start;
        if end < 64 {
            mask &= !(u64::MAX << end);
        }

        // `prev` is the position just past the last coded coefficient, so
        // `i - prev` is exactly the zero run the branchy form accumulated.
        let mut prev = start;
        while mask != 0 {
            let i = mask.trailing_zeros() as usize;
            mask &= mask - 1;

            let mut zero_run = (i - prev) as u8;
            while zero_run > 15 {
                self.huffman_encode(0xF0, ac_table)?;
                zero_run -= 16;
            }

            crate::prof::bump(crate::prof::Count::NonZeroAc, 1);
            let (size, value) = get_code(block[i]);
            self.huffman_encode_value(size, (zero_run << 4) | size, value, ac_table)?;
            prev = i + 1;
        }

        // Trailing zeros terminate the block with EOB, exactly as a non-zero
        // `zero_run` did before.
        if prev < end {
            self.huffman_encode(0x00, ac_table)?;
        }

        Ok(())
    }

    pub fn write_frame_header(
        &mut self,
        width: u16,
        height: u16,
        components: &[Component],
        progressive: bool,
    ) -> Result<(), EncodingError> {
        if progressive {
            self.write_marker(Marker::SOF(SOFType::ProgressiveDCT))?;
        } else {
            self.write_marker(Marker::SOF(SOFType::BaselineDCT))?;
        }

        self.write_u16(2 + 1 + 2 + 2 + 1 + (components.len() as u16) * 3)?;

        // Precision
        self.write_u8(8)?;

        self.write_u16(height)?;
        self.write_u16(width)?;

        self.write_u8(components.len() as u8)?;

        for component in components.iter() {
            self.write_u8(component.id)?;
            self.write_u8(
                (component.horizontal_sampling_factor << 4) | component.vertical_sampling_factor,
            )?;
            self.write_u8(component.quantization_table)?;
        }

        Ok(())
    }

    pub fn write_scan_header(
        &mut self,
        components: &[&Component],
        spectral: Option<(u8, u8)>,
    ) -> Result<(), EncodingError> {
        self.write_marker(Marker::SOS)?;

        self.write_u16(2 + 1 + (components.len() as u16) * 2 + 3)?;

        self.write_u8(components.len() as u8)?;

        for component in components.iter() {
            self.write_u8(component.id)?;
            self.write_u8((component.dc_huffman_table << 4) | component.ac_huffman_table)?;
        }

        let (spectral_start, spectral_end) = spectral.unwrap_or((0, 63));

        // Start of spectral or predictor selection
        self.write_u8(spectral_start)?;

        // End of spectral selection
        self.write_u8(spectral_end)?;

        // Successive approximation bit position high and low
        self.write_u8(0)?;

        Ok(())
    }
}

/// Bitmask of the non-zero coefficients in `block[1..64]`, bit `i-1` set when
/// `block[i] != 0`.
///
/// The AC loop's job is to find ~16% non-zero coefficients among 63, and the
/// straightforward form pays a data-dependent branch on every one of them. That
/// scan measured **~12-13% of whole encode** on its own (paired double-run vs a
/// null arm), about a third of all entropy cost — so it is worth finding the
/// non-zeros with arithmetic instead of branches, and then visiting only those.
///
/// AVX2 compares 16 coefficients per instruction; `movemask` on the packed
/// comparison yields the bits directly. The scalar fallback is the oracle.
#[cfg(all(feature = "simd", any(target_arch = "x86", target_arch = "x86_64")))]
#[target_feature(enable = "avx2")]
unsafe fn nonzero_mask_avx2(block: &[i16; 64]) -> u64 {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;

    let p = block.as_ptr();
    let zero = _mm256_setzero_si256();
    let mut mask = 0u64;
    for i in 0..4 {
        // 16 coefficients per iteration.
        let v = _mm256_loadu_si256(p.add(i * 16) as *const __m256i);
        let eq = _mm256_cmpeq_epi16(v, zero);
        // `packs` interleaves the 128-bit lanes, so undo that before movemask;
        // otherwise bit k would not correspond to coefficient k.
        let packed = _mm256_packs_epi16(eq, eq);
        let ordered = _mm256_permute4x64_epi64::<0b11_01_10_00>(packed);
        // movemask gives 1 where the coefficient EQUALS zero; invert for non-zero.
        let m = !(_mm256_movemask_epi8(ordered) as u32) & 0xFFFF;
        mask |= (m as u64) << (i * 16);
    }
    mask
}

/// Scalar twin and oracle for [`nonzero_mask_avx2`].
#[inline]
fn nonzero_mask_scalar(block: &[i16; 64]) -> u64 {
    let mut mask = 0u64;
    for (i, &v) in block.iter().enumerate() {
        mask |= ((v != 0) as u64) << i;
    }
    mask
}

#[inline]
fn nonzero_mask(block: &[i16; 64]) -> u64 {
    #[cfg(all(feature = "simd", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: guarded by the runtime feature check; reads 64 i16 from a
            // fixed-size array.
            #[allow(unsafe_code)]
            unsafe {
                return nonzero_mask_avx2(block);
            }
        }
    }
    nonzero_mask_scalar(block)
}

#[inline]
pub(crate) fn get_code(value: i16) -> (u8, u16) {
    let temp = value - (value.is_negative() as i16);
    let temp2 = value.abs();

    /*
     * Doing this instead of 16 - temp2.leading_zeros()
     * Gives the compiler the information that leadings_zeros
     * is always called on a non zero value, which removes a branch on x86
     */
    let num_bits = 15 - (temp2 << 1 | 1).leading_zeros() as u16;

    let coefficient = temp & ((1 << num_bits as usize) - 1);

    (num_bits as u8, coefficient as u16)
}

#[cfg(test)]
mod nonzero_mask_tests {
    use super::*;

    /// The SIMD mask must equal the scalar oracle exactly. `packs` interleaves
    /// the 256-bit lanes, so a missing permute would still produce a
    /// plausible-looking mask with the two halves of every 16 coefficients
    /// swapped — which is precisely the bug this checks for.
    #[test]
    fn nonzero_mask_matches_scalar() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..3000 {
            let mut b = [0i16; 64];
            for (i, v) in b.iter_mut().enumerate() {
                *v = match round {
                    0 => 0,
                    1 => 1,
                    2 => -1,
                    3 => i16::MIN,
                    4 => i16::MAX,
                    // One coefficient set, walked across every position: this is
                    // what catches a lane-ordering error.
                    5..=68 => {
                        if i == round - 5 {
                            1
                        } else {
                            0
                        }
                    }
                    _ => {
                        // Sparse, like real quantized blocks (~16% non-zero).
                        if next() % 100 < 16 {
                            (next() % 64) as i16 - 32
                        } else {
                            0
                        }
                    }
                };
            }
            assert_eq!(nonzero_mask(&b), nonzero_mask_scalar(&b), "round {round}");
        }
    }
}
