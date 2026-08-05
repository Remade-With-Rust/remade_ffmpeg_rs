use crate::encode::fdct::fdct;
use crate::encode::huffman::{CodingClass, HuffmanTable};
use crate::encode::image_buffer::*;
use crate::encode::marker::Marker;
use crate::encode::quantization::{QuantizationTable, QuantizationTableType};
use crate::encode::writer::{JfifWrite, JfifWriter, ZIGZAG};
use crate::encode::{EncodingError, PixelDensity};

use alloc::vec;
use alloc::vec::Vec;

#[cfg(feature = "std")]
use std::io::BufWriter;

#[cfg(feature = "std")]
use std::fs::File;

#[cfg(feature = "std")]
use std::path::Path;

/// # Color types used in encoding
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum JpegColorType {
    /// One component grayscale colorspace
    Luma,

    /// Three component YCbCr colorspace
    Ycbcr,

    /// 4 Component CMYK colorspace
    Cmyk,

    /// 4 Component YCbCrK colorspace
    Ycck,
}

impl JpegColorType {
    pub(crate) fn get_num_components(self) -> usize {
        use JpegColorType::*;

        match self {
            Luma => 1,
            Ycbcr => 3,
            Cmyk | Ycck => 4,
        }
    }
}

/// # Color types for input images
///
/// Available color input formats for [Encoder::encode]. Other types can be used
/// by implementing an [ImageBuffer](crate::encode::ImageBuffer).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ColorType {
    /// Grayscale with 1 byte per pixel
    Luma,

    /// RGB with 3 bytes per pixel
    Rgb,

    /// Red, Green, Blue with 4 bytes per pixel. The alpha channel will be ignored during encoding.
    Rgba,

    /// RGB with 3 bytes per pixel
    Bgr,

    /// RGBA with 4 bytes per pixel. The alpha channel will be ignored during encoding.
    Bgra,

    /// YCbCr with 3 bytes per pixel.
    Ycbcr,

    /// CMYK with 4 bytes per pixel.
    Cmyk,

    /// CMYK with 4 bytes per pixel. Encoded as YCCK (YCbCrK)
    CmykAsYcck,

    /// YCCK (YCbCrK) with 4 bytes per pixel.
    Ycck,
}

impl ColorType {
    pub(crate) fn get_bytes_per_pixel(self) -> usize {
        use ColorType::*;

        match self {
            Luma => 1,
            Rgb | Bgr | Ycbcr => 3,
            Rgba | Bgra | Cmyk | CmykAsYcck | Ycck => 4,
        }
    }
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
/// # Sampling factors for chroma subsampling
///
/// ## Warning
/// Sampling factor of 4 are not supported by all decoders or applications
#[allow(non_camel_case_types)]
pub enum SamplingFactor {
    F_1_1 = 1 << 4 | 1,
    F_2_1 = 2 << 4 | 1,
    F_1_2 = 1 << 4 | 2,
    F_2_2 = 2 << 4 | 2,
    F_4_1 = 4 << 4 | 1,
    F_4_2 = 4 << 4 | 2,
    F_1_4 = 1 << 4 | 4,
    F_2_4 = 2 << 4 | 4,

    /// Alias for F_1_1
    R_4_4_4 = 0x80 | 1 << 4 | 1,

    /// Alias for F_1_2
    R_4_4_0 = 0x80 | 1 << 4 | 2,

    /// Alias for F_1_4
    R_4_4_1 = 0x80 | 1 << 4 | 4,

    /// Alias for F_2_1
    R_4_2_2 = 0x80 | 2 << 4 | 1,

    /// Alias for F_2_2
    R_4_2_0 = 0x80 | 2 << 4 | 2,

    /// Alias for F_2_4
    R_4_2_1 = 0x80 | 2 << 4 | 4,

    /// Alias for F_4_1
    R_4_1_1 = 0x80 | 4 << 4 | 1,

    /// Alias for F_4_2
    R_4_1_0 = 0x80 | 4 << 4 | 2,
}

impl SamplingFactor {
    /// Get variant for supplied factors or None if not supported
    pub fn from_factors(horizontal: u8, vertical: u8) -> Option<SamplingFactor> {
        use SamplingFactor::*;

        match (horizontal, vertical) {
            (1, 1) => Some(F_1_1),
            (1, 2) => Some(F_1_2),
            (1, 4) => Some(F_1_4),
            (2, 1) => Some(F_2_1),
            (2, 2) => Some(F_2_2),
            (2, 4) => Some(F_2_4),
            (4, 1) => Some(F_4_1),
            (4, 2) => Some(F_4_2),
            _ => None,
        }
    }

    pub(crate) fn get_sampling_factors(self) -> (u8, u8) {
        let value = self as u8;
        ((value >> 4) & 0x07, value & 0xf)
    }

    pub(crate) fn supports_interleaved(self) -> bool {
        use SamplingFactor::*;

        // Interleaved mode is only supported with h/v sampling factors of 1 or 2.
        // Sampling factors of 4 needs sequential encoding
        matches!(
            self,
            F_1_1 | F_2_1 | F_1_2 | F_2_2 | R_4_4_4 | R_4_4_0 | R_4_2_2 | R_4_2_0
        )
    }
}

/// Quantized blocks per component, plus each component's `(cols, rows)`
/// block grid — the grid is needed to walk them in MCU order, and it is not
/// recoverable from the block count alone for ragged image sizes.
type MaterializedBlocks = ([Vec<[i16; 64]>; 4], [(usize, usize); 4]);

#[derive(Clone)]
pub(crate) struct Component {
    pub id: u8,
    pub quantization_table: u8,
    pub dc_huffman_table: u8,
    pub ac_huffman_table: u8,
    pub horizontal_sampling_factor: u8,
    pub vertical_sampling_factor: u8,
}

macro_rules! add_component {
    ($components:expr, $id:expr, $dest:expr, $h_sample:expr, $v_sample:expr) => {
        $components.push(Component {
            id: $id,
            quantization_table: $dest,
            dc_huffman_table: $dest,
            ac_huffman_table: $dest,
            horizontal_sampling_factor: $h_sample,
            vertical_sampling_factor: $v_sample,
        });
    };
}

/// # The JPEG encoder
pub struct Encoder<W: JfifWrite> {
    writer: JfifWriter<W>,
    density: PixelDensity,
    quality: u8,

    components: Vec<Component>,
    quantization_tables: [QuantizationTableType; 2],
    huffman_tables: [(HuffmanTable, HuffmanTable); 2],

    sampling_factor: SamplingFactor,

    progressive_scans: Option<u8>,

    restart_interval: Option<u16>,

    optimize_huffman_table: bool,
    /// `None` = choose by [`OPTIMIZE_BUFFER_BUDGET`].
    streaming_optimize: Option<bool>,
    branchy_quantize: bool,
    /// Rate-distortion optimization of quantized coefficients.
    trellis: bool,
    push_blocks: bool,

    app_segments: Vec<(u8, Vec<u8>)>,
}

impl<W: JfifWrite> Encoder<W> {
    /// Create a new encoder with the given quality
    ///
    /// The quality must be between 1 and 100 where 100 is the highest image quality.<br>
    /// By default, quality settings below 90 use a chroma subsampling (2x2 / 4:2:0) which can
    /// be changed with [set_sampling_factor](Encoder::set_sampling_factor)
    pub fn new(w: W, quality: u8) -> Encoder<W> {
        let huffman_tables = [
            (
                HuffmanTable::default_luma_dc(),
                HuffmanTable::default_luma_ac(),
            ),
            (
                HuffmanTable::default_chroma_dc(),
                HuffmanTable::default_chroma_ac(),
            ),
        ];

        let quantization_tables = [
            QuantizationTableType::Default,
            QuantizationTableType::Default,
        ];

        let sampling_factor = if quality < 90 {
            SamplingFactor::F_2_2
        } else {
            SamplingFactor::F_1_1
        };

        Encoder {
            writer: JfifWriter::new(w),
            density: PixelDensity::default(),
            quality,
            components: vec![],
            quantization_tables,
            huffman_tables,
            sampling_factor,
            progressive_scans: None,
            restart_interval: None,
            optimize_huffman_table: false,
            streaming_optimize: None,
            branchy_quantize: false,
            // OFF by default. It shipped on in 0.1.7-0.2.2 on the strength of
            // "-3.14% BD-rate for +3.1% encode time" — and that +3.1% was wrong
            // by ~46x. It was calibrated on synthetic fBm at ~223 KB/frame,
            // while trellis work is O(non-zero coefficients) per block in f64;
            // on real 1080p footage at ~700 KB/frame it measures **+144%**
            // (844 -> 2062 ms pinned CPU, 40 frames).
            //
            // That is a bad trade for a codec positioned as a fast drop-in, and
            // it is the whole reason the crate's own "1.19x faster than FFmpeg"
            // claim stopped holding. Opt in with `set_trellis(true)` or
            // `-trellis 1` when smaller files are worth the time; the BD-rate
            // benefit itself still needs re-measuring on real content, since the
            // cost figure from that same corpus was so far off.
            trellis: false,
            push_blocks: false,
            app_segments: Vec::new(),
        }
    }

