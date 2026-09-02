use crate::decode::error::{Error, Result, UnsupportedFeature};
use crate::decode::huffman::{fill_default_mjpeg_tables, HuffmanDecoder, HuffmanTable};
use crate::decode::marker::Marker;
use crate::decode::parser::{
    parse_app, parse_com, parse_dht, parse_dqt, parse_dri, parse_sof, parse_sos,
    AdobeColorTransform, AppData, CodingProcess, Component, Dimensions, EntropyCoding, FrameInfo,
    IccChunk, ScanInfo,
};
use crate::decode::read_u8;
use crate::decode::upsampler::Upsampler;
use crate::decode::worker::{
    compute_image_parallel, PreferWorkerKind, RowData, Worker, WorkerScope,
};
use crate::decode::Source;
use alloc::borrow::ToOwned;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::{format, vec};
use core::cmp;
use core::mem;
use core::ops::Range;

pub const MAX_COMPONENTS: usize = 4;

mod lossless;
use self::lossless::compute_image_lossless;

#[rustfmt::skip]
static UNZIGZAG: [u8; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

/// An enumeration over combinations of color spaces and bit depths a pixel can have.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PixelFormat {
    /// Luminance (grayscale), 8 bits
    L8,
    /// Luminance (grayscale), 16 bits
    L16,
    /// RGB, 8 bits per channel
    RGB24,
    /// CMYK, 8 bits per channel
    CMYK32,
}

impl PixelFormat {
    /// Determine the size in bytes of each pixel in this format
    pub fn pixel_bytes(&self) -> usize {
        match self {
            PixelFormat::L8 => 1,
            PixelFormat::L16 => 2,
            PixelFormat::RGB24 => 3,
            PixelFormat::CMYK32 => 4,
        }
    }
}

/// Represents metadata of an image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageInfo {
    /// The width of the image, in pixels.
    pub width: u16,
    /// The height of the image, in pixels.
    pub height: u16,
    /// The pixel format of the image.
    pub pixel_format: PixelFormat,
    /// The coding process of the image.
    pub coding_process: CodingProcess,
}

/// Describes the colour transform to apply before binary data is returned
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColorTransform {
    /// No transform should be applied and the data is returned as-is.
    None,
    /// Unknown colour transformation
    Unknown,
    /// Grayscale transform should be applied (expects 1 channel)
    Grayscale,
    /// RGB transform should be applied.
    RGB,
    /// YCbCr transform should be applied.
    YCbCr,
    /// CMYK transform should be applied.
    CMYK,
    /// YCCK transform should be applied.
    YCCK,
    /// big gamut Y/Cb/Cr, bg-sYCC
    JcsBgYcc,
    /// big gamut red/green/blue, bg-sRGB
    JcsBgRgb,
}

/// JPEG decoder
pub struct Decoder<R> {
    reader: crate::decode::Buffered<R>,

    frame: Option<FrameInfo>,
    dc_huffman_tables: Vec<Option<HuffmanTable>>,
    ac_huffman_tables: Vec<Option<HuffmanTable>>,
    quantization_tables: [Option<Arc<[u16; 64]>>; 4],

    restart_interval: u16,

    adobe_color_transform: Option<AdobeColorTransform>,
    color_transform: Option<ColorTransform>,

    is_jfif: bool,
    is_mjpeg: bool,

    icc_markers: Vec<IccChunk>,

    exif_data: Option<Vec<u8>>,
    xmp_data: Option<Vec<u8>>,
    psir_data: Option<Vec<u8>>,

    // Used for progressive JPEGs.
    coefficients: Vec<Vec<i16>>,
    // Bitmask of which coefficients has been completely decoded.
    coefficients_finished: [u64; MAX_COMPONENTS],

    // Maximum allowed size of decoded image buffer
    decoding_buffer_size_limit: usize,

    // Planar output (see `decode_planar`): a request flag and the place the
    // decode leaves its result, since the decode path itself returns Vec<u8>.
    planar_output: bool,
    planar_result: Option<PlanarImage>,

    /// Decode on the calling thread only. See [`set_single_threaded`].
    single_threaded: bool,

    /// Output-plane allocations handed back by a previous decode.
    plane_pool: Vec<Vec<u8>>,
}

/// One decoded component plane, at its own (possibly subsampled) resolution.
///
/// `data` is block-aligned, so rows are `stride` bytes apart while only the
/// first `width` of each row are image samples. Reading it as
/// `data[y * stride .. y * stride + width]` avoids a repack copy.
#[derive(Debug, Clone)]
pub struct PlanarComponent {
    pub data: Vec<u8>,
    /// Distance between row starts, in bytes. `>= width`.
    pub stride: usize,
    pub width: usize,
    pub height: usize,
    /// This component's sampling factors as coded in the JPEG frame header.
    pub horizontal_sampling_factor: u8,
    pub vertical_sampling_factor: u8,
}

/// A decoded image as separate component planes — the output of
/// [`Decoder::decode_planar`].
#[derive(Debug, Clone)]
pub struct PlanarImage {
    /// One per component: 1 = grayscale, 3 = Y'CbCr, 4 = CMYK/YCCK.
    pub components: Vec<PlanarComponent>,
    pub width: u16,
    pub height: u16,
}

impl PlanarImage {
    fn from_components(
        components: &[Component],
        planes: Vec<Vec<u8>>,
        output_size: Dimensions,
    ) -> Result<PlanarImage> {
        if components.len() != planes.len() {
            return Err(Error::Format("component/plane count mismatch".to_owned()));
        }
        let out = components
            .iter()
            .zip(planes)
            .map(|(c, data)| PlanarComponent {
                // Same stride the interleaved path computes before it repacks.
                stride: usize::from(c.block_size.width) * c.dct_scale,
                width: usize::from(c.size.width),
                height: usize::from(c.size.height),
                horizontal_sampling_factor: c.horizontal_sampling_factor,
                vertical_sampling_factor: c.vertical_sampling_factor,
                data,
            })
            .collect::<Vec<_>>();

        for c in &out {
            // A short final row is fine (the last row needs only `width`), but
            // anything less than that is a malformed plane, not a crop.
            let need = c.stride * c.height.saturating_sub(1) + c.width;
            if c.height == 0 || c.width == 0 || c.data.len() < need {
                return Err(Error::Format(
                    "component plane smaller than its geometry".to_owned(),
                ));
            }
        }

        Ok(PlanarImage {
            components: out,
            width: output_size.width,
            height: output_size.height,
        })
    }

    /// Take the plane allocations back out, to feed
    /// [`Decoder::recycle_planes`]. Consumes the image.
    pub fn into_planes(self) -> Vec<Vec<u8>> {
        self.components.into_iter().map(|c| c.data).collect()
    }

    /// Chroma subsampling as `(horizontal, vertical)` luma samples per chroma
    /// sample — `(1,1)` 4:4:4, `(2,1)` 4:2:2, `(2,2)` 4:2:0 — or `None` when the
    /// image is not 3-component Y'CbCr.
    pub fn chroma_subsampling(&self) -> Option<(usize, usize)> {
        if self.components.len() != 3 {
            return None;
        }
        let maxh = self
            .components
            .iter()
            .map(|c| c.horizontal_sampling_factor)
            .max()?;
        let maxv = self
            .components
            .iter()
            .map(|c| c.vertical_sampling_factor)
            .max()?;
        let (cb, cr) = (&self.components[1], &self.components[2]);
        // Both chroma planes must agree, else it is not a layout we can name.
        if cb.horizontal_sampling_factor != cr.horizontal_sampling_factor
            || cb.vertical_sampling_factor != cr.vertical_sampling_factor
        {
            return None;
        }
        Some((
            usize::from(maxh / cb.horizontal_sampling_factor.max(1)),
            usize::from(maxv / cb.vertical_sampling_factor.max(1)),
        ))
    }
}