    /// Set pixel density for the image
    ///
    /// By default, this value is None which is equal to "1 pixel per pixel".
    pub fn set_density(&mut self, density: PixelDensity) {
        self.density = density;
    }

    /// Return pixel density
    pub fn density(&self) -> PixelDensity {
        self.density
    }

    /// Set chroma subsampling factor
    pub fn set_sampling_factor(&mut self, sampling: SamplingFactor) {
        self.sampling_factor = sampling;
    }

    /// Get chroma subsampling factor
    pub fn sampling_factor(&self) -> SamplingFactor {
        self.sampling_factor
    }

    /// Set quantization tables for luma and chroma components
    pub fn set_quantization_tables(
        &mut self,
        luma: QuantizationTableType,
        chroma: QuantizationTableType,
    ) {
        self.quantization_tables = [luma, chroma];
    }

    /// Get configured quantization tables
    pub fn quantization_tables(&self) -> &[QuantizationTableType; 2] {
        &self.quantization_tables
    }

    /// Controls if progressive encoding is used.
    ///
    /// By default, progressive encoding uses 4 scans.<br>
    /// Use [set_progressive_scans](Encoder::set_progressive_scans) to use a different number of scans
    pub fn set_progressive(&mut self, progressive: bool) {
        self.progressive_scans = if progressive { Some(4) } else { None };
    }

    /// Set number of scans per component for progressive encoding
    ///
    /// Number of scans must be between 2 and 64.
    /// There is at least one scan for the DC coefficients and one for the remaining 63 AC coefficients.
    ///
    /// # Panics
    /// If number of scans is not within valid range
    pub fn set_progressive_scans(&mut self, scans: u8) {
        assert!(
            (2..=64).contains(&scans),
            "Invalid number of scans: {}",
            scans
        );
        self.progressive_scans = Some(scans);
    }

    /// Return number of progressive scans if progressive encoding is enabled
    pub fn progressive_scans(&self) -> Option<u8> {
        self.progressive_scans
    }

    /// Set restart interval
    ///
    /// Set numbers of MCUs between restart markers.
    pub fn set_restart_interval(&mut self, interval: u16) {
        self.restart_interval = if interval == 0 { None } else { Some(interval) };
    }

    /// Return the restart interval
    pub fn restart_interval(&self) -> Option<u16> {
        self.restart_interval
    }

    /// Measurement knob: build the block list by `push`ing a stack temporary
    /// (the original form) rather than quantizing into a pre-sized slot.
    pub fn set_push_blocks(&mut self, push: bool) {
        self.push_blocks = push;
    }

    /// Measurement knob: use the old branching quantizer. Output is
    /// bit-identical either way (`quantize_matches_branchy` proves it
    /// exhaustively), so this only exists to time the two against each other.
    pub fn set_branchy_quantize(&mut self, branchy: bool) {
        self.branchy_quantize = branchy;
    }

    /// Force how optimized Huffman tables are gathered, overriding the automatic
    /// choice described on [`Encoder::use_streaming_optimize`].
    ///
    /// `true` makes two streaming passes over the image — count, then write.
    /// `false` materializes every quantized block once and counts from that.
    /// Both produce valid, equivalently-sized output; the trade is repeated
    /// transform work against peak memory.
    /// Enable rate-distortion optimization of the quantized coefficients.
    ///
    /// Chooses where each block's EOB falls, and lowers coefficient magnitudes
    /// where the bits saved outweigh the distortion, instead of keeping whatever
    /// rounding produced. Changes the bitstream.
    ///
    /// **Off by default, and expensive.** Measured on real 1080p footage it
    /// costs **+144% encode time** — a figure that was originally published as
    /// +3.1% because it had been calibrated on synthetic content with far fewer
    /// non-zero coefficients than real material. Turn it on when smaller files
    /// are worth roughly 2.4x the encode time.
    pub fn set_trellis(&mut self, enabled: bool) {
        self.trellis = enabled;
    }

    pub fn set_streaming_optimize(&mut self, streaming: bool) {
        self.streaming_optimize = Some(streaming);
    }

    /// The forced strategy, or `None` when chosen automatically by size.
    pub fn streaming_optimize(&self) -> Option<bool> {
        self.streaming_optimize
    }

    /// Set if optimized huffman table should be created
    ///
    /// Optimized tables result in slightly smaller file sizes but decrease encoding performance.
    pub fn set_optimized_huffman_tables(&mut self, optimize_huffman_table: bool) {
        self.optimize_huffman_table = optimize_huffman_table;
    }

    /// Returns if optimized huffman table should be generated
    pub fn optimized_huffman_tables(&self) -> bool {
        self.optimize_huffman_table
    }

    /// Appends a custom app segment to the JFIF file
    ///
    /// Segment numbers need to be in the range between 1 and 15<br>
    /// The maximum allowed data length is 2^16 - 2 bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the segment number is invalid or data exceeds the allowed size
    pub fn add_app_segment(&mut self, segment_nr: u8, data: Vec<u8>) -> Result<(), EncodingError> {
        if segment_nr == 0 || segment_nr > 15 {
            Err(EncodingError::InvalidAppSegment(segment_nr))
        } else if data.len() > 65533 {
            Err(EncodingError::AppSegmentTooLarge(data.len()))
        } else {
            self.app_segments.push((segment_nr, data));
            Ok(())
        }
    }

    /// Add an ICC profile
    ///
    /// The maximum allowed data length is 16,707,345 bytes.
    ///
    /// # Errors
    ///
    /// Returns an Error if the data exceeds the maximum size for the ICC profile
    pub fn add_icc_profile(&mut self, data: &[u8]) -> Result<(), EncodingError> {
        // Based on https://www.color.org/ICC_Minor_Revision_for_Web.pdf
        // B.4  Embedding ICC profiles in JFIF files

        const MARKER: &[u8; 12] = b"ICC_PROFILE\0";
        const MAX_CHUNK_LENGTH: usize = 65535 - 2 - 12 - 2;

        let num_chunks = ceil_div(data.len(), MAX_CHUNK_LENGTH);

        // Sequence number is stored as a byte and starts with 1
        if num_chunks >= 255 {
            return Err(EncodingError::IccTooLarge(data.len()));
        }

        for (i, data) in data.chunks(MAX_CHUNK_LENGTH).enumerate() {
            let mut chunk_data = Vec::with_capacity(MAX_CHUNK_LENGTH);
            chunk_data.extend_from_slice(MARKER);
            chunk_data.push(i as u8 + 1);
            chunk_data.push(num_chunks as u8);
            chunk_data.extend_from_slice(data);

            self.add_app_segment(2, chunk_data)?;
        }

        Ok(())
    }

    /// Embeds Exif metadata into the image
    ///
    /// The maximum allowed data length is 65,528 bytes.
    ///
    /// # Errors
    ///
    /// Returns an Error if the data exceeds the maximum size for the Exif metadata
    pub fn add_exif_metadata(&mut self, data: &[u8]) -> Result<(), EncodingError> {
        // E x i f \0 \0
        /// The header for an EXIF APP1 segment
        const EXIF_HEADER: [u8; 6] = [0x45, 0x78, 0x69, 0x66, 0x00, 0x00];

        let mut formatted = EXIF_HEADER.to_vec();
        formatted.extend_from_slice(data);

        self.add_app_segment(1, formatted)
    }

    /// Encode an image
    ///
    /// Data format and length must conform to specified width, height and color type.
    pub fn encode(
        self,
        data: &[u8],
        width: u16,
        height: u16,
        color_type: ColorType,
    ) -> Result<(), EncodingError> {
        let required_data_len = width as usize * height as usize * color_type.get_bytes_per_pixel();

        if data.len() < required_data_len {
            return Err(EncodingError::BadImageData {
                length: data.len(),
                required: required_data_len,
            });
        }

        #[cfg(all(feature = "simd", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            if std::is_x86_feature_detected!("avx2") {
                use crate::encode::avx2::*;

                return match color_type {
                    ColorType::Luma => self
                        .encode_image_internal::<_, AVX2Operations>(GrayImage(data, width, height)),
                    ColorType::Rgb => self.encode_image_internal::<_, AVX2Operations>(
                        RgbImageAVX2(data, width, height),
                    ),
                    ColorType::Rgba => self.encode_image_internal::<_, AVX2Operations>(
                        RgbaImageAVX2(data, width, height),
                    ),
                    ColorType::Bgr => self.encode_image_internal::<_, AVX2Operations>(
                        BgrImageAVX2(data, width, height),
                    ),
                    ColorType::Bgra => self.encode_image_internal::<_, AVX2Operations>(
                        BgraImageAVX2(data, width, height),
                    ),
                    ColorType::Ycbcr => self.encode_image_internal::<_, AVX2Operations>(
                        YCbCrImage(data, width, height),
                    ),
                    ColorType::Cmyk => self
                        .encode_image_internal::<_, AVX2Operations>(CmykImage(data, width, height)),
                    ColorType::CmykAsYcck => self.encode_image_internal::<_, AVX2Operations>(
                        CmykAsYcckImage(data, width, height),
                    ),
                    ColorType::Ycck => self
                        .encode_image_internal::<_, AVX2Operations>(YcckImage(data, width, height)),
                };
            }
        }

        match color_type {
            ColorType::Luma => self.encode_image(GrayImage(data, width, height))?,
            ColorType::Rgb => self.encode_image(RgbImage(data, width, height))?,
            ColorType::Rgba => self.encode_image(RgbaImage(data, width, height))?,
            ColorType::Bgr => self.encode_image(BgrImage(data, width, height))?,
            ColorType::Bgra => self.encode_image(BgraImage(data, width, height))?,
            ColorType::Ycbcr => self.encode_image(YCbCrImage(data, width, height))?,
            ColorType::Cmyk => self.encode_image(CmykImage(data, width, height))?,
            ColorType::CmykAsYcck => self.encode_image(CmykAsYcckImage(data, width, height))?,
            ColorType::Ycck => self.encode_image(YcckImage(data, width, height))?,
        }

        Ok(())
    }

    /// Encode an image
    pub fn encode_image<I: ImageBuffer>(self, image: I) -> Result<(), EncodingError> {
        let _t = crate::prof::scope(crate::prof::Stage::Total);
        #[cfg(all(feature = "simd", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            if std::is_x86_feature_detected!("avx2") {
                use crate::encode::avx2::*;
                if self.branchy_quantize {
                    return self.encode_image_internal::<_, BranchyQuantizeOperations>(image);
                }
                return self.encode_image_internal::<_, AVX2Operations>(image);
            }
        }
        // NEON is baseline on aarch64, so this needs no runtime detection. Only
        // quantize is vectorized here; the forward DCT stays scalar (see
        // `encode::neon` for why that gap is deliberate rather than an oversight).
        #[cfg(all(feature = "simd", target_arch = "aarch64"))]
        {
            if !self.branchy_quantize {
                return self.encode_image_internal::<_, crate::encode::neon::NeonOperations>(image);
            }
        }
        if self.branchy_quantize {
            return self.encode_image_internal::<_, BranchyQuantizeOperations>(image);
        }
        self.encode_image_internal::<_, DefaultOperations>(image)
    }

    fn encode_image_internal<I: ImageBuffer, OP: Operations>(
        mut self,
        image: I,
    ) -> Result<(), EncodingError> {
        if image.width() == 0 || image.height() == 0 {
            return Err(EncodingError::ZeroImageDimensions {
                width: image.width(),
                height: image.height(),
            });
        }

        let q_tables = [
            QuantizationTable::new_with_quality(&self.quantization_tables[0], self.quality, true),
            QuantizationTable::new_with_quality(&self.quantization_tables[1], self.quality, false),
        ];

        let jpeg_color_type = image.get_jpeg_color_type();
        self.init_components(jpeg_color_type);

        self.writer.write_marker(Marker::SOI)?;

        self.writer.write_header(&self.density)?;

        if jpeg_color_type == JpegColorType::Cmyk {
            //Set ColorTransform info to "Unknown"
            let app_14 = b"Adobe\0\0\0\0\0\0\0";
            self.writer
                .write_segment(Marker::APP(14), app_14.as_ref())?;
        } else if jpeg_color_type == JpegColorType::Ycck {
            //Set ColorTransform info to YCCK
            let app_14 = b"Adobe\0\0\0\0\0\0\x02";
            self.writer
                .write_segment(Marker::APP(14), app_14.as_ref())?;
        }

        for (nr, data) in &self.app_segments {
            self.writer.write_segment(Marker::APP(*nr), data)?;
        }

        if let Some(scans) = self.progressive_scans {
            // Progressive genuinely needs the coefficients kept: every scan
            // revisits the same blocks.
            self.encode_image_progressive::<_, OP>(image, scans, &q_tables)?;
        } else if self.optimize_huffman_table
            && self.use_streaming_optimize(&image)
            && self.sampling_factor.supports_interleaved()
        {
            // Two streaming passes over the image: count, then write. Costs one
            // extra DCT+quantize (~8% of encode) and saves materializing every
            // block (~218 MB at 4K, plus the write and read back).
            let mut stats = HuffmanStats::default();
            self.scan_interleaved::<_, OP>(&image, &q_tables, Some(&mut stats))?;
            self.optimize_huffman_table_from_stats(&stats);
            self.scan_interleaved::<_, OP>(&image, &q_tables, None)?;
        } else if self.optimize_huffman_table || !self.sampling_factor.supports_interleaved() {
            self.encode_image_sequential::<_, OP>(image, &q_tables)?;
        } else {
            self.encode_image_interleaved::<_, OP>(image, &q_tables)?;
        }

        self.writer.write_marker(Marker::EOI)?;
        // Everything above went into the writer's staging buffer.
        self.writer.flush_output()?;

        Ok(())
    }

    fn init_components(&mut self, color: JpegColorType) {
        let (horizontal_sampling_factor, vertical_sampling_factor) =
            self.sampling_factor.get_sampling_factors();

        match color {
            JpegColorType::Luma => {
                add_component!(self.components, 0, 0, 1, 1);
            }
            JpegColorType::Ycbcr => {
                add_component!(
                    self.components,
                    0,
                    0,
                    horizontal_sampling_factor,
                    vertical_sampling_factor
                );
                add_component!(self.components, 1, 1, 1, 1);
                add_component!(self.components, 2, 1, 1, 1);
            }
            JpegColorType::Cmyk => {
                add_component!(self.components, 0, 1, 1, 1);
                add_component!(self.components, 1, 1, 1, 1);
                add_component!(self.components, 2, 1, 1, 1);
                add_component!(
                    self.components,
                    3,
                    0,
                    horizontal_sampling_factor,
                    vertical_sampling_factor
                );
            }
            JpegColorType::Ycck => {
                add_component!(
                    self.components,
                    0,
                    0,
                    horizontal_sampling_factor,
                    vertical_sampling_factor
                );
                add_component!(self.components, 1, 1, 1, 1);
                add_component!(self.components, 2, 1, 1, 1);
                add_component!(
                    self.components,
                    3,
                    0,
                    horizontal_sampling_factor,
                    vertical_sampling_factor
                );
            }
        }
    }

    fn get_max_sampling_size(&self) -> (usize, usize) {
        let max_h_sampling = self.components.iter().fold(1, |value, component| {
            value.max(component.horizontal_sampling_factor)
        });

        let max_v_sampling = self.components.iter().fold(1, |value, component| {
            value.max(component.vertical_sampling_factor)
        });

        (usize::from(max_h_sampling), usize::from(max_v_sampling))
    }

    fn write_frame_header<I: ImageBuffer>(
        &mut self,
        image: &I,
        q_tables: &[QuantizationTable; 2],
    ) -> Result<(), EncodingError> {
        self.writer.write_frame_header(
            image.width(),
            image.height(),
            &self.components,
            self.progressive_scans.is_some(),
        )?;

        self.writer.write_quantization_segment(0, &q_tables[0])?;
        self.writer.write_quantization_segment(1, &q_tables[1])?;

        self.writer
            .write_huffman_segment(CodingClass::Dc, 0, &self.huffman_tables[0].0)?;

        self.writer
            .write_huffman_segment(CodingClass::Ac, 0, &self.huffman_tables[0].1)?;

        if image.get_jpeg_color_type().get_num_components() >= 3 {
            self.writer
                .write_huffman_segment(CodingClass::Dc, 1, &self.huffman_tables[1].0)?;

            self.writer
                .write_huffman_segment(CodingClass::Ac, 1, &self.huffman_tables[1].1)?;
        }

        if let Some(restart_interval) = self.restart_interval {
            self.writer.write_dri(restart_interval)?;
        }

        Ok(())
    }

    fn init_rows(&mut self, buffer_size: usize) -> [Vec<u8>; 4] {
        // To simplify the code and to give the compiler more infos to optimize stuff we always initialize 4 components
        // Resource overhead should be minimal because an empty Vec doesn't allocate

        match self.components.len() {
            1 => [
                Vec::with_capacity(buffer_size),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ],
            3 => [
                Vec::with_capacity(buffer_size),
                Vec::with_capacity(buffer_size),
                Vec::with_capacity(buffer_size),
                Vec::new(),
            ],
            4 => [
                Vec::with_capacity(buffer_size),
                Vec::with_capacity(buffer_size),
                Vec::with_capacity(buffer_size),
                Vec::with_capacity(buffer_size),
            ],
            len => unreachable!("Unsupported component length: {}", len),
        }
    }

    /// Encode all components with one scan
    ///
    /// This is only valid for sampling factors of 1 and 2
    fn encode_image_interleaved<I: ImageBuffer, OP: Operations>(
        &mut self,
        image: I,
        q_tables: &[QuantizationTable; 2],
    ) -> Result<(), EncodingError> {
        self.scan_interleaved::<_, OP>(&image, q_tables, None)
    }

    /// One interleaved (MCU-order) pass over the image.
    ///
    /// With `stats = None` it writes the scan. With `stats = Some(..)` it writes
    /// nothing and only accumulates Huffman symbol frequencies — the same
    /// traversal, so the DC predictor chain is identical. That matters: MCU
    /// order visits a component's blocks in a different sequence than raster
    /// order, so DC differences (and therefore the histogram) differ between
    /// them. Counting in any other order would build tables for a scan we are
    /// not going to write.
    fn scan_interleaved<I: ImageBuffer, OP: Operations>(
        &mut self,
        image: &I,
        q_tables: &[QuantizationTable; 2],
        mut stats: Option<&mut HuffmanStats>,
    ) -> Result<(), EncodingError> {
        if stats.is_none() {
            self.write_frame_header(image, q_tables)?;
            self.writer
                .write_scan_header(&self.components.iter().collect::<Vec<_>>(), None)?;
        }

        let (max_h_sampling, max_v_sampling) = self.get_max_sampling_size();

        let width = image.width();
        let height = image.height();

        let num_cols = ceil_div(usize::from(width), 8 * max_h_sampling);
        let num_rows = ceil_div(usize::from(height), 8 * max_v_sampling);

        let buffer_width = num_cols * 8 * max_h_sampling;
        let buffer_size = buffer_width * 8 * max_v_sampling;

        let mut row: [Vec<_>; 4] = self.init_rows(buffer_size);

        let mut prev_dc = [0i16; 4];

        let restart_interval = self.restart_interval.unwrap_or(0);
        let mut restarts = 0;
        let mut restarts_to_go = restart_interval;

        for block_y in 0..num_rows {
            {
                let _rs = crate::prof::scope(crate::prof::Stage::RowSetup);
                for r in &mut row {
                    r.clear();
                }
            }
            for y in 0..(8 * max_v_sampling) {
                let y = y + block_y * 8 * max_v_sampling;
                let y = (y.min(height as usize - 1)) as u16;

                {
                    let _s = crate::prof::scope(crate::prof::Stage::FillBuffers);
                    image.fill_buffers(y, &mut row);
                }

                {
                    let _rs = crate::prof::scope(crate::prof::Stage::RowSetup);
                    for _ in usize::from(width)..buffer_width {
                        for channel in &mut row {
                            if !channel.is_empty() {
                                channel.push(channel[channel.len() - 1]);
                            }
                        }
                    }
                }
            }

            for block_x in 0..num_cols {
                if restart_interval > 0 && restarts_to_go == 0 {
                    if stats.is_none() {
                        self.writer.finalize_bit_buffer()?;
                        self.writer
                            .write_marker(Marker::RST((restarts % 8) as u8))?;
                    }
                    prev_dc[0] = 0;
                    prev_dc[1] = 0;
                    prev_dc[2] = 0;
                    prev_dc[3] = 0;
                }

                for (i, component) in self.components.iter().enumerate() {
                    for v_offset in 0..component.vertical_sampling_factor as usize {
                        for h_offset in 0..component.horizontal_sampling_factor as usize {
                            let mut block = {
                                let _gb = crate::prof::scope(crate::prof::Stage::GetBlock);
                                get_block(
                                    &row[i],
                                    block_x * 8 * max_h_sampling + (h_offset * 8),
                                    v_offset * 8,
                                    max_h_sampling / component.horizontal_sampling_factor as usize,
                                    max_v_sampling / component.vertical_sampling_factor as usize,
                                    buffer_width,
                                )
                            };

                            // The `copy` arm is the null: it pays the scratch
                            // duplication that doubling costs, and nothing else.
                            {
                                let _s = crate::prof::scope(crate::prof::Stage::Fdct);
                                OP::fdct(&mut block);
                            }

                            let mut q_block = [0i16; 64];

                            {
                                let _s = crate::prof::scope(crate::prof::Stage::Quantize);
                                OP::quantize_block(
                                    &block,
                                    &mut q_block,
                                    &q_tables[component.quantization_table as usize],
                                );
                            }

                            // Must run in BOTH passes of the streaming route:
                            // the histogram has to be counted over the symbols
                            // that will actually be written, or the optimized
                            // tables are built for a different bitstream.
                            if self.trellis {
                                crate::encode::trellis::truncate_rd(
                                    &block,
                                    &mut q_block,
                                    &q_tables[component.quantization_table as usize],
                                    &self.huffman_tables[component.ac_huffman_table as usize].1,
                                );
                            }

                            match stats.as_deref_mut() {
                                Some(st) => {
                                    let _s =
                                        crate::prof::scope(crate::prof::Stage::HuffmanOptimize);
                                    let dc = component.dc_huffman_table as usize;
                                    let ac = component.ac_huffman_table as usize;
                                    JfifWriter::<W>::count_block(
                                        &q_block,
                                        prev_dc[i],
                                        &mut st.dc[dc],
                                        &mut st.ac[ac],
                                    );
                                }
                                None => {
                                    let _s = crate::prof::scope(crate::prof::Stage::Entropy);
                                    self.writer.write_block(
                                        &q_block,
                                        prev_dc[i],
                                        &self.huffman_tables[component.dc_huffman_table as usize].0,
                                        &self.huffman_tables[component.ac_huffman_table as usize].1,
                                    )?;
                                }
                            }

                            prev_dc[i] = q_block[0];
                        }
                    }
                }

                if restart_interval > 0 {
                    if restarts_to_go == 0 {
                        restarts_to_go = restart_interval;
                        restarts += 1;
                        restarts &= 7;
                    }
                    restarts_to_go -= 1;
                }
            }
        }

        if stats.is_none() {
            self.writer.finalize_bit_buffer()?;
        }

        Ok(())
    }

    /// Visit the materialized blocks in MCU (interleaved) order.
    ///
    /// `encode_blocks` sizes its BUFFER to MCU-aligned dimensions but rebuilds
    /// the BLOCK GRID from the unaligned `ceil_div(dim, 8)` — for 1080p luma
    /// that is 240x135, while an interleaved walk wants 136 rows. The last MCU
    /// row therefore has no materialized second block row.
    ///
    /// Coordinates are clamped to the last real block, which is valid MCU
    /// padding: those samples fall outside the image and the decoder discards
    /// them. (The streaming path pads differently — it reads edge-replicated
    /// pixels from the padded buffer — so the two routes can differ by a few
    /// bytes in the final MCU. Both are conformant; neither affects any pixel
    /// the decoder keeps.)
    fn for_each_block_interleaved(
        components: &[Component],
        blocks: &[Vec<[i16; 64]>; 4],
        grid: &[(usize, usize); 4],
        mcu_cols: usize,
        mcu_rows: usize,
        mut f: impl FnMut(usize, usize, &[i16; 64]),
    ) {
        for my in 0..mcu_rows {
            for mx in 0..mcu_cols {
                for (i, c) in components.iter().enumerate() {
                    let ch = c.horizontal_sampling_factor as usize;
                    let cv = c.vertical_sampling_factor as usize;
                    let (cols_i, rows_i) = grid[i];
                    for v in 0..cv {
                        for h in 0..ch {
                            let bx = (mx * ch + h).min(cols_i - 1);
                            let by = (my * cv + v).min(rows_i - 1);
                            f(my * mcu_cols + mx, i, &blocks[i][by * cols_i + bx]);
                        }
                    }
                }
            }
        }
    }

    /// One INTERLEAVED scan written from materialized blocks.
    ///
    /// The block-materializing route used to emit one scan PER COMPONENT, which
    /// is legal but a layout mainstream encoders never produce and some hardware
    /// decoders handle poorly — and which route was taken depended on nothing
    /// but an internal memory budget, so the scan layout of the output changed
    /// with resolution. This keeps the materializing route's speed and gives it
    /// the standard layout.
    ///
    /// The Huffman histogram is gathered in the SAME MCU order it is written in.
    /// That matters: MCU order visits a component's blocks in a different
    /// sequence than raster order, so the DC differences — and therefore the
    /// symbol frequencies — differ between them. Counting in raster order would
    /// build tables for a scan that is not the one being written.
    /// Gather the Huffman histogram in MCU order.
    ///
    /// MUST run BEFORE `write_frame_header`, which emits the DHT segments: the
    /// tables written into the file have to be the tables the scan is coded
    /// with. Getting this backwards produces a file our own decoder happens to
    /// tolerate and ffmpeg rejects outright ("error count: 65") — which is why
    /// the interop check earns its place in the gate.
    fn optimize_huffman_interleaved(
        &mut self,
        blocks: &[Vec<[i16; 64]>; 4],
        grid: &[(usize, usize); 4],
        mcu_cols: usize,
        mcu_rows: usize,
    ) {
        let _s = crate::prof::scope(crate::prof::Stage::HuffmanOptimize);
        let components = self.components.clone();
        let restart_interval = self.restart_interval.unwrap_or(0) as usize;
        let mut stats = HuffmanStats::default();
        let mut prev_dc = [0i16; 4];
        let mut last_mcu = usize::MAX;
        Self::for_each_block_interleaved(
            &components,
            blocks,
            grid,
            mcu_cols,
            mcu_rows,
            |mcu, i, block| {
                if restart_interval > 0 && mcu != last_mcu && mcu % restart_interval == 0 {
                    prev_dc = [0i16; 4];
                }
                last_mcu = mcu;
                let c = &components[i];
                JfifWriter::<W>::count_block(
                    block,
                    prev_dc[i],
                    &mut stats.dc[c.dc_huffman_table as usize],
                    &mut stats.ac[c.ac_huffman_table as usize],
                );
                prev_dc[i] = block[0];
            },
        );
        self.optimize_huffman_table_from_stats(&stats);
    }

    fn write_interleaved_from_blocks(
        &mut self,
        blocks: &[Vec<[i16; 64]>; 4],
        grid: &[(usize, usize); 4],
        mcu_cols: usize,
        mcu_rows: usize,
    ) -> Result<(), EncodingError> {
        let components = self.components.clone();

        self.writer
            .write_scan_header(&components.iter().collect::<Vec<_>>(), None)?;

        let restart_interval = self.restart_interval.unwrap_or(0) as usize;
        let mut prev_dc = [0i16; 4];
        let mut restarts = 0usize;
        let mut last_mcu = usize::MAX;

        // Walk MCU order directly rather than materializing it.
        //
        // This used to collect `Vec<(usize, usize, [i16; 64])>` first, purely so
        // the `self.writer` borrow stayed clear of a closure borrowing `blocks`
        // — a borrow-checker workaround that copied every block a SECOND time:
        // 48,960 x 144 B = **7.1 MB per 1080p frame**, on the path the CLI takes
        // by default (`optimize_huffman` is on unless asked otherwise).
        //
        // The traversal is plain index arithmetic, so writing it out inline
        // costs nothing and borrows only what it reads.
        for my in 0..mcu_rows {
            for mx in 0..mcu_cols {
                let mcu = my * mcu_cols + mx;
                if restart_interval > 0
                    && mcu != last_mcu
                    && mcu % restart_interval == 0
                    && mcu != 0
                {
                    self.writer.finalize_bit_buffer()?;
                    self.writer
                        .write_marker(Marker::RST((restarts % 8) as u8))?;
                    restarts += 1;
                    prev_dc = [0i16; 4];
                }
                last_mcu = mcu;

                for (i, c) in components.iter().enumerate() {
                    let ch = c.horizontal_sampling_factor as usize;
                    let cv = c.vertical_sampling_factor as usize;
                    let (cols_i, rows_i) = grid[i];
                    for v in 0..cv {
                        for h in 0..ch {
                            let bx = (mx * ch + h).min(cols_i - 1);
                            let by = (my * cv + v).min(rows_i - 1);
                            let block = &blocks[i][by * cols_i + bx];
                            {
                                let _s = crate::prof::scope(crate::prof::Stage::Entropy);
                                self.writer.write_block(
                                    block,
                                    prev_dc[i],
                                    &self.huffman_tables[c.dc_huffman_table as usize].0,
                                    &self.huffman_tables[c.ac_huffman_table as usize].1,
                                )?;
                            }
                            prev_dc[i] = block[0];
                        }
                    }
                }
            }
        }
        self.writer.finalize_bit_buffer()?;
        Ok(())
    }

    /// Encode components with one scan per component
    fn encode_image_sequential<I: ImageBuffer, OP: Operations>(
        &mut self,
        image: I,
        q_tables: &[QuantizationTable; 2],
    ) -> Result<(), EncodingError> {
        let (blocks, grid) = self.encode_blocks::<_, OP>(&image, q_tables);

        // Prefer the standard interleaved layout whenever the sampling factors
        // allow it. Only genuinely non-interleavable sampling falls through to
        // per-component scans below.
        if self.sampling_factor.supports_interleaved() {
            let (max_h, max_v) = self.get_max_sampling_size();
            let mcu_cols = ceil_div(usize::from(image.width()), 8 * max_h);
            let mcu_rows = ceil_div(usize::from(image.height()), 8 * max_v);
            if self.optimize_huffman_table {
                self.optimize_huffman_interleaved(&blocks, &grid, mcu_cols, mcu_rows);
            }
            // Only now may the header go out: it carries the DHT segments.
            self.write_frame_header(&image, q_tables)?;
            return self.write_interleaved_from_blocks(&blocks, &grid, mcu_cols, mcu_rows);
        }

        if self.optimize_huffman_table {
            let _s = crate::prof::scope(crate::prof::Stage::HuffmanOptimize);
            self.optimize_huffman_table(&blocks);
        }

        self.write_frame_header(&image, q_tables)?;

        for (i, component) in self.components.iter().enumerate() {
            let restart_interval = self.restart_interval.unwrap_or(0);
            let mut restarts = 0;
            let mut restarts_to_go = restart_interval;

            self.writer.write_scan_header(&[component], None)?;

            let mut prev_dc = 0;

            for block in &blocks[i] {
                if restart_interval > 0 && restarts_to_go == 0 {
                    self.writer.finalize_bit_buffer()?;
                    self.writer
                        .write_marker(Marker::RST((restarts % 8) as u8))?;

                    prev_dc = 0;
                }

                {
                    let _s = crate::prof::scope(crate::prof::Stage::Entropy);
                    self.writer.write_block(
                        block,
                        prev_dc,
                        &self.huffman_tables[component.dc_huffman_table as usize].0,
                        &self.huffman_tables[component.ac_huffman_table as usize].1,
                    )?;
                }

                prev_dc = block[0];

                if restart_interval > 0 {
                    if restarts_to_go == 0 {
                        restarts_to_go = restart_interval;
                        restarts += 1;
                        restarts &= 7;
                    }
                    restarts_to_go -= 1;
                }
            }

            self.writer.finalize_bit_buffer()?;
        }

        Ok(())
    }

    /// Encode image in progressive mode
    ///
    /// This only support spectral selection for now
    fn encode_image_progressive<I: ImageBuffer, OP: Operations>(
        &mut self,
        image: I,
        scans: u8,
        q_tables: &[QuantizationTable; 2],
    ) -> Result<(), EncodingError> {
        let (blocks, _grid) = self.encode_blocks::<_, OP>(&image, q_tables);

        if self.optimize_huffman_table {
            let _s = crate::prof::scope(crate::prof::Stage::HuffmanOptimize);
            self.optimize_huffman_table(&blocks);
        }

        self.write_frame_header(&image, q_tables)?;

        // Phase 1: DC Scan
        //          Only the DC coefficients can be transfer in the first component scans
        for (i, component) in self.components.iter().enumerate() {
            self.writer.write_scan_header(&[component], Some((0, 0)))?;

            let restart_interval = self.restart_interval.unwrap_or(0);
            let mut restarts = 0;
            let mut restarts_to_go = restart_interval;

            let mut prev_dc = 0;

            for block in &blocks[i] {
                if restart_interval > 0 && restarts_to_go == 0 {
                    self.writer.finalize_bit_buffer()?;
                    self.writer
                        .write_marker(Marker::RST((restarts % 8) as u8))?;

                    prev_dc = 0;
                }

                self.writer.write_dc(
                    block[0],
                    prev_dc,
                    &self.huffman_tables[component.dc_huffman_table as usize].0,
                )?;

                prev_dc = block[0];

                if restart_interval > 0 {
                    if restarts_to_go == 0 {
                        restarts_to_go = restart_interval;
                        restarts += 1;
                        restarts &= 7;
                    }
                    restarts_to_go -= 1;
                }
            }

            self.writer.finalize_bit_buffer()?;
        }

        // Phase 2: AC scans
        let scans = scans as usize - 1;

        let values_per_scan = 64 / scans;

        for scan in 0..scans {
            let start = (scan * values_per_scan).max(1);
            let end = if scan == scans - 1 {
                // ensure last scan is always transfers the remaining coefficients
                64
            } else {
                (scan + 1) * values_per_scan
            };

            for (i, component) in self.components.iter().enumerate() {
                let restart_interval = self.restart_interval.unwrap_or(0);
                let mut restarts = 0;
                let mut restarts_to_go = restart_interval;

                self.writer
                    .write_scan_header(&[component], Some((start as u8, end as u8 - 1)))?;

                for block in &blocks[i] {
                    if restart_interval > 0 && restarts_to_go == 0 {
                        self.writer.finalize_bit_buffer()?;
                        self.writer
                            .write_marker(Marker::RST((restarts % 8) as u8))?;
                    }

                    self.writer.write_ac_block(
                        block,
                        start,
                        end,
                        &self.huffman_tables[component.ac_huffman_table as usize].1,
                    )?;

                    if restart_interval > 0 {
                        if restarts_to_go == 0 {
                            restarts_to_go = restart_interval;
                            restarts += 1;
                            restarts &= 7;
                        }
                        restarts_to_go -= 1;
                    }
                }

                self.writer.finalize_bit_buffer()?;
            }
        }

        Ok(())
    }

    fn encode_blocks<I: ImageBuffer, OP: Operations>(
        &mut self,
        image: &I,
        q_tables: &[QuantizationTable; 2],
    ) -> MaterializedBlocks {
        let width = image.width();
        let height = image.height();
        let mut grid = [(1usize, 1usize); 4];

        let (max_h_sampling, max_v_sampling) = self.get_max_sampling_size();

        let num_cols = ceil_div(usize::from(width), 8 * max_h_sampling) * max_h_sampling;
        let num_rows = ceil_div(usize::from(height), 8 * max_v_sampling) * max_v_sampling;

        debug_assert!(num_cols > 0);
        debug_assert!(num_rows > 0);

        let buffer_width = num_cols * 8;
        let buffer_size = num_cols * num_rows * 64;

        let mut row: [Vec<_>; 4] = self.init_rows(buffer_size);

        for y in 0..num_rows * 8 {
            let y = (y.min(usize::from(height) - 1)) as u16;

            {
                let _s = crate::prof::scope(crate::prof::Stage::FillBuffers);
                image.fill_buffers(y, &mut row);
            }

            {
                let _rs = crate::prof::scope(crate::prof::Stage::RowSetup);
                for _ in usize::from(width)..num_cols * 8 {
                    for channel in &mut row {
                        if !channel.is_empty() {
                            channel.push(channel[channel.len() - 1]);
                        }
                    }
                }
            }
        }

        let num_cols = ceil_div(usize::from(width), 8);
        let num_rows = ceil_div(usize::from(height), 8);

        debug_assert!(num_cols > 0);
        debug_assert!(num_rows > 0);

        let mut blocks: [Vec<_>; 4] = self.init_block_buffers(buffer_size / 64);

        for (i, component) in self.components.iter().enumerate() {
            let h_scale = max_h_sampling / component.horizontal_sampling_factor as usize;
            let v_scale = max_v_sampling / component.vertical_sampling_factor as usize;

            let cols = ceil_div(num_cols, h_scale);
            let rows = ceil_div(num_rows, v_scale);
            grid[i] = (cols, rows);

            debug_assert!(cols > 0);
            debug_assert!(rows > 0);

            // Size the component's block list once, so the hot loop writes
            // through an index instead of pushing a 128-byte value per block
            // (1.36M copies, ~174 MB, for one 4K frame).
            let base = blocks[i].len();
            if !self.push_blocks {
                blocks[i].resize(base + rows * cols, [0i16; 64]);
            }
            for block_y in 0..rows {
                for block_x in 0..cols {
                    let _bb = crate::prof::scope(crate::prof::Stage::BlockBody);
                    let mut block = {
                        let _gb = crate::prof::scope(crate::prof::Stage::GetBlock);
                        get_block(
                            &row[i],
                            block_x * 8 * h_scale,
                            block_y * 8 * v_scale,
                            h_scale,
                            v_scale,
                            buffer_width,
                        )
                    };

                    {
                        let _s = crate::prof::scope(crate::prof::Stage::Fdct);
                        OP::fdct(&mut block);
                    }

                    let q_table = &q_tables[component.quantization_table as usize];
                    // Rate-distortion pass over the quantized block. It needs
                    // BOTH the pre-quantization coefficients and the quantized
                    // ones, so here is the only place it can run.
                    let ac = &self.huffman_tables[component.ac_huffman_table as usize].1;
                    if self.push_blocks {
                        let mut q_block = [0i16; 64];
                        {
                            let _s = crate::prof::scope(crate::prof::Stage::Quantize);
                            OP::quantize_block(&block, &mut q_block, q_table);
                        }
                        if self.trellis {
                            crate::encode::trellis::truncate_rd(&block, &mut q_block, q_table, ac);
                        }
                        blocks[i].push(q_block);
                    } else {
                        let slot = base + block_y * cols + block_x;
                        {
                            let _s = crate::prof::scope(crate::prof::Stage::Quantize);
                            OP::quantize_block(&block, &mut blocks[i][slot], q_table);
                        }
                        if self.trellis {
                            crate::encode::trellis::truncate_rd(
                                &block,
                                &mut blocks[i][slot],
                                q_table,
                                ac,
                            );
                        }
                    }
                }
            }
        }
        (blocks, grid)
    }

    fn init_block_buffers(&mut self, buffer_size: usize) -> [Vec<[i16; 64]>; 4] {
        // To simplify the code and to give the compiler more infos to optimize stuff we always initialize 4 components
        // Resource overhead should be minimal because an empty Vec doesn't allocate

        match self.components.len() {
            1 => [
                Vec::with_capacity(buffer_size),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ],
            3 => [
                Vec::with_capacity(buffer_size),
                Vec::with_capacity(buffer_size),
                Vec::with_capacity(buffer_size),
                Vec::new(),
            ],
            4 => [
                Vec::with_capacity(buffer_size),
                Vec::with_capacity(buffer_size),
                Vec::with_capacity(buffer_size),
                Vec::with_capacity(buffer_size),
            ],
            len => unreachable!("Unsupported component length: {}", len),
        }
    }

    /// Pick the optimize strategy for this image.
    ///
    /// Buffering the quantized blocks is **~1.23x faster** than the two-pass
    /// streaming form (41-pair ABBA at 4K: 5/41, z = -4.84, median 0.814),
    /// because streaming repeats the whole fill/extract/DCT/quantize chain and
    /// buffering only pays sequential memory traffic. So buffer by default.
    ///
    /// The catch is peak memory: the buffer is 128 bytes per 8x8 block, which is
    /// ~174 MB for 4K 4:2:0 and ~700 MB for 8K. Above
    /// [`OPTIMIZE_BUFFER_BUDGET`] the speed is not worth the footprint and the
    /// streaming path takes over.
    ///
    /// Note this trade *moved*: when `fill_buffers` was 17x more expensive,
    /// streaming and buffering measured even, because streaming paid that cost
    /// twice. Fixing `fill_buffers` is what made buffering the better arm.
    fn use_streaming_optimize<I: ImageBuffer>(&self, image: &I) -> bool {
        if let Some(forced) = self.streaming_optimize {
            return forced;
        }
        let (max_h, max_v) = self.get_max_sampling_size();
        let cols = ceil_div(usize::from(image.width()), 8 * max_h) * max_h;
        let rows = ceil_div(usize::from(image.height()), 8 * max_v) * max_v;
        let bytes = cols
            .saturating_mul(rows)
            .saturating_mul(self.components.len().max(1))
            .saturating_mul(core::mem::size_of::<[i16; 64]>());
        bytes > OPTIMIZE_BUFFER_BUDGET
    }

    /// Build optimized tables from frequencies gathered by a streaming pass.
    ///
    /// Same tables the block-materializing path produces, without the buffer:
    /// at 4K that path allocates ~218 MB of `[i16; 64]` just to count symbols it
    /// has already computed once.
    fn optimize_huffman_table_from_stats(&mut self, stats: &HuffmanStats) {
        let max_tables = self.components.len().min(2);
        for table in 0..max_tables {
            let mut dc_freq = stats.dc[table];
            let mut ac_freq = stats.ac[table];
            // Reserve the all-ones code, exactly as the materializing path does.
            dc_freq[256] = 1;
            ac_freq[256] = 1;
            self.huffman_tables[table] = (
                HuffmanTable::new_optimized(dc_freq),
                HuffmanTable::new_optimized(ac_freq),
            );
        }
    }

    // Create new huffman tables optimized for this image
    fn optimize_huffman_table(&mut self, blocks: &[Vec<[i16; 64]>; 4]) {
        // TODO: Find out if it's possible to reuse some code from the writer

        let max_tables = self.components.len().min(2) as u8;

        for table in 0..max_tables {
            let mut dc_freq = [0u32; 257];
            dc_freq[256] = 1;
            let mut ac_freq = [0u32; 257];
            ac_freq[256] = 1;

            let mut had_ac = false;
            let mut had_dc = false;

            for (i, component) in self.components.iter().enumerate() {
                if component.dc_huffman_table == table {
                    had_dc = true;

                    let mut prev_dc = 0;

                    debug_assert!(!blocks[i].is_empty());

                    for block in &blocks[i] {
                        let value = block[0];
                        let diff = value - prev_dc;
                        let num_bits = get_num_bits(diff);

                        dc_freq[num_bits as usize] += 1;

                        prev_dc = value;
                    }
                }

                if component.ac_huffman_table == table {
                    had_ac = true;

                    if let Some(scans) = self.progressive_scans {
                        let scans = scans as usize - 1;

                        let values_per_scan = 64 / scans;

                        for scan in 0..scans {
                            let start = (scan * values_per_scan).max(1);
                            let end = if scan == scans - 1 {
                                // Due to rounding we might need to transfer more than values_per_scan values in the last scan
                                64
                            } else {
                                (scan + 1) * values_per_scan
                            };

                            debug_assert!(!blocks[i].is_empty());

                            for block in &blocks[i] {
                                let mut zero_run = 0;

                                for &value in &block[start..end] {
                                    if value == 0 {
                                        zero_run += 1;
                                    } else {
                                        while zero_run > 15 {
                                            ac_freq[0xF0] += 1;
                                            zero_run -= 16;
                                        }
                                        let num_bits = get_num_bits(value);
                                        let symbol = (zero_run << 4) | num_bits;

                                        ac_freq[symbol as usize] += 1;

                                        zero_run = 0;
                                    }
                                }

                                if zero_run > 0 {
                                    ac_freq[0] += 1;
                                }
                            }
                        }
                    } else {
                        for block in &blocks[i] {
                            let mut zero_run = 0;

                            for &value in &block[1..] {
                                if value == 0 {
                                    zero_run += 1;
                                } else {
                                    while zero_run > 15 {
                                        ac_freq[0xF0] += 1;
                                        zero_run -= 16;
                                    }
                                    let num_bits = get_num_bits(value);
                                    let symbol = (zero_run << 4) | num_bits;

                                    ac_freq[symbol as usize] += 1;

                                    zero_run = 0;
                                }
                            }

                            if zero_run > 0 {
                                ac_freq[0] += 1;
                            }
                        }
                    }
                }
            }

            assert!(had_dc, "Missing DC data for table {}", table);
            assert!(had_ac, "Missing AC data for table {}", table);

            self.huffman_tables[table as usize] = (
                HuffmanTable::new_optimized(dc_freq),
                HuffmanTable::new_optimized(ac_freq),
            );
        }
    }
}