impl<R: Source> Decoder<R> {
    /// Creates a new `Decoder` using the reader `reader`.
    pub fn new(reader: R) -> Decoder<R> {
        Decoder {
            reader: crate::decode::Buffered::new(reader),
            frame: None,
            dc_huffman_tables: vec![None, None, None, None],
            ac_huffman_tables: vec![None, None, None, None],
            quantization_tables: [None, None, None, None],
            restart_interval: 0,
            adobe_color_transform: None,
            color_transform: None,
            is_jfif: false,
            is_mjpeg: false,
            icc_markers: Vec::new(),
            exif_data: None,
            xmp_data: None,
            psir_data: None,
            coefficients: Vec::new(),
            coefficients_finished: [0; MAX_COMPONENTS],
            decoding_buffer_size_limit: usize::MAX,
            planar_output: false,
            planar_result: None,
            single_threaded: false,
            plane_pool: Vec::new(),
        }
    }

    /// Colour transform to use when decoding the image. App segments relating to colour transforms
    /// will be ignored.
    pub fn set_color_transform(&mut self, transform: ColorTransform) {
        self.color_transform = Some(transform);
    }

    /// Set maximum buffer size allowed for decoded images
    pub fn set_max_decoding_buffer_size(&mut self, max: usize) {
        self.decoding_buffer_size_limit = max;
    }

    /// Returns metadata about the image.
    ///
    /// The returned value will be `None` until a call to either `read_info` or `decode` has
    /// returned `Ok`.
    pub fn info(&self) -> Option<ImageInfo> {
        match self.frame {
            Some(ref frame) => {
                let pixel_format = match frame.components.len() {
                    1 => match frame.precision {
                        2..=8 => PixelFormat::L8,
                        9..=16 => PixelFormat::L16,
                        _ => panic!(),
                    },
                    3 => PixelFormat::RGB24,
                    4 => PixelFormat::CMYK32,
                    _ => panic!(),
                };

                Some(ImageInfo {
                    width: frame.output_size.width,
                    height: frame.output_size.height,
                    pixel_format,
                    coding_process: frame.coding_process,
                })
            }
            None => None,
        }
    }

    /// Returns raw exif data, starting at the TIFF header, if the image contains any.
    ///
    /// The returned value will be `None` until a call to `decode` has returned `Ok`.    
    pub fn exif_data(&self) -> Option<&[u8]> {
        self.exif_data.as_deref()
    }

    /// Returns the raw XMP packet if there is any.
    ///
    /// The returned value will be `None` until a call to `decode` has returned `Ok`.
    pub fn xmp_data(&self) -> Option<&[u8]> {
        self.xmp_data.as_deref()
    }

    /// Returns the embeded icc profile if the image contains one.
    pub fn icc_profile(&self) -> Option<Vec<u8>> {
        let mut marker_present: [Option<&IccChunk>; 256] = [None; 256];
        let num_markers = self.icc_markers.len();
        if num_markers == 0 || num_markers >= 255 {
            return None;
        }
        // check the validity of the markers
        for chunk in &self.icc_markers {
            if usize::from(chunk.num_markers) != num_markers {
                // all the lengths must match
                return None;
            }
            if chunk.seq_no == 0 {
                return None;
            }
            if marker_present[usize::from(chunk.seq_no)].is_some() {
                // duplicate seq_no
                return None;
            } else {
                marker_present[usize::from(chunk.seq_no)] = Some(chunk);
            }
        }

        // assemble them together by seq_no failing if any are missing
        let mut data = Vec::new();
        // seq_no's start at 1
        for &chunk in marker_present.get(1..=num_markers)? {
            data.extend_from_slice(&chunk?.data);
        }
        Some(data)
    }

    /// Heuristic to avoid starting thread, synchronization if we expect a small amount of
    /// parallelism to be utilized.
    fn select_worker(frame: &FrameInfo, worker_preference: PreferWorkerKind) -> PreferWorkerKind {
        const PARALLELISM_THRESHOLD: u64 = 128 * 128;

        match worker_preference {
            PreferWorkerKind::Immediate => PreferWorkerKind::Immediate,
            PreferWorkerKind::Multithreaded => {
                let width: u64 = frame.output_size.width.into();
                let height: u64 = frame.output_size.width.into();
                if width * height > PARALLELISM_THRESHOLD {
                    PreferWorkerKind::Multithreaded
                } else {
                    PreferWorkerKind::Immediate
                }
            }
        }
    }

    /// Tries to read metadata from the image without decoding it.
    ///
    /// If successful, the metadata can be obtained using the `info` method.
    pub fn read_info(&mut self) -> Result<()> {
        WorkerScope::with(|worker| self.decode_internal(true, worker)).map(|_| ())
    }

    /// Configure the decoder to scale the image during decoding.
    ///
    /// This efficiently scales the image by the smallest supported scale
    /// factor that produces an image larger than or equal to the requested
    /// size in at least one axis. The currently implemented scale factors
    /// are 1/8, 1/4, 1/2 and 1.
    ///
    /// To generate a thumbnail of an exact size, pass the desired size and
    /// then scale to the final size using a traditional resampling algorithm.
    pub fn scale(&mut self, requested_width: u16, requested_height: u16) -> Result<(u16, u16)> {
        self.read_info()?;
        let frame = self.frame.as_mut().unwrap();
        let idct_size = crate::decode::idct::choose_idct_size(
            frame.image_size,
            Dimensions {
                width: requested_width,
                height: requested_height,
            },
        );
        frame.update_idct_size(idct_size)?;
        Ok((frame.output_size.width, frame.output_size.height))
    }

    /// Decodes the image and returns the decoded pixels if successful.
    pub fn decode(&mut self) -> Result<Vec<u8>> {
        WorkerScope::with(|worker| self.decode_internal(false, worker))
    }

    /// Hand plane allocations from a previous decode back for reuse.
    ///
    /// Sizing fresh output planes is **66.7% of whole-frame decode** here — 3.1
    /// MB per 1080p frame, dominated by first-touch page faults rather than the
    /// zeroing (writing the same pages a second time costs 1.2%). Recycling the
    /// allocation skips the faults entirely; it is the same trick FFmpeg plays
    /// with `AVBufferPool`, and it is why a decoder that is constructed per
    /// frame otherwise pays this on every frame.
    ///
    /// Buffers are refilled, not trusted: every byte is overwritten before it is
    /// read, so recycling cannot leak one image into the next.
    ///
    /// ```no_run
    /// # use rusty_jpeg::decode::Decoder;
    /// # fn f(frames: Vec<Vec<u8>>) -> Result<(), rusty_jpeg::decode::Error> {
    /// let mut pool = Vec::new();
    /// for bytes in frames {
    ///     let mut d = Decoder::new(&bytes[..]);
    ///     d.recycle_planes(core::mem::take(&mut pool));
    ///     let image = d.decode_planar()?;
    ///     // ... use `image` ...
    ///     pool = image.into_planes();
    /// }
    /// # Ok(()) }
    /// ```
    pub fn recycle_planes(&mut self, planes: Vec<Vec<u8>>) {
        self.plane_pool = planes;
    }

    /// Decode entirely on the calling thread.
    ///
    /// By default any image larger than 128x128 is handed to a worker thread and
    /// its rows shipped over an mpsc channel. That is a poor trade for a single
    /// frame: the handoff is per row-batch, and when the process is confined to
    /// one core it is pure overhead. Measured on 1080p 4:2:0, pinned to one core
    /// (CPU time, 300 decodes), the threaded path costs more than it earns.
    ///
    /// Set this when decoding many images concurrently — where parallelism
    /// belongs at the image level, not inside one image — or when benchmarking
    /// against a single-threaded reference.
    pub fn set_single_threaded(&mut self, single: bool) {
        self.single_threaded = single;
    }

    /// Decode to **planar** component planes, at each component's own
    /// resolution — no chroma upsampling, no colour conversion.
    ///
    /// [`decode`](Self::decode) always returns interleaved RGB, which for a
    /// 4:2:0 image means upsampling chroma to full resolution and converting
    /// colour. A video pipeline then immediately undoes both. This returns the
    /// planes as the entropy decoder produced them, which is what such a
    /// pipeline actually wants.
    ///
    /// Not supported for lossless JPEG (which has its own 16-bit path).
    pub fn decode_planar(&mut self) -> Result<PlanarImage> {
        let _t = crate::prof::scope(crate::prof::Stage::Total);
        self.planar_output = true;
        let result = WorkerScope::with(|worker| self.decode_internal(false, worker));
        self.planar_output = false;
        result?;
        self.planar_result.take().ok_or(Error::Unsupported(
            UnsupportedFeature::SubsamplingRatio, // lossless took the other branch
        ))
    }