#[cfg(feature = "std")]
impl Encoder<BufWriter<File>> {
    /// Create a new decoder that writes into a file
    ///
    /// See [new](Encoder::new) for further information.
    ///
    /// # Errors
    ///
    /// Returns an `IoError(std::io::Error)` if the file can't be created
    pub fn new_file<P: AsRef<Path>>(
        path: P,
        quality: u8,
    ) -> Result<Encoder<BufWriter<File>>, EncodingError> {
        let file = File::create(path)?;
        let buf = BufWriter::new(file);
        Ok(Self::new(buf, quality))
    }
}

/// Huffman symbol frequencies for the (at most two) table slots a baseline
/// JPEG uses. Index 256 is reserved by the table builder.
struct HuffmanStats {
    dc: [[u32; 257]; 2],
    ac: [[u32; 257]; 2],
}

impl Default for HuffmanStats {
    fn default() -> Self {
        HuffmanStats {
            dc: [[0; 257]; 2],
            ac: [[0; 257]; 2],
        }
    }
}

/// Peak block-buffer size above which optimized-Huffman encoding switches from
/// the faster buffering path to the slower streaming one. 4K 4:2:0 sits under
/// this; 8K does not.
const OPTIMIZE_BUFFER_BUDGET: usize = 256 * 1024 * 1024;

/// Extract an 8x8 block with NO subsampling: 8 contiguous bytes per row, widened
/// to i16 and level-shifted by -128.
///
/// This is the luma path, and on 4:2:0 it is **two thirds of all blocks**. The
/// scalar form pays a multiply, an index computation and a bounds check per
/// sample; here each row is one 8-byte load, one widen, one subtract, one store.
///
/// # Safety
/// Caller guarantees `start_y + 8` rows and `start_x + 8` columns lie inside
/// `data`, and that SSE4.1 is available.
#[cfg(all(feature = "simd", any(target_arch = "x86", target_arch = "x86_64")))]
#[target_feature(enable = "sse4.1")]
unsafe fn get_block_1x1_simd(
    data: &[u8],
    start_x: usize,
    start_y: usize,
    width: usize,
) -> [i16; 64] {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;

    let mut block = [0i16; 64];
    let bias = _mm_set1_epi16(128);
    for y in 0..8 {
        let src = data.as_ptr().add((start_y + y) * width + start_x);
        let v = _mm_loadl_epi64(src as *const __m128i);
        let widened = _mm_cvtepu8_epi16(v);
        let shifted = _mm_sub_epi16(widened, bias);
        _mm_storeu_si128(block.as_mut_ptr().add(y * 8) as *mut __m128i, shifted);
    }
    block
}