    fn decode_internal(
        &mut self,
        stop_after_metadata: bool,
        worker_scope: &WorkerScope,
    ) -> Result<Vec<u8>> {
        if stop_after_metadata && self.frame.is_some() {
            // The metadata has already been read.
            return Ok(Vec::new());
        } else if self.frame.is_none()
            && (read_u8(&mut self.reader)? != 0xFF
                || Marker::from_u8(read_u8(&mut self.reader)?) != Some(Marker::SOI))
        {
            return Err(Error::Format(
                "first two bytes are not an SOI marker".to_owned(),
            ));
        }

        let mut previous_marker = Marker::SOI;
        let mut pending_marker = None;
        let mut scans_processed = 0;
        let mut planes = vec![
            Vec::<u8>::new();
            self.frame
                .as_ref()
                .map_or(0, |frame| frame.components.len())
        ];
        let mut planes_u16 = vec![
            Vec::<u16>::new();
            self.frame
                .as_ref()
                .map_or(0, |frame| frame.components.len())
        ];

        loop {
            let marker = match pending_marker.take() {
                Some(m) => m,
                None => self.read_marker()?,
            };

            match marker {
                // Frame header
                Marker::SOF(..) => {
                    // Section 4.10
                    // "An image contains only one frame in the cases of sequential and
                    //  progressive coding processes; an image contains multiple frames for the
                    //  hierarchical mode."
                    if self.frame.is_some() {
                        return Err(Error::Unsupported(UnsupportedFeature::Hierarchical));
                    }

                    let frame = parse_sof(&mut self.reader, marker)?;
                    let component_count = frame.components.len();

                    if frame.is_differential {
                        return Err(Error::Unsupported(UnsupportedFeature::Hierarchical));
                    }
                    if frame.entropy_coding == EntropyCoding::Arithmetic {
                        return Err(Error::Unsupported(
                            UnsupportedFeature::ArithmeticEntropyCoding,
                        ));
                    }
                    if frame.precision != 8 && frame.coding_process != CodingProcess::Lossless {
                        return Err(Error::Unsupported(UnsupportedFeature::SamplePrecision(
                            frame.precision,
                        )));
                    }
                    if !(2..=16).contains(&frame.precision) {
                        return Err(Error::Unsupported(UnsupportedFeature::SamplePrecision(
                            frame.precision,
                        )));
                    }
                    if component_count != 1 && component_count != 3 && component_count != 4 {
                        return Err(Error::Unsupported(UnsupportedFeature::ComponentCount(
                            component_count as u8,
                        )));
                    }

                    // Make sure we support the subsampling ratios used.
                    let _ = Upsampler::new(
                        &frame.components,
                        frame.image_size.width,
                        frame.image_size.height,
                    )?;

                    self.frame = Some(frame);

                    if stop_after_metadata {
                        return Ok(Vec::new());
                    }

                    planes = vec![Vec::new(); component_count];
                    planes_u16 = vec![Vec::new(); component_count];
                }

                // Scan header
                Marker::SOS => {
                    if self.frame.is_none() {
                        return Err(Error::Format("scan encountered before frame".to_owned()));
                    }

                    let frame = self.frame.clone().unwrap();
                    let scan = parse_sos(&mut self.reader, &frame)?;

                    // Validate the scan's Huffman table references ONCE, here,
                    // rather than discovering a missing table three levels down
                    // in the middle of an MCU.
                    //
                    // Which tables a scan needs depends on what it codes, and
                    // progressive scans legitimately declare only one kind: a
                    // DC-only scan (Ss=0, Se=0) needs no AC table, and a DC
                    // REFINEMENT scan reads raw bits and needs no table at all.
                    // Demanding both unconditionally is what broke progressive
                    // files; demanding neither is what let a panic reach the
                    // block loop. The checks below are the exact conditions
                    // under which `decode_block` will actually use each table.
                    //
                    // This is not progressive-specific: any scan, baseline
                    // included, can name a table slot no DHT ever defined.
                    // Progressive merely makes it common.
                    {
                        let needs_dc = scan.spectral_selection.start == 0
                            && scan.successive_approximation_high == 0;
                        let needs_ac = scan.spectral_selection.end > 1;
                        for i in 0..scan.component_indices.len() {
                            if needs_dc
                                && self.dc_huffman_tables[scan.dc_table_indices[i]].is_none()
                            {
                                return Err(Error::Format(format!(
                                    "scan references DC Huffman table {} which no DHT defines",
                                    scan.dc_table_indices[i]
                                )));
                            }
                            if needs_ac
                                && self.ac_huffman_tables[scan.ac_table_indices[i]].is_none()
                            {
                                return Err(Error::Format(format!(
                                    "scan references AC Huffman table {} which no DHT defines",
                                    scan.ac_table_indices[i]
                                )));
                            }
                        }
                    }

                    if frame.coding_process == CodingProcess::DctProgressive
                        && self.coefficients.is_empty()
                    {
                        // Progressive keeps every coefficient of the whole image
                        // resident, because later scans revisit the same blocks.
                        // That buffer is sized straight from the frame header,
                        // so a malformed SOF sizes it directly: a fuzzer reached
                        // `malloc(8589934592)` — 8 GiB — from a 906-byte input.
                        //
                        // `decoding_buffer_size_limit` existed but was only
                        // enforced in `decode_planes`, which runs after every
                        // scan has been decoded — far too late to prevent the
                        // allocation it is meant to bound. Check it here, with
                        // checked arithmetic so the product cannot wrap.
                        let mut total = 0usize;
                        for c in &frame.components {
                            let n = (c.block_size.width as usize)
                                .checked_mul(c.block_size.height as usize)
                                .and_then(|b| b.checked_mul(64))
                                .ok_or_else(|| {
                                    Error::Format(
                                        "progressive coefficient buffer size overflows".to_owned(),
                                    )
                                })?;
                            total = total.checked_add(n).ok_or_else(|| {
                                Error::Format(
                                    "progressive coefficient buffer size overflows".to_owned(),
                                )
                            })?;
                        }
                        if total > self.decoding_buffer_size_limit {
                            return Err(Error::Format(
                                "progressive coefficient buffer exceeds maximum allowed size"
                                    .to_owned(),
                            ));
                        }

                        self.coefficients = frame
                            .components
                            .iter()
                            .map(|c| {
                                let block_count =
                                    c.block_size.width as usize * c.block_size.height as usize;
                                vec![0; block_count * 64]
                            })
                            .collect();
                    }

                    if frame.coding_process == CodingProcess::Lossless {
                        let (marker, data) = self.decode_scan_lossless(&frame, &scan)?;

                        for (i, plane) in data
                            .into_iter()
                            .enumerate()
                            .filter(|(_, plane)| !plane.is_empty())
                        {
                            planes_u16[i] = plane;
                        }
                        pending_marker = marker;
                    } else {
                        // This was previously buggy, so let's explain the log here a bit. When a
                        // progressive frame is encoded then the coefficients (DC, AC) of each
                        // component (=color plane) can be split amongst scans. In particular it can
                        // happen or at least occurs in the wild that a scan contains coefficient 0 of
                        // all components. If now one but not all components had all other coefficients
                        // delivered in previous scans then such a scan contains all components but
                        // completes only some of them! (This is technically NOT permitted for all
                        // other coefficients as the standard dictates that scans with coefficients
                        // other than the 0th must only contain ONE component so we would either
                        // complete it or not. We may want to detect and error in case more component
                        // are part of a scan than allowed.) What a weird edge case.
                        //
                        // But this means we track precisely which components get completed here.
                        let mut finished = [false; MAX_COMPONENTS];

                        if scan.successive_approximation_low == 0 {
                            for (&i, component_finished) in
                                scan.component_indices.iter().zip(&mut finished)
                            {
                                if self.coefficients_finished[i] == !0 {
                                    continue;
                                }
                                for j in scan.spectral_selection.clone() {
                                    self.coefficients_finished[i] |= 1 << j;
                                }
                                if self.coefficients_finished[i] == !0 {
                                    *component_finished = true;
                                }
                            }
                        }

                        // Must honour `single_threaded` here, not just in
                        // `decode_planes`. `get_or_init_worker` CACHES the
                        // worker on first use, and for a baseline image this
                        // site runs first — so hardcoding Multithreaded made
                        // `set_single_threaded` silently do nothing, and left
                        // `reclaim_buffer` (which only the immediate worker
                        // implements) dead in the path that allocates 6.3 MB
                        // of MCU-row buffers per 1080p frame.
                        let preference = Self::select_worker(
                            &frame,
                            if self.single_threaded {
                                PreferWorkerKind::Immediate
                            } else {
                                PreferWorkerKind::Multithreaded
                            },
                        );

                        let (marker, data) = worker_scope
                            .get_or_init_worker(preference, |worker| {
                                self.decode_scan(&frame, &scan, worker, &finished)
                            })?;

                        if let Some(data) = data {
                            for (i, plane) in data
                                .into_iter()
                                .enumerate()
                                .filter(|(_, plane)| !plane.is_empty())
                            {
                                if self.coefficients_finished[i] == !0 {
                                    planes[i] = plane;
                                }
                            }
                        }

                        pending_marker = marker;
                    }

                    scans_processed += 1;
                }

                // Table-specification and miscellaneous markers
                // Quantization table-specification
                Marker::DQT => {
                    let tables = parse_dqt(&mut self.reader)?;

                    for (i, &table) in tables.iter().enumerate() {
                        if let Some(table) = table {
                            let mut unzigzagged_table = [0u16; 64];

                            for j in 0..64 {
                                unzigzagged_table[UNZIGZAG[j] as usize] = table[j];
                            }

                            self.quantization_tables[i] = Some(Arc::new(unzigzagged_table));
                        }
                    }
                }
                // Huffman table-specification
                Marker::DHT => {
                    let is_baseline = self.frame.as_ref().map(|frame| frame.is_baseline);
                    let (dc_tables, ac_tables) = parse_dht(&mut self.reader, is_baseline)?;

                    let current_dc_tables = mem::take(&mut self.dc_huffman_tables);
                    self.dc_huffman_tables = dc_tables
                        .into_iter()
                        .zip(current_dc_tables)
                        .map(|(a, b)| a.or(b))
                        .collect();

                    let current_ac_tables = mem::take(&mut self.ac_huffman_tables);
                    self.ac_huffman_tables = ac_tables
                        .into_iter()
                        .zip(current_ac_tables)
                        .map(|(a, b)| a.or(b))
                        .collect();
                }
                // Arithmetic conditioning table-specification
                Marker::DAC => {
                    return Err(Error::Unsupported(
                        UnsupportedFeature::ArithmeticEntropyCoding,
                    ))
                }
                // Restart interval definition
                Marker::DRI => self.restart_interval = parse_dri(&mut self.reader)?,
                // Comment
                Marker::COM => {
                    let _comment = parse_com(&mut self.reader)?;
                }
                // Application data
                Marker::APP(..) => {
                    if let Some(data) = parse_app(&mut self.reader, marker)? {
                        match data {
                            AppData::Adobe(color_transform) => {
                                self.adobe_color_transform = Some(color_transform)
                            }
                            AppData::Jfif => {
                                // From the JFIF spec:
                                // "The APP0 marker is used to identify a JPEG FIF file.
                                //     The JPEG FIF APP0 marker is mandatory right after the SOI marker."
                                // Some JPEGs in the wild does not follow this though, so we allow
                                // JFIF headers anywhere APP0 markers are allowed.
                                /*
                                if previous_marker != Marker::SOI {
                                    return Err(Error::Format("the JFIF APP0 marker must come right after the SOI marker".to_owned()));
                                }
                                */

                                self.is_jfif = true;
                            }
                            AppData::Avi1 => self.is_mjpeg = true,
                            AppData::Icc(icc) => self.icc_markers.push(icc),
                            AppData::Exif(data) => self.exif_data = Some(data),
                            AppData::Xmp(data) => self.xmp_data = Some(data),
                            AppData::Psir(data) => self.psir_data = Some(data),
                        }
                    }
                }
                // Restart
                Marker::RST(..) => {
                    // Some encoders emit a final RST marker after entropy-coded data, which
                    // decode_scan does not take care of. So if we encounter one, we ignore it.
                    if previous_marker != Marker::SOS {
                        return Err(Error::Format(
                            "RST found outside of entropy-coded data".to_owned(),
                        ));
                    }
                }

                // Define number of lines
                Marker::DNL => {
                    // Section B.2.1
                    // "If a DNL segment (see B.2.5) is present, it shall immediately follow the first scan."
                    if previous_marker != Marker::SOS || scans_processed != 1 {
                        return Err(Error::Format(
                            "DNL is only allowed immediately after the first scan".to_owned(),
                        ));
                    }

                    return Err(Error::Unsupported(UnsupportedFeature::DNL));
                }

                // Hierarchical mode markers
                Marker::DHP | Marker::EXP => {
                    return Err(Error::Unsupported(UnsupportedFeature::Hierarchical))
                }

                // End of image
                Marker::EOI => break,

                _ => {
                    return Err(Error::Format(format!(
                        "{:?} marker found where not allowed",
                        marker
                    )))
                }
            }

            previous_marker = marker;
        }

        if self.frame.is_none() {
            return Err(Error::Format(
                "end of image encountered before frame".to_owned(),
            ));
        }

        let frame = self.frame.as_ref().unwrap();
        let preference = Self::select_worker(
            frame,
            if self.single_threaded {
                PreferWorkerKind::Immediate
            } else {
                PreferWorkerKind::Multithreaded
            },
        );

        worker_scope.get_or_init_worker(preference, |worker| {
            self.decode_planes(worker, planes, planes_u16)
        })
    }