/// Extract an 8x8 block with 2x2 box averaging — the 4:2:0 chroma path, a third
/// of all blocks.
///
/// `maddubs` does the horizontal pairwise sum of 16 samples in one instruction,
/// which is exactly the inner two adds of the box filter; the two source rows
/// then add vertically. Rounding is `(sum + 2) >> 2`, matching the scalar form's
/// `(sum + half) / n` for `n == 4` exactly.
///
/// # Safety
/// Caller guarantees `start_y + 16` rows and `start_x + 16` columns lie inside
/// `data`, and that SSSE3 is available.
#[cfg(all(feature = "simd", any(target_arch = "x86", target_arch = "x86_64")))]
#[target_feature(enable = "ssse3")]
unsafe fn get_block_2x2_simd(
    data: &[u8],
    start_x: usize,
    start_y: usize,
    width: usize,
) -> [i16; 64] {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;

    let mut block = [0i16; 64];
    let ones = _mm_set1_epi8(1);
    let round = _mm_set1_epi16(2);
    let bias = _mm_set1_epi16(128);
    for y in 0..8 {
        let r0 = data.as_ptr().add((start_y + 2 * y) * width + start_x);
        let r1 = r0.add(width);
        // Pairwise horizontal sums: 16 u8 -> 8 i16.
        let a = _mm_maddubs_epi16(_mm_loadu_si128(r0 as *const __m128i), ones);
        let b = _mm_maddubs_epi16(_mm_loadu_si128(r1 as *const __m128i), ones);
        let sum = _mm_add_epi16(a, b);
        let avg = _mm_srai_epi16::<2>(_mm_add_epi16(sum, round));
        _mm_storeu_si128(
            block.as_mut_ptr().add(y * 8) as *mut __m128i,
            _mm_sub_epi16(avg, bias),
        );
    }
    block
}