    fn decode_planes(
        &mut self,
        worker: &mut dyn Worker,
        mut planes: Vec<Vec<u8>>,
        planes_u16: Vec<Vec<u16>>,
    ) -> Result<Vec<u8>> {
        if self.frame.is_none() {
            return Err(Error::Format(
                "end of image encountered before frame".to_owned(),
            ));
        }

        let frame = self.frame.as_ref().unwrap();

        if frame
            .components
            .len()
            .checked_mul(frame.output_size.width.into())
            .and_then(|m| m.checked_mul(frame.output_size.height.into()))
            .is_none_or(|m| self.decoding_buffer_size_limit < m)
        {
            return Err(Error::Format(
                "size of decoded image exceeds maximum allowed size".to_owned(),
            ));
        }

        // If we're decoding a progressive jpeg and a component is unfinished, render what we've got
        if frame.coding_process == CodingProcess::DctProgressive
            && self.coefficients.len() == frame.components.len()
        {
            for (i, component) in frame.components.iter().enumerate() {
                // Only dealing with unfinished components
                if self.coefficients_finished[i] == !0 {
                    continue;
                }

                let quantization_table =
                    match self.quantization_tables[component.quantization_table_index].clone() {
                        Some(quantization_table) => quantization_table,
                        None => continue,
                    };

                // Get the worker prepared
                let row_data = RowData {
                    index: i,
                    recycled: self.plane_pool.pop(),
                    component: component.clone(),
                    quantization_table,
                };
                worker.start(row_data)?;

                // Send the rows over to the worker and collect the result
                let coefficients_per_mcu_row = usize::from(component.block_size.width)
                    * usize::from(component.vertical_sampling_factor)
                    * 64;

                let mut tasks = (0..frame.mcu_size.height).map(|mcu_y| {
                    let offset = usize::from(mcu_y) * coefficients_per_mcu_row;
                    let row_coefficients =
                        self.coefficients[i][offset..offset + coefficients_per_mcu_row].to_vec();
                    (i, row_coefficients)
                });

                // FIXME: additional potential work stealing opportunities for rayon case if we
                // also internally can parallelize over components.
                worker.append_rows(&mut tasks)?;
                planes[i] = worker.get_result(i)?;
            }
        }

        let _s = crate::prof::scope(crate::prof::Stage::DecOutput);
        if frame.coding_process == CodingProcess::Lossless {
            return compute_image_lossless(frame, planes_u16);
        }

        // Clone the little bits we need so the borrow of `self.frame` ends here
        // and the planar branch can write back to `self`.
        let components = frame.components.clone();
        let output_size = frame.output_size;
        let color_transform = self.determine_color_transform();

        if self.planar_output {
            self.planar_result = Some(PlanarImage::from_components(
                &components,
                planes,
                output_size,
            )?);
            return Ok(Vec::new());
        }

        compute_image(&components, planes, output_size, color_transform)
    }

    fn determine_color_transform(&self) -> ColorTransform {
        if let Some(color_transform) = self.color_transform {
            return color_transform;
        }

        let frame = self.frame.as_ref().unwrap();

        if frame.components.len() == 1 {
            return ColorTransform::Grayscale;
        }

        // Using logic for determining colour as described here: https://entropymine.wordpress.com/2018/10/22/how-is-a-jpeg-images-color-type-determined/

        if frame.components.len() == 3 {
            match (
                frame.components[0].identifier,
                frame.components[1].identifier,
                frame.components[2].identifier,
            ) {
                (1, 2, 3) => {
                    return ColorTransform::YCbCr;
                }
                (1, 34, 35) => {
                    return ColorTransform::JcsBgYcc;
                }
                (82, 71, 66) => {
                    return ColorTransform::RGB;
                }
                (114, 103, 98) => {
                    return ColorTransform::JcsBgRgb;
                }
                _ => {}
            }

            if self.is_jfif {
                return ColorTransform::YCbCr;
            }
        }

        if let Some(colour_transform) = self.adobe_color_transform {
            match colour_transform {
                AdobeColorTransform::Unknown => {
                    if frame.components.len() == 3 {
                        return ColorTransform::RGB;
                    } else if frame.components.len() == 4 {
                        return ColorTransform::CMYK;
                    }
                }
                AdobeColorTransform::YCbCr => {
                    return ColorTransform::YCbCr;
                }
                AdobeColorTransform::YCCK => {
                    return ColorTransform::YCCK;
                }
            }
        } else if frame.components.len() == 4 {
            return ColorTransform::CMYK;
        }

        if frame.components.len() == 4 {
            ColorTransform::YCCK
        } else if frame.components.len() == 3 {
            ColorTransform::YCbCr
        } else {
            ColorTransform::Unknown
        }
    }

    fn read_marker(&mut self) -> Result<Marker> {
        loop {
            // This should be an error as the JPEG spec doesn't allow extraneous data between marker segments.
            // libjpeg allows this though and there are images in the wild utilising it, so we are
            // forced to support this behavior.
            // Sony Ericsson P990i is an example of a device which produce this sort of JPEGs.
            while read_u8(&mut self.reader)? != 0xFF {}

            // Section B.1.1.2
            // All markers are assigned two-byte codes: an X’FF’ byte followed by a
            // byte which is not equal to 0 or X’FF’ (see Table B.1). Any marker may
            // optionally be preceded by any number of fill bytes, which are bytes
            // assigned code X’FF’.
            let mut byte = read_u8(&mut self.reader)?;

            // Section B.1.1.2
            // "Any marker may optionally be preceded by any number of fill bytes, which are bytes assigned code X’FF’."
            while byte == 0xFF {
                byte = read_u8(&mut self.reader)?;
            }

            if byte != 0x00 && byte != 0xFF {
                return Ok(Marker::from_u8(byte).unwrap());
            }
        }
    }