/// Extract one 8x8 block, box-averaging when the component is subsampled.
///
/// `col_stride`/`row_stride` are the subsampling ratios — 2 and 2 for 4:2:0
/// chroma. This used to take only the top-left sample of each box and DISCARD
/// the other three, which is not downsampling but decimation: it aliases every
/// chroma frequency above the subsampled Nyquist straight back into the
/// baseband, and no amount of bitrate can undo it afterwards.
///
/// The tell, on content built from saturated chroma edges: PSNR sat at ~14.1 dB
/// FLAT from quality 50 to 95 while the file grew from 45 KB to 83 KB. Error
/// that does not respond to bitrate is not quantization error.
///
/// libjpeg, mozjpeg and ffmpeg all box-average here. The average is rounded
/// half-up, matching libjpeg's `h2v2_downsample`.
fn get_block(
    data: &[u8],
    start_x: usize,
    start_y: usize,
    col_stride: usize,
    row_stride: usize,
    width: usize,
) -> [i16; 64] {
    let mut block = [0i16; 64];

    // SIMD interior paths. Both require the whole sampling window inside the
    // buffer, which `encode_blocks` guarantees by padding rows to MCU bounds —
    // `getblock_EDGE` counts 0 on every geometry tried. `RUSTY_JPEG_ARM=slowblock`
    // forces the scalar oracle.
    #[cfg(all(feature = "simd", any(target_arch = "x86", target_arch = "x86_64")))]
    {
        {
            let h = data.len() / width;
            if col_stride == 1
                && row_stride == 1
                && start_x + 8 <= width
                && start_y + 8 <= h
                && std::is_x86_feature_detected!("sse4.1")
            {
                // SAFETY: bounds proven above; SSE4.1 checked at runtime.
                #[allow(unsafe_code)]
                unsafe {
                    return get_block_1x1_simd(data, start_x, start_y, width);
                }
            }
            if col_stride == 2
                && row_stride == 2
                && start_x + 16 <= width
                && start_y + 16 <= h
                && std::is_x86_feature_detected!("ssse3")
            {
                // SAFETY: bounds proven above; SSSE3 checked at runtime.
                #[allow(unsafe_code)]
                unsafe {
                    return get_block_2x2_simd(data, start_x, start_y, width);
                }
            }
        }
    }

    // Fast path: no subsampling, so there is nothing to average.
    if col_stride == 1 && row_stride == 1 {
        for y in 0..8 {
            for x in 0..8 {
                let ix = start_x + (x * col_stride);
                let iy = start_y + (y * row_stride);
                block[y * 8 + x] = (data[iy * width + ix] as i16) - 128;
            }
        }
        return block;
    }

    let n = (col_stride * row_stride) as u32;
    let half = n / 2;
    let height = data.len() / width;

    // Interior fast path.
    //
    // The clamps below are dead on EVERY block, not merely on interior ones:
    // `encode_blocks` pads its row buffer to MCU boundaries, so a block's
    // sampling window always fits. Counted on ragged geometries that ought to be
    // the worst case — 127x65, 320x241, 1920x1080 — the clamped path is taken
    // **0** times.
    //
    // Dead does not mean free. The clamps are what stop the compiler proving the
    // index, so each of the 256 samples of a 4:2:0 chroma block paid a `min` AND
    // a bounds check it could never need. Proving the window fits, once per
    // block, replaces 256 bounds checks with two slice checks per row.
    //
    // The clamped path stays as the oracle and as cover should that padding
    // invariant ever change.
    //
    // `RUSTY_JPEG_ARM=slowblock` forces the general path, so the two can be A/B'd
    // in one binary, and the general path stays as the oracle.
    if start_x + 8 * col_stride <= width && start_y + 8 * row_stride <= height {
        if col_stride == 2 && row_stride == 2 {
            // 4:2:0 — the dominant case, worth its own straight-line body.
            for y in 0..8 {
                let iy = start_y + 2 * y;
                let a = iy * width + start_x;
                let b = a + width;
                let r0 = &data[a..a + 16];
                let r1 = &data[b..b + 16];
                for x in 0..8 {
                    let sum = r0[2 * x] as u32
                        + r0[2 * x + 1] as u32
                        + r1[2 * x] as u32
                        + r1[2 * x + 1] as u32;
                    block[y * 8 + x] = ((sum + 2) / 4) as i16 - 128;
                }
            }
            return block;
        }
        for y in 0..8 {
            let iy = start_y + y * row_stride;
            for x in 0..8 {
                let ix = start_x + x * col_stride;
                let mut sum = 0u32;
                for dy in 0..row_stride {
                    let base = (iy + dy) * width + ix;
                    let row = &data[base..base + col_stride];
                    for &v in row {
                        sum += v as u32;
                    }
                }
                block[y * 8 + x] = ((sum + half) / n) as i16 - 128;
            }
        }
        return block;
    }

    for y in 0..8 {
        for x in 0..8 {
            let ix = start_x + (x * col_stride);
            let iy = start_y + (y * row_stride);
            let mut sum = 0u32;
            for dy in 0..row_stride {
                // The row buffer is padded to block boundaries, but clamp
                // anyway so a ragged edge cannot read out of the plane.
                let sy = (iy + dy).min(height - 1);
                let base = sy * width;
                for dx in 0..col_stride {
                    let sx = (ix + dx).min(width - 1);
                    sum += data[base + sx] as u32;
                }
            }
            block[y * 8 + x] = ((sum + half) / n) as i16 - 128;
        }
    }

    block
}