    #[allow(clippy::type_complexity)]
    #[allow(clippy::too_many_arguments)]
    fn decode_scan(
        &mut self,
        frame: &FrameInfo,
        scan: &ScanInfo,
        worker: &mut dyn Worker,
        finished: &[bool; MAX_COMPONENTS],
    ) -> Result<(Option<Marker>, Option<Vec<Vec<u8>>>)> {
        let _s = crate::prof::scope(crate::prof::Stage::DecScan);
        assert!(scan.component_indices.len() <= MAX_COMPONENTS);

        let components: Vec<Component> = scan
            .component_indices
            .iter()
            .map(|&i| frame.components[i].clone())
            .collect();

        // Verify that all required quantization tables has been set.
        if components
            .iter()
            .any(|component| self.quantization_tables[component.quantization_table_index].is_none())
        {
            return Err(Error::Format("use of unset quantization table".to_owned()));
        }

        if self.is_mjpeg {
            fill_default_mjpeg_tables(
                scan,
                &mut self.dc_huffman_tables,
                &mut self.ac_huffman_tables,
            );
        }

        // Verify that all required huffman tables has been set.
        if scan.spectral_selection.start == 0
            && scan
                .dc_table_indices
                .iter()
                .any(|&i| self.dc_huffman_tables[i].is_none())
        {
            return Err(Error::Format(
                "scan makes use of unset dc huffman table".to_owned(),
            ));
        }
        if scan.spectral_selection.end > 1
            && scan
                .ac_table_indices
                .iter()
                .any(|&i| self.ac_huffman_tables[i].is_none())
        {
            return Err(Error::Format(
                "scan makes use of unset ac huffman table".to_owned(),
            ));
        }

        // Prepare the worker thread for the work to come.
        for (i, component) in components.iter().enumerate() {
            if finished[i] {
                let row_data = RowData {
                    index: i,
                    recycled: self.plane_pool.pop(),
                    component: component.clone(),
                    quantization_table: self.quantization_tables
                        [component.quantization_table_index]
                        .clone()
                        .unwrap(),
                };

                worker.start(row_data)?;
            }
        }

        let is_progressive = frame.coding_process == CodingProcess::DctProgressive;
        let is_interleaved = components.len() > 1;
        // Fuse entropy decode straight into the IDCT when the worker consumes
        // blocks synchronously. That removes the write -> zero -> read round trip
        // through the per-MCU-row coefficient buffer -- 18.8 MB per 1080p frame
        // through a ~60 KB buffer that does not fit in L1 -- and keeps each block
        // in L1 from the symbol loop through the transform.
        //
        // Restricted to baseline interleaved scans: progressive revisits blocks
        // across scans and non-interleaved streams index the row buffer by their
        // position WITHIN a batch, so both still need the buffer.
        let fused_eligible = worker.supports_fused()
            && !is_progressive
            && is_interleaved
            && scan.successive_approximation_high == 0;
        // `supports_fused` is true for exactly the worker `as_immediate`
        // resolves, so this bool decides both paths.
        let use_fused = fused_eligible;
        let mut dummy_block = [0i16; 64];
        let mut huffman = HuffmanDecoder::new();
        let mut dc_predictors = [0i16; MAX_COMPONENTS];
        let mut mcus_left_until_restart = self.restart_interval;
        let mut expected_rst_num = 0;
        let mut eob_run = 0;
        let mut mcu_row_coefficients = vec![vec![]; components.len()];

        // A fused scan never touches this buffer -- allocating and zeroing it
        // would be pure waste.
        if !is_progressive && !use_fused {
            for (i, component) in components.iter().enumerate().filter(|&(i, _)| finished[i]) {
                let coefficients_per_mcu_row = component.block_size.width as usize
                    * component.vertical_sampling_factor as usize
                    * 64;
                mcu_row_coefficients[i] = vec![0i16; coefficients_per_mcu_row];
            }
        }

        // 4.8.2
        // When reading from the stream, if the data is non-interleaved then an MCU consists of
        // exactly one block (effectively a 1x1 sample).
        let (mcu_horizontal_samples, mcu_vertical_samples) = if is_interleaved {
            let horizontal = components
                .iter()
                .map(|component| component.horizontal_sampling_factor as u16)
                .collect::<Vec<_>>();
            let vertical = components
                .iter()
                .map(|component| component.vertical_sampling_factor as u16)
                .collect::<Vec<_>>();
            (horizontal, vertical)
        } else {
            (vec![1], vec![1])
        };

        // This also affects how many MCU values we read from stream. If it's a non-interleaved stream,
        // the MCUs will be exactly the block count.
        let (max_mcu_x, max_mcu_y) = if is_interleaved {
            (frame.mcu_size.width, frame.mcu_size.height)
        } else {
            (
                components[0].block_size.width,
                components[0].block_size.height,
            )
        };

        for mcu_y in 0..max_mcu_y {
            if mcu_y * 8 >= frame.image_size.height {
                break;
            }

            for mcu_x in 0..max_mcu_x {
                if mcu_x * 8 >= frame.image_size.width {
                    break;
                }

                if self.restart_interval > 0 {
                    if mcus_left_until_restart == 0 {
                        match huffman.take_marker(&mut self.reader)? {
                            Some(Marker::RST(n)) => {
                                if n != expected_rst_num {
                                    return Err(Error::Format(format!(
                                        "found RST{} where RST{} was expected",
                                        n, expected_rst_num
                                    )));
                                }

                                huffman.reset();
                                // Section F.2.1.3.1
                                dc_predictors = [0i16; MAX_COMPONENTS];
                                // Section G.1.2.2
                                eob_run = 0;

                                expected_rst_num = (expected_rst_num + 1) % 8;
                                mcus_left_until_restart = self.restart_interval;
                            }
                            Some(marker) => {
                                return Err(Error::Format(format!(
                                    "found marker {:?} inside scan where RST{} was expected",
                                    marker, expected_rst_num
                                )))
                            }
                            None => {
                                return Err(Error::Format(format!(
                                    "no marker found where RST{} was expected",
                                    expected_rst_num
                                )))
                            }
                        }
                    }

                    mcus_left_until_restart -= 1;
                }

                let _bl = crate::prof::scope(crate::prof::Stage::DecBlockLoop);
                // Resolved once per MCU rather than once per block: all six
                // blocks of a 4:2:0 MCU then reach the transform through a
                // STATIC call the optimizer can inline, instead of six indirect
                // ones through `&mut dyn Worker`. The borrow is scoped to this
                // loop so the row-dispatch code below can still use `worker`.
                let mut fused_worker = if use_fused {
                    worker.as_immediate()
                } else {
                    None
                };
                for (i, component) in components.iter().enumerate() {
                    // Hoisted out of the per-BLOCK loops below. All of this is
                    // fixed for the whole component, but it used to be
                    // re-derived ~49k times per 1080p frame: two double
                    // indirections through the scan's table indices plus an
                    // `Option::as_ref`, and a `Range` clone, per block.
                    let dc_table = self.dc_huffman_tables[scan.dc_table_indices[i]].as_ref();
                    let ac_table = self.ac_huffman_tables[scan.ac_table_indices[i]].as_ref();
                    let spectral_selection = scan.spectral_selection.clone();
                    let blocks_wide = component.block_size.width as usize;
                    let fused = use_fused && finished[i];
                    for v_pos in 0..mcu_vertical_samples[i] {
                        for h_pos in 0..mcu_horizontal_samples[i] {
                            if fused {
                                // Stack-local, so it never leaves L1.
                                let mut block = [0i16; 64];
                                decode_block(
                                    &mut self.reader,
                                    &mut block,
                                    &mut huffman,
                                    dc_table,
                                    ac_table,
                                    spectral_selection.clone(),
                                    scan.successive_approximation_low,
                                    &mut eob_run,
                                    &mut dc_predictors[i],
                                )?;
                                let by = (mcu_y * mcu_vertical_samples[i] + v_pos) as usize;
                                let bx = (mcu_x * mcu_horizontal_samples[i] + h_pos) as usize;
                                fused_worker
                                    .as_mut()
                                    .unwrap()
                                    .fused_block_inner(i, by, bx, &block);
                                continue;
                            }
                            let coefficients = if is_progressive {
                                let block_y = (mcu_y * mcu_vertical_samples[i] + v_pos) as usize;
                                let block_x = (mcu_x * mcu_horizontal_samples[i] + h_pos) as usize;
                                let block_offset = (block_y * blocks_wide + block_x) * 64;
                                &mut self.coefficients[scan.component_indices[i]]
                                    [block_offset..block_offset + 64]
                            } else if finished[i] {
                                // Because the worker thread operates in batches as if we were always interleaved, we
                                // need to distinguish between a single-shot buffer and one that's currently in process
                                // (for a non-interleaved) stream
                                let mcu_batch_current_row = if is_interleaved {
                                    0
                                } else {
                                    mcu_y % component.vertical_sampling_factor as u16
                                };

                                let block_y = (mcu_batch_current_row * mcu_vertical_samples[i]
                                    + v_pos) as usize;
                                let block_x = (mcu_x * mcu_horizontal_samples[i] + h_pos) as usize;
                                let block_offset = (block_y * blocks_wide + block_x) * 64;
                                &mut mcu_row_coefficients[i][block_offset..block_offset + 64]
                            } else {
                                &mut dummy_block[..64]
                            }
                            .try_into()
                            .unwrap();

                            if scan.successive_approximation_high == 0 {
                                decode_block(
                                    &mut self.reader,
                                    coefficients,
                                    &mut huffman,
                                    dc_table,
                                    ac_table,
                                    spectral_selection.clone(),
                                    scan.successive_approximation_low,
                                    &mut eob_run,
                                    &mut dc_predictors[i],
                                )?;
                            } else {
                                decode_block_successive_approximation(
                                    &mut self.reader,
                                    coefficients,
                                    &mut huffman,
                                    ac_table,
                                    spectral_selection.clone(),
                                    scan.successive_approximation_low,
                                    &mut eob_run,
                                )?;
                            }
                        }
                    }
                }
            }

            // Send the coefficients from this MCU row to the worker thread for dequantization and idct.
            for (i, component) in components.iter().enumerate() {
                if finished[i] {
                    if use_fused {
                        // Already transformed block-by-block; nothing buffered.
                        continue;
                    }
                    // In the event of non-interleaved streams, if we're still building the buffer out,
                    // keep going; don't send it yet. We also need to ensure we don't skip over the last
                    // row(s) of the image.
                    if !is_interleaved
                        && (mcu_y + 1) * 8 < frame.image_size.height
                        && (mcu_y + 1) % component.vertical_sampling_factor as u16 > 0
                    {
                        continue;
                    }

                    let coefficients_per_mcu_row = component.block_size.width as usize
                        * component.vertical_sampling_factor as usize
                        * 64;

                    let row_coefficients = if is_progressive {
                        // Because non-interleaved streams will have multiple MCU rows concatenated together,
                        // the row for calculating the offset is different.
                        let worker_mcu_y = if is_interleaved {
                            mcu_y
                        } else {
                            // Explicitly doing floor-division here
                            mcu_y / component.vertical_sampling_factor as u16
                        };

                        let offset = worker_mcu_y as usize * coefficients_per_mcu_row;
                        self.coefficients[scan.component_indices[i]]
                            [offset..offset + coefficients_per_mcu_row]
                            .to_vec()
                    } else {
                        // Refill a buffer the worker has finished with when it
                        // offers one, instead of allocating and zeroing a fresh
                        // Vec for every MCU row of every component (~6.3 MB per
                        // 1080p frame). Workers that ship rows to another thread
                        // cannot return them, and fall back to allocating.
                        let _a = crate::prof::scope(crate::prof::Stage::DecMcuRowAlloc);
                        let replacement = match worker.reclaim_buffer() {
                            Some(mut buf) => {
                                buf.clear();
                                buf.resize(coefficients_per_mcu_row, 0);
                                buf
                            }
                            None => vec![0i16; coefficients_per_mcu_row],
                        };
                        mem::replace(&mut mcu_row_coefficients[i], replacement)
                    };

                    // FIXME: additional potential work stealing opportunities for rayon case if we
                    // also internally can parallelize over components.
                    let _d = crate::prof::scope(crate::prof::Stage::DecRowDispatch);
                    worker.append_row((i, row_coefficients))?;
                }
            }
        }

        let mut marker = huffman.take_marker(&mut self.reader)?;
        while let Some(Marker::RST(_)) = marker {
            marker = self.read_marker().ok();
        }

        if finished.iter().any(|&c| c) {
            // Retrieve all the data from the worker thread.
            let mut data = vec![Vec::new(); frame.components.len()];

            for (i, &component_index) in scan.component_indices.iter().enumerate() {
                if finished[i] {
                    data[component_index] = worker.get_result(i)?;
                }
            }

            Ok((marker, Some(data)))
        } else {
            Ok((marker, None))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_block<R: Source>(
    reader: &mut crate::decode::Buffered<R>,
    coefficients: &mut [i16; 64],
    huffman: &mut HuffmanDecoder,
    dc_table: Option<&HuffmanTable>,
    ac_table: Option<&HuffmanTable>,
    spectral_selection: Range<u8>,
    successive_approximation_low: u8,
    eob_run: &mut u16,
    dc_predictor: &mut i16,
) -> Result<()> {
    let _s = crate::prof::scope(crate::prof::Stage::DecEntropy);
    debug_assert_eq!(coefficients.len(), 64);

    if spectral_selection.start == 0 {
        // Section F.2.2.1
        // Figure F.12
        //
        // A scan that codes DC must define a DC table. A file that names one it
        // never defined is malformed, and malformed input is an error, not a
        // panic — this is a decoder for bytes it did not produce.
        let dc_table = match dc_table {
            Some(table) => table,
            None => {
                return Err(Error::Format(
                    "scan codes DC coefficients but defines no DC Huffman table".to_owned(),
                ))
            }
        };
        let value = huffman.decode(reader, dc_table)?;
        let diff = match value {
            0 => 0,
            1..=11 => huffman.receive_extend(reader, value)?,
            _ => {
                // Section F.1.2.1.1
                // Table F.1
                return Err(Error::Format(
                    "invalid DC difference magnitude category".to_owned(),
                ));
            }
        };

        // Malicious JPEG files can cause this add to overflow, therefore we use wrapping_add.
        // One example of such a file is tests/crashtest/images/dc-predictor-overflow.jpg
        *dc_predictor = dc_predictor.wrapping_add(diff);
        coefficients[0] = *dc_predictor << successive_approximation_low;
    }

    let mut index = cmp::max(spectral_selection.start, 1);

    if index < spectral_selection.end && *eob_run > 0 {
        *eob_run -= 1;
        return Ok(());
    }

    // Section F.1.2.2.1
    //
    // Only a scan that actually codes AC coefficients needs an AC table, and
    // the table is resolved ONCE here rather than per symbol (~362k times per
    // 1080p frame).
    //
    // The guard is not decoration. A progressive DC-only scan has Ss=0, Se=0,
    // so the loop below never runs — and libjpeg, mozjpeg and Photoshop all
    // emit DHT segments PER SCAN, defining the AC tables only after the DC scan
    // has been written. Such a scan therefore names an AC table slot that does
    // not exist yet. Demanding the table before the loop panicked on the
    // majority of real progressive JPEGs; a malformed file that genuinely codes
    // AC without a table must be an error, never a panic.
    if index < spectral_selection.end {
        let ac_table = match ac_table {
            Some(table) => table,
            None => {
                return Err(Error::Format(
                    "scan codes AC coefficients but defines no AC Huffman table".to_owned(),
                ))
            }
        };
        while index < spectral_selection.end {
            if let Some((value, run)) = huffman.decode_fast_ac(reader, ac_table)? {
                index += run;

                if index >= spectral_selection.end {
                    break;
                }

                coefficients[UNZIGZAG[index as usize & 63] as usize & 63] =
                    value << successive_approximation_low;
                index += 1;
            } else {
                let byte = huffman.decode(reader, ac_table)?;
                let r = byte >> 4;
                let s = byte & 0x0f;

                if s == 0 {
                    match r {
                        15 => index += 16, // Run length of 16 zero coefficients.
                        _ => {
                            *eob_run = (1 << r) - 1;

                            if r > 0 {
                                *eob_run += huffman.get_bits(reader, r)?;
                            }

                            break;
                        }
                    }
                } else {
                    index += r;

                    if index >= spectral_selection.end {
                        break;
                    }

                    coefficients[UNZIGZAG[index as usize & 63] as usize & 63] =
                        huffman.receive_extend(reader, s)? << successive_approximation_low;
                    index += 1;
                }
            }
        }
    }

    Ok(())
}

fn decode_block_successive_approximation<R: Source>(
    reader: &mut crate::decode::Buffered<R>,
    coefficients: &mut [i16; 64],
    huffman: &mut HuffmanDecoder,
    ac_table: Option<&HuffmanTable>,
    spectral_selection: Range<u8>,
    successive_approximation_low: u8,
    eob_run: &mut u16,
) -> Result<()> {
    debug_assert_eq!(coefficients.len(), 64);

    let bit = 1 << successive_approximation_low;

    if spectral_selection.start == 0 {
        // Section G.1.2.1

        if huffman.get_bits(reader, 1)? == 1 {
            coefficients[0] |= bit;
        }
    } else {
        // Section G.1.2.3

        if *eob_run > 0 {
            *eob_run -= 1;
            refine_non_zeroes(reader, coefficients, huffman, spectral_selection, 64, bit)?;
            return Ok(());
        }

        let mut index = spectral_selection.start;

        // Same reasoning as the first-pass AC loop: resolve the table once, and
        // report a missing one rather than unwrapping into a panic.
        let ac_table = match ac_table {
            Some(table) => table,
            None => {
                return Err(Error::Format(
                    "refinement scan codes AC coefficients but defines no AC Huffman table"
                        .to_owned(),
                ))
            }
        };
        while index < spectral_selection.end {
            let byte = huffman.decode(reader, ac_table)?;
            let r = byte >> 4;
            let s = byte & 0x0f;

            let mut zero_run_length = r;
            let mut value = 0;

            match s {
                0 => {
                    match r {
                        15 => {
                            // Run length of 16 zero coefficients.
                            // We don't need to do anything special here, zero_run_length is 15
                            // and then value (which is zero) gets written, resulting in 16
                            // zero coefficients.
                        }
                        _ => {
                            *eob_run = (1 << r) - 1;

                            if r > 0 {
                                *eob_run += huffman.get_bits(reader, r)?;
                            }

                            // Force end of block.
                            zero_run_length = 64;
                        }
                    }
                }
                1 => {
                    if huffman.get_bits(reader, 1)? == 1 {
                        value = bit;
                    } else {
                        value = -bit;
                    }
                }
                _ => return Err(Error::Format("unexpected huffman code".to_owned())),
            }

            let range = Range {
                start: index,
                end: spectral_selection.end,
            };
            index = refine_non_zeroes(reader, coefficients, huffman, range, zero_run_length, bit)?;

            if value != 0 {
                coefficients[UNZIGZAG[index as usize & 63] as usize & 63] = value;
            }

            index += 1;
        }
    }

    Ok(())
}

fn refine_non_zeroes<R: Source>(
    reader: &mut crate::decode::Buffered<R>,
    coefficients: &mut [i16; 64],
    huffman: &mut HuffmanDecoder,
    range: Range<u8>,
    zrl: u8,
    bit: i16,
) -> Result<u8> {
    debug_assert_eq!(coefficients.len(), 64);

    let last = range.end - 1;
    let mut zero_run_length = zrl;

    for i in range {
        let index = UNZIGZAG[i as usize] as usize;

        let coefficient = &mut coefficients[index];

        if *coefficient == 0 {
            if zero_run_length == 0 {
                return Ok(i);
            }

            zero_run_length -= 1;
        } else if huffman.get_bits(reader, 1)? == 1 && *coefficient & bit == 0 {
            if *coefficient > 0 {
                *coefficient = coefficient
                    .checked_add(bit)
                    .ok_or_else(|| Error::Format("Coefficient overflow".to_owned()))?;
            } else {
                *coefficient = coefficient
                    .checked_sub(bit)
                    .ok_or_else(|| Error::Format("Coefficient overflow".to_owned()))?;
            }
        }
    }

    Ok(last)
}

fn compute_image(
    components: &[Component],
    mut data: Vec<Vec<u8>>,
    output_size: Dimensions,
    color_transform: ColorTransform,
) -> Result<Vec<u8>> {
    if data.is_empty() || data.iter().any(Vec::is_empty) {
        return Err(Error::Format("not all components have data".to_owned()));
    }

    if components.len() == 1 {
        let component = &components[0];
        let mut decoded: Vec<u8> = data.remove(0);

        let width = component.size.width as usize;
        let height = component.size.height as usize;
        let size = width * height;
        let line_stride = component.block_size.width as usize * component.dct_scale;

        // if the image width is a multiple of the block size,
        // then we don't have to move bytes in the decoded data
        if usize::from(output_size.width) != line_stride {
            // The first line already starts at index 0, so we need to move only lines 1..height
            // We move from the top down because all lines are being moved backwards.
            for y in 1..height {
                let destination_idx = y * width;
                let source_idx = y * line_stride;
                let end = source_idx + width;
                decoded.copy_within(source_idx..end, destination_idx);
            }
        }
        decoded.resize(size, 0);
        Ok(decoded)
    } else {
        compute_image_parallel(components, data, output_size, color_transform)
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn choose_color_convert_func(
    component_count: usize,
    color_transform: ColorTransform,
) -> Result<fn(&[Vec<u8>], &mut [u8])> {
    match component_count {
        3 => match color_transform {
            ColorTransform::None => Ok(color_no_convert),
            ColorTransform::Grayscale => Err(Error::Format(
                "Invalid number of channels (3) for Grayscale data".to_string(),
            )),
            ColorTransform::RGB => Ok(color_convert_line_rgb),
            ColorTransform::YCbCr => Ok(color_convert_line_ycbcr),
            ColorTransform::CMYK => Err(Error::Format(
                "Invalid number of channels (3) for CMYK data".to_string(),
            )),
            ColorTransform::YCCK => Err(Error::Format(
                "Invalid number of channels (3) for YCCK data".to_string(),
            )),
            ColorTransform::JcsBgYcc => Err(Error::Unsupported(
                UnsupportedFeature::ColorTransform(ColorTransform::JcsBgYcc),
            )),
            ColorTransform::JcsBgRgb => Err(Error::Unsupported(
                UnsupportedFeature::ColorTransform(ColorTransform::JcsBgRgb),
            )),
            ColorTransform::Unknown => Err(Error::Format("Unknown colour transform".to_string())),
        },
        4 => match color_transform {
            ColorTransform::None => Ok(color_no_convert),
            ColorTransform::Grayscale => Err(Error::Format(
                "Invalid number of channels (4) for Grayscale data".to_string(),
            )),
            ColorTransform::RGB => Err(Error::Format(
                "Invalid number of channels (4) for RGB data".to_string(),
            )),
            ColorTransform::YCbCr => Err(Error::Format(
                "Invalid number of channels (4) for YCbCr data".to_string(),
            )),
            ColorTransform::CMYK => Ok(color_convert_line_cmyk),
            ColorTransform::YCCK => Ok(color_convert_line_ycck),

            ColorTransform::JcsBgYcc => Err(Error::Unsupported(
                UnsupportedFeature::ColorTransform(ColorTransform::JcsBgYcc),
            )),
            ColorTransform::JcsBgRgb => Err(Error::Unsupported(
                UnsupportedFeature::ColorTransform(ColorTransform::JcsBgRgb),
            )),
            ColorTransform::Unknown => Err(Error::Format("Unknown colour transform".to_string())),
        },
        _ => panic!(),
    }
}

fn color_convert_line_rgb(data: &[Vec<u8>], output: &mut [u8]) {
    assert!(data.len() == 3, "wrong number of components for rgb");
    let [r, g, b]: &[Vec<u8>; 3] = data.try_into().unwrap();
    for (((chunk, r), g), b) in output
        .chunks_exact_mut(3)
        .zip(r.iter())
        .zip(g.iter())
        .zip(b.iter())
    {
        chunk[0] = *r;
        chunk[1] = *g;
        chunk[2] = *b;
    }
}

fn color_convert_line_ycbcr(data: &[Vec<u8>], output: &mut [u8]) {
    assert!(data.len() == 3, "wrong number of components for ycbcr");
    let [y, cb, cr]: &[_; 3] = data.try_into().unwrap();

    #[cfg(not(feature = "platform_independent"))]
    let arch_specific_pixels = {
        if let Some(ycbcr) = crate::decode::arch::get_color_convert_line_ycbcr() {
            #[allow(unsafe_code)]
            unsafe {
                ycbcr(y, cb, cr, output)
            }
        } else {
            0
        }
    };

    #[cfg(feature = "platform_independent")]
    let arch_specific_pixels = 0;

    for (((chunk, y), cb), cr) in output
        .chunks_exact_mut(3)
        .zip(y.iter())
        .zip(cb.iter())
        .zip(cr.iter())
        .skip(arch_specific_pixels)
    {
        let (r, g, b) = ycbcr_to_rgb(*y, *cb, *cr);
        chunk[0] = r;
        chunk[1] = g;
        chunk[2] = b;
    }
}

fn color_convert_line_ycck(data: &[Vec<u8>], output: &mut [u8]) {
    assert!(data.len() == 4, "wrong number of components for ycck");
    let [c, m, y, k]: &[Vec<u8>; 4] = data.try_into().unwrap();

    for ((((chunk, c), m), y), k) in output
        .chunks_exact_mut(4)
        .zip(c.iter())
        .zip(m.iter())
        .zip(y.iter())
        .zip(k.iter())
    {
        let (r, g, b) = ycbcr_to_rgb(*c, *m, *y);
        chunk[0] = r;
        chunk[1] = g;
        chunk[2] = b;
        chunk[3] = 255 - *k;
    }
}

fn color_convert_line_cmyk(data: &[Vec<u8>], output: &mut [u8]) {
    assert!(data.len() == 4, "wrong number of components for cmyk");
    let [c, m, y, k]: &[Vec<u8>; 4] = data.try_into().unwrap();

    for ((((chunk, c), m), y), k) in output
        .chunks_exact_mut(4)
        .zip(c.iter())
        .zip(m.iter())
        .zip(y.iter())
        .zip(k.iter())
    {
        chunk[0] = 255 - c;
        chunk[1] = 255 - m;
        chunk[2] = 255 - y;
        chunk[3] = 255 - k;
    }
}

fn color_no_convert(data: &[Vec<u8>], output: &mut [u8]) {
    let mut output_iter = output.iter_mut();

    for pixel in data {
        for d in pixel {
            *(output_iter.next().unwrap()) = *d;
        }
    }
}

const FIXED_POINT_OFFSET: i32 = 20;
const HALF: i32 = (1 << FIXED_POINT_OFFSET) / 2;

// ITU-R BT.601
// Based on libjpeg-turbo's jdcolext.c
fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    let y = y as i32 * (1 << FIXED_POINT_OFFSET) + HALF;
    let cb = cb as i32 - 128;
    let cr = cr as i32 - 128;

    let r = clamp_fixed_point(y + stbi_f2f(1.40200) * cr);
    let g = clamp_fixed_point(y - stbi_f2f(0.34414) * cb - stbi_f2f(0.71414) * cr);
    let b = clamp_fixed_point(y + stbi_f2f(1.77200) * cb);
    (r, g, b)
}

fn stbi_f2f(x: f32) -> i32 {
    (x * ((1 << FIXED_POINT_OFFSET) as f32) + 0.5) as i32
}

fn clamp_fixed_point(value: i32) -> u8 {
    (value >> FIXED_POINT_OFFSET).clamp(0, 255) as u8
}