fn ceil_div(value: usize, div: usize) -> usize {
    value / div + usize::from(value % div != 0)
}

fn get_num_bits(mut value: i16) -> u8 {
    if value < 0 {
        value = -value;
    }

    let mut num_bits = 0;

    while value > 0 {
        num_bits += 1;
        value >>= 1;
    }

    num_bits
}

pub(crate) trait Operations {
    #[inline(always)]
    fn fdct(data: &mut [i16; 64]) {
        fdct(data);
    }

    #[inline(always)]
    fn quantize_block(block: &[i16; 64], q_block: &mut [i16; 64], table: &QuantizationTable) {
        crate::encode::quantization::quantize_block_scalar(block, q_block, table)
    }
}

pub(crate) struct DefaultOperations;

impl Operations for DefaultOperations {}

/// Measurement-only kernel set holding the **pre-brick** quantizer: permuted
/// table lookups (`reciprocals[ZIGZAG[i]]`) rather than the pre-permuted ones.
///
/// Exists so the shipped quantizer can be A/B'd against its predecessor inside a
/// single process with the arms interleaved — sequential runs on this box drift
/// far more than the effect being measured (one profiler run read 2.76x another
/// for identical work). Output is bit-identical; `quantize_zz_matches_permuted_lookup`
/// proves it exhaustively.
pub(crate) struct BranchyQuantizeOperations;

impl Operations for BranchyQuantizeOperations {
    /// Delegate to the same FDCT the shipped path uses.
    ///
    /// Without this, the branchy arm would inherit the trait's *scalar* FDCT
    /// while the branchless arm ran the AVX2 one, and the A/B would be
    /// comparing two changes at once — the classic "do both arms do identical
    /// work?" failure. The `avx2` selection below mirrors `encode_image`'s, and
    /// is only reached after the same runtime feature check.
    #[inline(always)]
    fn fdct(data: &mut [i16; 64]) {
        #[cfg(all(feature = "simd", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            if std::is_x86_feature_detected!("avx2") {
                return <crate::encode::avx2::AVX2Operations as Operations>::fdct(data);
            }
        }
        fdct(data)
    }

    fn quantize_block(block: &[i16; 64], q_block: &mut [i16; 64], table: &QuantizationTable) {
        for i in 0..64 {
            let z = ZIGZAG[i] as usize & 0x3f;
            q_block[i] = table.quantize(block[z], z);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use crate::encode::encoder::get_num_bits;
    use crate::encode::writer::get_code;
    use crate::encode::{Encoder, SamplingFactor};

    #[test]
    fn test_get_num_bits() {
        let min_max = 2i16.pow(13);

        for value in -min_max..=min_max {
            let num_bits1 = get_num_bits(value);
            let (num_bits2, _) = get_code(value);

            assert_eq!(
                num_bits1, num_bits2,
                "Difference in num bits for value {}: {} vs {}",
                value, num_bits1, num_bits2
            );
        }
    }

    #[test]
    fn sampling_factors() {
        assert_eq!(SamplingFactor::F_1_1.get_sampling_factors(), (1, 1));
        assert_eq!(SamplingFactor::F_2_1.get_sampling_factors(), (2, 1));
        assert_eq!(SamplingFactor::F_1_2.get_sampling_factors(), (1, 2));
        assert_eq!(SamplingFactor::F_2_2.get_sampling_factors(), (2, 2));
        assert_eq!(SamplingFactor::F_4_1.get_sampling_factors(), (4, 1));
        assert_eq!(SamplingFactor::F_4_2.get_sampling_factors(), (4, 2));
        assert_eq!(SamplingFactor::F_1_4.get_sampling_factors(), (1, 4));
        assert_eq!(SamplingFactor::F_2_4.get_sampling_factors(), (2, 4));

        assert_eq!(SamplingFactor::R_4_4_4.get_sampling_factors(), (1, 1));
        assert_eq!(SamplingFactor::R_4_4_0.get_sampling_factors(), (1, 2));
        assert_eq!(SamplingFactor::R_4_4_1.get_sampling_factors(), (1, 4));
        assert_eq!(SamplingFactor::R_4_2_2.get_sampling_factors(), (2, 1));
        assert_eq!(SamplingFactor::R_4_2_0.get_sampling_factors(), (2, 2));
        assert_eq!(SamplingFactor::R_4_2_1.get_sampling_factors(), (2, 4));
        assert_eq!(SamplingFactor::R_4_1_1.get_sampling_factors(), (4, 1));
        assert_eq!(SamplingFactor::R_4_1_0.get_sampling_factors(), (4, 2));
    }

    #[test]
    fn test_set_progressive() {
        let mut encoder = Encoder::new(vec![], 100);
        encoder.set_progressive(true);
        assert_eq!(encoder.progressive_scans(), Some(4));

        encoder.set_progressive(false);
        assert_eq!(encoder.progressive_scans(), None);
    }
}
