//! Vorbis setup-header parsing → **encode-side** codebook tables.
//!
//! We embed a libvorbis setup verbatim (emitted as our setup header) and parse it
//! here into the structures the encoder needs: Huffman codeword tables (entry →
//! codeword bits) and VQ dictionaries (vector → nearest entry). The parse mirrors
//! lewton's decoder field-for-field; the **whole-setup parse landing exactly on the
//! framing bit** is the structural gate that proves every width is byte-perfect
//! against a real libvorbis blob. Floor/residue/mapping/mode configs are parsed and
//! kept for later bricks (they must be read anyway to reach the framing bit).

use rff_core::{Error, Result};

/// The embedded q4 / stereo / 44.1 kHz setup (packet 3), emitted verbatim + parsed.
pub const SETUP_Q4_STEREO: &[u8] = include_bytes!("setup_q4_stereo.bin");

// ---------------------------------------------------------------------------
// Primitives (mirrors of lewton's decoder helpers)
// ---------------------------------------------------------------------------

/// vorbis `ilog`: number of significant bits (`ilog(0)=0`, `ilog(1)=1`, `ilog(7)=3`).
fn ilog(v: u64) -> u32 {
    64 - v.leading_zeros()
}

/// vorbis float32 unpack (codebook `minimum_value` / `delta_value`).
fn float32_unpack(val: u32) -> f32 {
    let sgn = val & 0x8000_0000;
    let exp = (val & 0x7fe0_0000) >> 21;
    let mantissa = (val & 0x001f_ffff) as f64;
    let m = if sgn != 0 { -mantissa } else { mantissa };
    (m as f32) * (exp as f32 - 788.0).exp2()
}

/// `lookup1_values`: greatest integer `r` with `r^dims <= entries` (nth-root).
fn lookup1_values(entries: u32, dims: u16) -> u32 {
    if dims == 0 {
        return u32::MAX; // matches lewton; never hit in practice (dims >= 1).
    }
    let mut r: u32 = 0;
    loop {
        let next = (r as u64) + 1;
        let mut p: u64 = 1;
        let mut over = false;
        for _ in 0..dims {
            p = p.saturating_mul(next);
            if p > entries as u64 {
                over = true;
                break;
            }
        }
        if over {
            break;
        }
        r += 1;
    }
    r
}

/// LSb-first bit reader (Vorbis convention: first bit read is bit 0 of byte 0).
struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    bit: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader {
            data,
            byte: 0,
            bit: 0,
        }
    }

    /// Read the low `n` bits (`n <= 32`), least-significant bit first.
    fn read(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        let mut val = 0u32;
        for i in 0..n {
            if self.byte >= self.data.len() {
                return Err(Error::invalid("vorbis setup: unexpected end of packet"));
            }
            let b = (self.data[self.byte] >> self.bit) & 1;
            val |= (b as u32) << i;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.byte += 1;
            }
        }
        Ok(val)
    }

    fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read(1)? == 1)
    }

    fn read_f32(&mut self) -> Result<f32> {
        Ok(float32_unpack(self.read(32)?))
    }

    /// Current position in bits (for the framing-bit gate).
    fn bit_pos(&self) -> usize {
        self.byte * 8 + self.bit as usize
    }
}

// ---------------------------------------------------------------------------
// Huffman codeword assignment (libvorbis `_make_words`, verbatim port)
// ---------------------------------------------------------------------------

/// Assign canonical Vorbis Huffman codewords from codeword lengths, returning the
/// **natural** (MSb-first) codeword per entry (0 for unused, length-0 entries).
/// Verbatim port of libvorbis `_make_words`. Verified against the Vorbis I spec
/// §3.2.1 worked example in tests.
fn make_words_natural(lengths: &[u8]) -> Result<Vec<u32>> {
    let n = lengths.len();
    let mut codes = vec![0u32; n];
    let mut marker = [0u32; 33];
    for (i, &len_u8) in lengths.iter().enumerate() {
        let length = len_u8 as usize;
        if length == 0 {
            continue;
        }
        let mut entry = marker[length];
        if length < 32 && (entry >> length) != 0 {
            return Err(Error::invalid("vorbis setup: overpopulated huffman tree"));
        }
        codes[i] = entry;
        // Advance the marker at this length, jumping branches as needed.
        let mut j = length;
        while j > 0 {
            if marker[j] & 1 != 0 {
                if j == 1 {
                    marker[1] += 1;
                } else {
                    marker[j] = marker[j - 1] << 1;
                }
                break;
            }
            marker[j] += 1;
            j -= 1;
        }
        // Re-dangle the longer markers from our new node.
        for j in (length + 1)..33 {
            if (marker[j] >> 1) == entry {
                entry = marker[j];
                marker[j] = marker[j - 1] << 1;
            } else {
                break;
            }
        }
    }
    Ok(codes)
}

/// Reverse the low `len` bits of `code` (the packer writes codewords LSb-first, so
/// the stored, write-ready value is the natural codeword bit-reversed over `len`).
fn reverse_bits(code: u32, len: u8) -> u32 {
    let mut out = 0u32;
    for j in 0..len {
        out = (out << 1) | ((code >> j) & 1);
    }
    out
}

// ---------------------------------------------------------------------------
// Encode-side codebook
// ---------------------------------------------------------------------------

/// A parsed codebook with **encode-ready** tables.
pub struct Codebook {
    pub dimensions: u16,
    pub entries: u32,
    /// Write-ready codeword per entry (already LSb-first bit-reversed); pair with
    /// `lengths`. Unused entries (length 0) have codeword 0 and must not be emitted.
    pub codewords: Vec<u32>,
    pub lengths: Vec<u8>,
    /// VQ dictionary: `entries * dimensions` scalars, or `None` for scalar-only.
    pub vq: Option<Vec<f32>>,
    /// Fast-quantize lattice: `Some((levels, base))` for a *full*, non-sequential lookup-1 book
    /// with every entry used — each dimension picks independently from `levels`, so the nearest
    /// entry is the per-dimension nearest in `O(dim·levels)` vs `O(entries·dim)`. Quality-neutral
    /// (it's the min-error entry, same as the brute-force search at λ=0; verified).
    pub lattice: Option<(Vec<f32>, u32)>,
    /// Used-entry VQ vectors in **structure-of-arrays** layout: `used_vq[d·used_count + i]` is
    /// dimension `d` of the `i`-th used entry, so each dimension's column is contiguous. Sparse
    /// books (e.g. the dim-8 residue book uses 81 of 6561 entries) iterate only the live entries;
    /// the SoA columns let the branchless distance pass in [`quantize_vector`] auto-vectorize.
    used_vq: Vec<f32>,
    /// Original entry index for each packed used column `i` (maps the argmin back to a codebook entry).
    used_entry: Vec<u32>,
    /// Codeword length (as `f32`) for each packed used column `i` — a contiguous column so the
    /// rate-distortion term `λ·len` adds into the cost vector without a gather.
    used_len: Vec<f32>,
}

/// Cached AVX2 availability — the residue-VQ quantizer is called hundreds of thousands of times
/// per stream, so resolve the feature probe once into a relaxed atomic.
#[cfg(all(target_arch = "x86_64", feature = "simd"))]
fn have_avx2() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static CACHE: AtomicU8 = AtomicU8::new(2); // 0 = no, 1 = yes, 2 = unresolved
    match CACHE.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => {
            let v = std::arch::is_x86_feature_detected!("avx2");
            CACHE.store(v as u8, Ordering::Relaxed);
            v
        }
    }
}

impl Codebook {
    /// The `(codeword, length)` to write for entry `e`.
    pub fn encode(&self, e: u32) -> (u32, u8) {
        let i = e as usize;
        (self.codewords[i], self.lengths[i])
    }

    /// Nearest VQ entry to `vector`. Full lattice books quantize per dimension in `O(dim·levels)`
    /// (the dominant residue book is a dim-8 lattice); other books use the rate-distortion search
    /// (`‖v−vq‖² + λ·codeword_len`). Both give the min-error entry at λ=0.
    pub fn quantize_vector(&self, vector: &[f32], lambda: f32) -> u32 {
        let dim = self.dimensions as usize;
        if let Some((levels, base)) = &self.lattice {
            let mut entry = 0u32;
            let mut mul = 1u32;
            for &v in &vector[..dim] {
                let mut best_j = 0usize;
                let mut best_d = f32::INFINITY;
                for (j, &lvl) in levels.iter().enumerate() {
                    let d = (v - lvl) * (v - lvl);
                    if d < best_d {
                        best_d = d;
                        best_j = j;
                    }
                }
                entry += best_j as u32 * mul;
                mul *= *base;
            }
            return entry;
        }
        // Brute force over only the *used* entries. `cost` is a fresh single-provenance scratch
        // slice (stack for the common small case) so the SoA distance passes carry clean `noalias`;
        // a heap buffer covers the rare oversized book.
        let uc = self.used_entry.len();
        if uc == 0 {
            return 0;
        }
        const CAP: usize = 512;
        if uc <= CAP {
            let mut cost = [0.0f32; CAP];
            self.brute_quantize(vector, lambda, &mut cost[..uc])
        } else {
            let mut cost = vec![0.0f32; uc];
            self.brute_quantize(vector, lambda, &mut cost)
        }
    }

    /// Rate-distortion nearest used entry: fill `cost[i] = Σ_d (v[d]−col_d[i])² + λ·len[i]` then
    /// take the first-wins argmin. Dispatches to the AVX2 kernel when available (byte-identical —
    /// no FMA, same accumulation order), else the scalar path.
    #[inline]
    fn brute_quantize(&self, vector: &[f32], lambda: f32, cost: &mut [f32]) -> u32 {
        #[cfg(all(target_arch = "x86_64", feature = "simd"))]
        {
            if have_avx2() {
                // SAFETY: `have_avx2()` confirmed the ISA; the kernel only does aligned-agnostic
                // `loadu`/`storeu` within `[0, cost.len())` and the SoA columns of matching length.
                unsafe { self.brute_cost_avx2(vector, lambda, cost) };
                return self.argmin_entry(cost);
            }
        }
        self.brute_cost_scalar(vector, lambda, cost);
        self.argmin_entry(cost)
    }

    /// Scalar fill of the cost vector (the oracle + the non-AVX2 fallback). Four branchless SoA
    /// passes; `cost.len()` == used-entry count.
    #[inline]
    fn brute_cost_scalar(&self, vector: &[f32], lambda: f32, cost: &mut [f32]) {
        let dim = self.dimensions as usize;
        let uc = cost.len();
        let v0 = vector[0];
        for (c, &u) in cost.iter_mut().zip(&self.used_vq[0..uc]) {
            let diff = v0 - u;
            *c = diff * diff;
        }
        for (d, &vd) in vector[..dim].iter().enumerate().skip(1) {
            for (c, &u) in cost.iter_mut().zip(&self.used_vq[d * uc..(d + 1) * uc]) {
                let diff = vd - u;
                *c += diff * diff;
            }
        }
        for (c, &len) in cost.iter_mut().zip(&self.used_len) {
            *c += lambda * len;
        }
    }

    /// AVX2 fill of the cost vector — 8 used entries per iteration. Uses `mul`+`add` (NOT `fmadd`)
    /// and the same `d`-ascending accumulation as [`Self::brute_cost_scalar`], so every `cost[i]`
    /// is bit-identical to the scalar path (gated by `brute_quantize_matches_reference`). The
    /// first-wins argmin is left to the scalar [`Self::argmin_entry`] (min-fold + early-exit
    /// `position` measured faster than a vectorized scan on these ~80–220-entry books).
    ///
    /// # Safety
    /// Requires the `avx2` target feature (checked by the caller). Reads/writes stay within
    /// `cost[0..uc]` and the `dim` SoA columns `used_vq[d*uc .. (d+1)*uc]`, each of length `uc`.
    #[cfg(all(target_arch = "x86_64", feature = "simd"))]
    #[target_feature(enable = "avx2")]
    unsafe fn brute_cost_avx2(&self, vector: &[f32], lambda: f32, cost: &mut [f32]) {
        use std::arch::x86_64::*;
        let dim = self.dimensions as usize;
        let uc = cost.len();
        let lam = _mm256_set1_ps(lambda);
        let vq = self.used_vq.as_ptr();
        let lens = self.used_len.as_ptr();
        let out = cost.as_mut_ptr();
        let mut i = 0usize;
        while i + 8 <= uc {
            let mut acc = _mm256_setzero_ps();
            for d in 0..dim {
                let vd = _mm256_set1_ps(*vector.get_unchecked(d));
                let col = _mm256_loadu_ps(vq.add(d * uc + i));
                let diff = _mm256_sub_ps(vd, col);
                acc = _mm256_add_ps(acc, _mm256_mul_ps(diff, diff));
            }
            let rd = _mm256_mul_ps(lam, _mm256_loadu_ps(lens.add(i)));
            _mm256_storeu_ps(out.add(i), _mm256_add_ps(acc, rd));
            i += 8;
        }
        // Scalar tail (same order → same rounding).
        while i < uc {
            let mut e = 0.0f32;
            for d in 0..dim {
                let diff = *vector.get_unchecked(d) - *vq.add(d * uc + i);
                e += diff * diff;
            }
            *out.add(i) = e + lambda * *lens.add(i);
            i += 1;
        }
    }

    /// First-wins argmin over a filled cost vector, mapped back to a codebook entry. `min` value
    /// (a reduction) then the first index equal to it — exactly the scalar `if cost < best` result.
    #[inline]
    fn argmin_entry(&self, cost: &[f32]) -> u32 {
        let best_cost = cost.iter().copied().fold(f32::INFINITY, f32::min);
        let best_i = cost.iter().position(|&c| c == best_cost).unwrap_or(0);
        self.used_entry[best_i]
    }
}

/// Reconstruct the VQ dictionary (`entries * dimensions` scalars). Mirrors lewton's
/// `lookup_vec_val_decode` exactly so our dictionary is bit-identical to the decoder's.
fn vq_lookup(
    lookup_type: u8,
    min: f32,
    delta: f32,
    seq_p: bool,
    multiplicands: &[u32],
    entries: u32,
    dims: u16,
) -> Vec<f32> {
    let mut out = Vec::with_capacity(entries as usize * dims as usize);
    if lookup_type == 1 {
        let lookup_values = multiplicands.len() as u32;
        for lookup_offset in 0..entries {
            let mut last = 0.0f32;
            let mut index_divisor = 1u32;
            for _ in 0..dims {
                let moff = ((lookup_offset / index_divisor) % lookup_values) as usize;
                let v = multiplicands[moff] as f32 * delta + min + last;
                if seq_p {
                    last = v;
                }
                out.push(v);
                index_divisor *= lookup_values;
            }
        }
    } else {
        for lookup_offset in 0..entries {
            let mut last = 0.0f32;
            let base = lookup_offset as usize * dims as usize;
            for d in 0..dims as usize {
                let v = multiplicands[base + d] as f32 * delta + min + last;
                if seq_p {
                    last = v;
                }
                out.push(v);
            }
        }
    }
    out
}

fn read_codebook(rdr: &mut BitReader) -> Result<Codebook> {
    if rdr.read(24)? != 0x564342 {
        return Err(Error::invalid("vorbis setup: bad codebook sync pattern"));
    }
    let dimensions = rdr.read(16)? as u16;
    let entries = rdr.read(24)?;
    let ordered = rdr.read_bool()?;

    let mut lengths: Vec<u8> = Vec::with_capacity(entries as usize);
    if !ordered {
        let sparse = rdr.read_bool()?;
        for _ in 0..entries {
            let length = if sparse {
                if rdr.read_bool()? {
                    (rdr.read(5)? as u8) + 1
                } else {
                    0 // unused entry
                }
            } else {
                (rdr.read(5)? as u8) + 1
            };
            lengths.push(length);
        }
    } else {
        let mut current_entry: u32 = 0;
        let mut current_length = (rdr.read(5)? as u8) + 1;
        while current_entry < entries {
            let number = rdr.read(ilog((entries - current_entry) as u64))?;
            for _ in 0..number {
                lengths.push(current_length);
            }
            current_entry += number;
            current_length += 1;
            if current_entry > entries {
                return Err(Error::invalid("vorbis setup: codebook length overflow"));
            }
        }
    }

    let lookup_type = rdr.read(4)? as u8;
    if lookup_type > 2 {
        return Err(Error::invalid("vorbis setup: bad codebook lookup type"));
    }
    let (vq, lattice) = if lookup_type == 0 {
        (None, None)
    } else {
        let min = rdr.read_f32()?;
        let delta = rdr.read_f32()?;
        let value_bits = (rdr.read(4)? as u8) + 1;
        let seq_p = rdr.read_bool()?;
        let lookup_values = if lookup_type == 1 {
            lookup1_values(entries, dimensions)
        } else {
            entries * dimensions as u32
        };
        let mut multiplicands = Vec::with_capacity(lookup_values as usize);
        for _ in 0..lookup_values {
            multiplicands.push(rdr.read(value_bits as u32)?);
        }
        let vq = vq_lookup(lookup_type, min, delta, seq_p, &multiplicands, entries, dimensions);
        // A full, non-sequential lookup-1 book with every entry used is a separable lattice.
        let lattice = if lookup_type == 1 && !seq_p && lengths.iter().all(|&l| l > 0) {
            let mut prod = 1u64;
            for _ in 0..dimensions {
                prod = prod.saturating_mul(lookup_values as u64);
            }
            (prod == entries as u64).then(|| {
                let levels = multiplicands.iter().map(|&mc| mc as f32 * delta + min).collect();
                (levels, lookup_values)
            })
        } else {
            None
        };
        (Some(vq), lattice)
    };

    let natural = make_words_natural(&lengths)?;
    let codewords = natural
        .iter()
        .zip(&lengths)
        .map(|(&c, &l)| reverse_bits(c, l))
        .collect();

    // Pack the used entries for the brute-force quantizer (skips dead entries), in SoA layout:
    // column `d` (all used entries' dimension `d`) is contiguous, so the distance pass vectorizes.
    let (used_vq, used_entry, used_len) = match (&vq, lattice.is_some()) {
        (Some(v), false) => {
            let dim = dimensions as usize;
            let mut entry = Vec::new();
            let mut len_f = Vec::new();
            for (e, &len) in lengths.iter().enumerate() {
                if len > 0 {
                    entry.push(e as u32);
                    len_f.push(len as f32);
                }
            }
            let uc = entry.len();
            let mut uvq = vec![0.0f32; dim * uc];
            for (i, &e) in entry.iter().enumerate() {
                let src = e as usize * dim;
                for d in 0..dim {
                    uvq[d * uc + i] = v[src + d];
                }
            }
            (uvq, entry, len_f)
        }
        _ => (Vec::new(), Vec::new(), Vec::new()),
    };

    Ok(Codebook {
        dimensions,
        entries,
        codewords,
        lengths,
        vq,
        lattice,
        used_vq,
        used_entry,
        used_len,
    })
}

// ---------------------------------------------------------------------------
// Floors / residues / mappings / modes (parsed + kept for bricks 3/4/6/7)
// ---------------------------------------------------------------------------

pub enum Floor {
    Zero(Floor0),
    One(Floor1),
}

pub struct Floor0 {
    pub order: u8,
    pub rate: u16,
    pub bark_map_size: u16,
    pub amplitude_bits: u8,
    pub amplitude_offset: u8,
    pub book_list: Vec<u8>,
}

pub struct Floor1 {
    pub partition_class: Vec<u8>,
    pub class_dimensions: Vec<u8>,
    pub class_subclasses: Vec<u8>,
    pub class_masterbooks: Vec<u8>,
    pub subclass_books: Vec<Vec<i16>>,
    pub multiplier: u8,
    pub x_list: Vec<u32>,
}

pub struct Residue {
    pub residue_type: u8,
    pub begin: u32,
    pub end: u32,
    pub partition_size: u32,
    pub classifications: u8,
    pub classbook: u8,
    pub cascade: Vec<u8>,
    /// Per classification, the up-to-8 book indices (-1 = unused).
    pub books: Vec<[i16; 8]>,
}

pub struct Mapping {
    pub submaps: u8,
    /// `(magnitude_channel, angle_channel)` coupling steps.
    pub coupling: Vec<(u8, u8)>,
    pub mux: Vec<u8>,
    pub submap_floors: Vec<u8>,
    pub submap_residues: Vec<u8>,
}

pub struct Mode {
    pub blockflag: bool,
    pub mapping: u8,
}

fn read_floor(rdr: &mut BitReader, codebook_cnt: u16) -> Result<Floor> {
    let floor_type = rdr.read(16)? as u16;
    match floor_type {
        0 => {
            let order = rdr.read(8)? as u8;
            let rate = rdr.read(16)? as u16;
            let bark_map_size = rdr.read(16)? as u16;
            let amplitude_bits = rdr.read(6)? as u8;
            let amplitude_offset = rdr.read(8)? as u8;
            let number_of_books = (rdr.read(4)? as u8) + 1;
            let mut book_list = Vec::with_capacity(number_of_books as usize);
            for _ in 0..number_of_books {
                let v = rdr.read(8)? as u8;
                if v as u16 > codebook_cnt {
                    return Err(Error::invalid("vorbis setup: floor0 book out of range"));
                }
                book_list.push(v);
            }
            Ok(Floor::Zero(Floor0 {
                order,
                rate,
                bark_map_size,
                amplitude_bits,
                amplitude_offset,
                book_list,
            }))
        }
        1 => {
            let partitions = rdr.read(5)? as u8;
            let mut max_class: i32 = -1;
            let mut partition_class = Vec::with_capacity(partitions as usize);
            for _ in 0..partitions {
                let c = rdr.read(4)? as u8;
                max_class = max_class.max(c as i32);
                partition_class.push(c);
            }
            let class_count = (max_class + 1) as usize;
            let mut class_dimensions = Vec::with_capacity(class_count);
            let mut class_subclasses = Vec::with_capacity(class_count);
            let mut class_masterbooks = Vec::with_capacity(class_count);
            let mut subclass_books = Vec::with_capacity(class_count);
            for _ in 0..class_count {
                class_dimensions.push((rdr.read(3)? as u8) + 1);
                let subclass = rdr.read(2)? as u8;
                class_subclasses.push(subclass);
                if subclass != 0 {
                    let mb = rdr.read(8)? as u8;
                    if mb as u16 >= codebook_cnt {
                        return Err(Error::invalid("vorbis setup: floor1 masterbook out of range"));
                    }
                    class_masterbooks.push(mb);
                } else {
                    class_masterbooks.push(0);
                }
                let books_cnt = 1u16 << subclass;
                let mut books = Vec::with_capacity(books_cnt as usize);
                for _ in 0..books_cnt {
                    let book = (rdr.read(8)? as i16) - 1;
                    if book >= codebook_cnt as i16 {
                        return Err(Error::invalid("vorbis setup: floor1 subclass book out of range"));
                    }
                    books.push(book);
                }
                subclass_books.push(books);
            }
            let multiplier = (rdr.read(2)? as u8) + 1;
            let rangebits = rdr.read(4)?;
            let mut x_list = vec![0u32, 1u32 << rangebits];
            for &c in &partition_class {
                for _ in 0..class_dimensions[c as usize] {
                    x_list.push(rdr.read(rangebits)?);
                }
            }
            Ok(Floor::One(Floor1 {
                partition_class,
                class_dimensions,
                class_subclasses,
                class_masterbooks,
                subclass_books,
                multiplier,
                x_list,
            }))
        }
        _ => Err(Error::invalid("vorbis setup: unknown floor type")),
    }
}

fn read_residue(rdr: &mut BitReader, codebook_cnt: usize) -> Result<Residue> {
    let residue_type = rdr.read(16)? as u16;
    if residue_type > 2 {
        return Err(Error::invalid("vorbis setup: bad residue type"));
    }
    let begin = rdr.read(24)?;
    let end = rdr.read(24)?;
    if begin > end {
        return Err(Error::invalid("vorbis setup: residue begin > end"));
    }
    let partition_size = rdr.read(24)? + 1;
    let classifications = (rdr.read(6)? as u8) + 1;
    let classbook = rdr.read(8)? as u8;
    let mut cascade = Vec::with_capacity(classifications as usize);
    for _ in 0..classifications {
        let low = rdr.read(3)? as u8;
        let high = if rdr.read_bool()? { rdr.read(5)? as u8 } else { 0 };
        cascade.push((high << 3) | low);
    }
    let mut books = Vec::with_capacity(classifications as usize);
    for &vals_used in &cascade {
        let mut val_i = [-1i16; 8];
        // Only bits 0..7 carry book values (bit 7 is never read, per spec/lewton).
        for (i, slot) in val_i.iter_mut().enumerate().take(7) {
            if vals_used & (1 << i) != 0 {
                let entry = rdr.read(8)? as usize;
                if entry >= codebook_cnt {
                    return Err(Error::invalid("vorbis setup: residue book out of range"));
                }
                *slot = entry as i16;
            }
        }
        books.push(val_i);
    }
    if classbook as usize >= codebook_cnt {
        return Err(Error::invalid("vorbis setup: residue classbook out of range"));
    }
    Ok(Residue {
        residue_type: residue_type as u8,
        begin,
        end,
        partition_size,
        classifications,
        classbook,
        cascade,
        books,
    })
}

fn read_mapping(
    rdr: &mut BitReader,
    audio_chan_ilog: u32,
    audio_channels: u8,
    floor_count: u8,
    residue_count: u8,
) -> Result<Mapping> {
    if rdr.read(16)? != 0 {
        return Err(Error::invalid("vorbis setup: bad mapping type"));
    }
    let submaps = if rdr.read_bool()? {
        (rdr.read(4)? as u8) + 1
    } else {
        1
    };
    let coupling_steps = if rdr.read_bool()? {
        (rdr.read(8)? as u16) + 1
    } else {
        0
    };
    let mut coupling = Vec::with_capacity(coupling_steps as usize);
    for _ in 0..coupling_steps {
        let mag = rdr.read(audio_chan_ilog)? as u8;
        let angle = rdr.read(audio_chan_ilog)? as u8;
        if mag == angle || mag >= audio_channels || angle >= audio_channels {
            return Err(Error::invalid("vorbis setup: bad coupling channels"));
        }
        coupling.push((mag, angle));
    }
    if rdr.read(2)? != 0 {
        return Err(Error::invalid("vorbis setup: nonzero mapping reserved bits"));
    }
    let mux = if submaps > 1 {
        let mut m = Vec::with_capacity(audio_channels as usize);
        for _ in 0..audio_channels {
            let v = rdr.read(4)? as u8;
            if v >= submaps {
                return Err(Error::invalid("vorbis setup: mux out of range"));
            }
            m.push(v);
        }
        m
    } else {
        vec![0u8; audio_channels as usize]
    };
    let mut submap_floors = Vec::with_capacity(submaps as usize);
    let mut submap_residues = Vec::with_capacity(submaps as usize);
    for _ in 0..submaps {
        let _reserved = rdr.read(8)?;
        let floor = rdr.read(8)? as u8;
        let residue = rdr.read(8)? as u8;
        if floor >= floor_count || residue >= residue_count {
            return Err(Error::invalid("vorbis setup: submap floor/residue out of range"));
        }
        submap_floors.push(floor);
        submap_residues.push(residue);
    }
    Ok(Mapping {
        submaps,
        coupling,
        mux,
        submap_floors,
        submap_residues,
    })
}

fn read_mode(rdr: &mut BitReader, mapping_count: u8) -> Result<Mode> {
    let blockflag = rdr.read_bool()?;
    let window_type = rdr.read(16)?;
    let transform_type = rdr.read(16)?;
    let mapping = rdr.read(8)? as u8;
    if window_type != 0 || transform_type != 0 || mapping >= mapping_count {
        return Err(Error::invalid("vorbis setup: bad mode info"));
    }
    Ok(Mode { blockflag, mapping })
}

/// The parsed setup, with encode-ready codebooks + configs for later bricks.
pub struct SetupTables {
    pub codebooks: Vec<Codebook>,
    pub floors: Vec<Floor>,
    pub residues: Vec<Residue>,
    pub mappings: Vec<Mapping>,
    pub modes: Vec<Mode>,
}

/// Parse a Vorbis setup header (packet 3) into encode-side tables. Mirrors lewton's
/// `read_header_setup` field-for-field and **verifies the trailing framing bit** —
/// which only lands correctly if every preceding width was parsed exactly.
pub fn parse_setup(packet: &[u8], audio_channels: u8) -> Result<SetupTables> {
    if packet.len() < 7 || packet[0] != 0x05 || &packet[1..7] != b"vorbis" {
        return Err(Error::invalid("vorbis setup: bad header prefix"));
    }
    let mut rdr = BitReader::new(&packet[7..]);
    let audio_chan_ilog = ilog((audio_channels - 1) as u64);

    // 1. Codebooks
    let codebook_count = (rdr.read(8)? as u16) + 1;
    let mut codebooks = Vec::with_capacity(codebook_count as usize);
    for _ in 0..codebook_count {
        codebooks.push(read_codebook(&mut rdr)?);
    }

    // 2. Time-domain transforms (all must be 0)
    let time_count = (rdr.read(6)? as u8) + 1;
    for _ in 0..time_count {
        if rdr.read(16)? != 0 {
            return Err(Error::invalid("vorbis setup: nonzero time transform"));
        }
    }

    // 3. Floors
    let floor_count = (rdr.read(6)? as u8) + 1;
    let mut floors = Vec::with_capacity(floor_count as usize);
    for _ in 0..floor_count {
        floors.push(read_floor(&mut rdr, codebook_count)?);
    }

    // 4. Residues
    let residue_count = (rdr.read(6)? as u8) + 1;
    let mut residues = Vec::with_capacity(residue_count as usize);
    for _ in 0..residue_count {
        residues.push(read_residue(&mut rdr, codebooks.len())?);
    }

    // 5. Mappings
    let mapping_count = (rdr.read(6)? as u8) + 1;
    let mut mappings = Vec::with_capacity(mapping_count as usize);
    for _ in 0..mapping_count {
        mappings.push(read_mapping(
            &mut rdr,
            audio_chan_ilog,
            audio_channels,
            floor_count,
            residue_count,
        )?);
    }

    // 6. Modes
    let mode_count = (rdr.read(6)? as u8) + 1;
    let mut modes = Vec::with_capacity(mode_count as usize);
    for _ in 0..mode_count {
        modes.push(read_mode(&mut rdr, mapping_count)?);
    }

    // Framing bit — must be set, and we must be within the last byte of the packet.
    if !rdr.read_bool()? {
        return Err(Error::invalid("vorbis setup: framing bit not set"));
    }
    // The parse must have consumed the whole packet bar the final (partial) byte.
    let consumed_bytes = rdr.bit_pos().div_ceil(8) + 7; // +7 for the "vorbis" prefix
    if consumed_bytes != packet.len() {
        return Err(Error::invalid("vorbis setup: trailing data after framing bit"));
    }

    Ok(SetupTables {
        codebooks,
        floors,
        residues,
        mappings,
        modes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ilog_matches_spec() {
        assert_eq!(ilog(0), 0);
        assert_eq!(ilog(1), 1);
        assert_eq!(ilog(2), 2);
        assert_eq!(ilog(3), 2);
        assert_eq!(ilog(7), 3);
    }

    #[test]
    fn float32_unpack_matches_lewton() {
        assert_eq!(float32_unpack(1611661312), 1.0);
        assert_eq!(float32_unpack(1616117760), 5.0);
        assert_eq!(float32_unpack(3759144960), -1.0);
        assert_eq!(float32_unpack(3780634624), -1530.0);
    }

    #[test]
    fn lookup1_values_matches_spec() {
        assert_eq!(lookup1_values(1025, 10), 2);
        assert_eq!(lookup1_values(1024, 10), 2);
        assert_eq!(lookup1_values(1023, 10), 1);
        assert_eq!(lookup1_values(3126, 5), 5);
        assert_eq!(lookup1_values(3125, 5), 5);
        assert_eq!(lookup1_values(3124, 5), 4);
        assert_eq!(lookup1_values(1, 1), 1);
    }

    /// The Vorbis I spec §3.2.1 worked example: lengths [2,4,4,4,4,2,3,3] must produce
    /// exactly these natural codewords. Independent check that `_make_words` is right.
    #[test]
    fn make_words_matches_spec_example() {
        let lengths = [2u8, 4, 4, 4, 4, 2, 3, 3];
        let codes = make_words_natural(&lengths).unwrap();
        assert_eq!(
            codes,
            vec![0b00, 0b0100, 0b0101, 0b0110, 0b0111, 0b10, 0b110, 0b111]
        );
    }

    /// A codeword table must be a valid prefix code that round-trips: encode each
    /// entry (write-ready codeword, LSb-first) then decode it back to the same entry.
    #[test]
    fn huffman_roundtrips() {
        let lengths = [2u8, 4, 4, 4, 4, 2, 3, 3];
        let natural = make_words_natural(&lengths).unwrap();
        let written: Vec<u32> = natural
            .iter()
            .zip(&lengths)
            .map(|(&c, &l)| reverse_bits(c, l))
            .collect();

        // Decode entry `i` from its written codeword by accumulating the LSb-first
        // bit stream and matching against the (len, pattern) table.
        for (i, (&code, &len)) in written.iter().zip(&lengths).enumerate() {
            let mut pat = 0u32;
            let mut decoded = None;
            for k in 0..len {
                let bit = (code >> k) & 1; // bits arrive LSb-first as written
                pat |= bit << k;
                let klen = k + 1;
                // First (len, pattern) match wins (prefix-free guarantees uniqueness).
                for (e, (&c2, &l2)) in written.iter().zip(&lengths).enumerate() {
                    if l2 == klen && c2 == pat {
                        decoded = Some(e);
                        break;
                    }
                }
                if decoded.is_some() {
                    break;
                }
            }
            assert_eq!(decoded, Some(i), "entry {i} failed to round-trip");
        }
    }

    /// The embedded q4 setup must parse fully and land exactly on the framing bit —
    /// the structural gate proving every field width is byte-perfect.
    #[test]
    fn setup_parses_and_lands_on_framing_bit() {
        let s = parse_setup(SETUP_Q4_STEREO, 2).expect("setup parses");
        assert!(!s.codebooks.is_empty());
        assert!(!s.floors.is_empty());
        assert!(!s.residues.is_empty());
        assert!(!s.mappings.is_empty());
        assert!(!s.modes.is_empty());
        // Every codebook produced a length table + write-ready codeword table.
        for cb in &s.codebooks {
            assert_eq!(cb.codewords.len(), cb.entries as usize);
            assert_eq!(cb.lengths.len(), cb.entries as usize);
        }
    }

    /// Every VQ codebook's own reconstructed dictionary vectors must quantize back to
    /// their own entry (exact match, zero error).
    #[test]
    fn vq_nearest_is_exact_for_dict_vectors() {
        let s = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        let mut checked = 0;
        for cb in &s.codebooks {
            let Some(vq) = cb.vq.as_ref() else { continue };
            let dim = cb.dimensions as usize;
            // Check a used entry (skip length-0 unused ones).
            for e in 0..cb.entries as usize {
                if cb.lengths[e] == 0 {
                    continue;
                }
                let vector = &vq[e * dim..(e + 1) * dim];
                assert_eq!(cb.quantize_vector(vector, 0.0), e as u32);
                checked += 1;
                break; // one representative per codebook keeps the test fast
            }
        }
        assert!(checked > 0, "expected at least one VQ codebook");
    }

    #[test]
    #[ignore]
    fn dump_residue_structure() {
        let s = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        for (ri, r) in s.residues.iter().enumerate() {
            eprintln!(
                "RESIDUE {ri}: type={} begin={} end={} psize={} nclass={} classbook={} (dim={})",
                r.residue_type,
                r.begin,
                r.end,
                r.partition_size,
                r.classifications,
                r.classbook,
                s.codebooks[r.classbook as usize].dimensions
            );
            for c in 0..r.classifications as usize {
                let books: Vec<String> = r.books[c]
                    .iter()
                    .map(|&b| {
                        if b < 0 {
                            "-".to_string()
                        } else {
                            let cb = &s.codebooks[b as usize];
                            let used = cb.lengths.iter().filter(|&&l| l > 0).count();
                            format!("b{b}(d{},e{},u{used},{})", cb.dimensions, cb.entries,
                                if cb.lattice.is_some() { "LAT" } else { "brute" })
                        }
                    })
                    .filter(|s| s != "-")
                    .collect();
                eprintln!("  class {c}: {}", books.join(" "));
            }
        }
    }

    #[test]
    #[ignore]
    fn dump_book_lengths() {
        let s = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        for (i, cb) in s.codebooks.iter().enumerate() {
            if cb.vq.is_none() || cb.lattice.is_some() {
                continue;
            }
            let lens: Vec<u8> = cb.lengths.iter().copied().filter(|&l| l > 0).collect();
            let mn = *lens.iter().min().unwrap();
            let mx = *lens.iter().max().unwrap();
            eprintln!(
                "BOOK {i}: dim={} entries={} used={} len[min={mn} max={mx}] uniform={}",
                cb.dimensions,
                cb.entries,
                lens.len(),
                mn == mx
            );
        }
    }

    #[test]
    #[ignore]
    fn dump_vq_book_status() {
        let s = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        for (i, cb) in s.codebooks.iter().enumerate() {
            if cb.vq.is_none() {
                continue;
            }
            let used = cb.lengths.iter().filter(|&&l| l > 0).count();
            eprintln!(
                "BOOK {i}: dim={} entries={} used={} lattice={}",
                cb.dimensions,
                cb.entries,
                used,
                cb.lattice.is_some()
            );
        }
    }

    /// The SoA two-pass brute quantizer must return the exact same entry as a from-scratch
    /// reference search over the full dictionary — at λ=0 AND λ>0 (production uses λ>0, where the
    /// codeword-length term participates in the argmin). Pins the layout/two-pass as byte-identical.
    #[test]
    fn brute_quantize_matches_reference() {
        let s = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        // Reference: min (Σ(v−vq)² + λ·len) over used entries, same accumulation order, first wins.
        fn reference(cb: &Codebook, v: &[f32], lambda: f32) -> u32 {
            let dim = cb.dimensions as usize;
            let vq = cb.vq.as_ref().unwrap();
            let (mut best, mut best_cost) = (0u32, f32::INFINITY);
            for e in 0..cb.entries as usize {
                let len = cb.lengths[e];
                if len == 0 {
                    continue;
                }
                let mut err = 0.0f32;
                for d in 0..dim {
                    let diff = v[d] - vq[e * dim + d];
                    err += diff * diff;
                }
                let cost = err + lambda * len as f32;
                if cost < best_cost {
                    best_cost = cost;
                    best = e as u32;
                }
            }
            best
        }
        let mut nbrute = 0;
        for cb in &s.codebooks {
            // Only the non-lattice VQ books take the brute path.
            if cb.vq.is_none() || cb.lattice.is_some() {
                continue;
            }
            nbrute += 1;
            let dim = cb.dimensions as usize;
            let vq = cb.vq.as_ref().unwrap();
            for seed in 0..40usize {
                let v: Vec<f32> = (0..dim)
                    .map(|d| {
                        let base = (seed * 13 + d * 7) % cb.entries as usize;
                        vq[base * dim + d] + 0.05 * ((seed + 3 * d) as f32).sin()
                    })
                    .collect();
                for &lambda in &[0.0f32, 0.05, 0.15, 0.4] {
                    assert_eq!(
                        cb.quantize_vector(&v, lambda),
                        reference(cb, &v, lambda),
                        "brute mismatch dim={dim} entries={} λ={lambda}",
                        cb.entries
                    );
                }
            }
        }
        assert!(nbrute > 0, "expected non-lattice brute books");
    }

    /// The lattice fast quantizer must return the exact same entry as the brute-force min-error
    /// search (so it's a pure speedup, quality-neutral).
    #[test]
    fn lattice_quantize_matches_brute_force() {
        let s = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
        let mut nlat = 0;
        for cb in &s.codebooks {
            if cb.lattice.is_none() {
                continue;
            }
            nlat += 1;
            let dim = cb.dimensions as usize;
            let vq = cb.vq.as_ref().unwrap();
            for seed in 0..24usize {
                let v: Vec<f32> = (0..dim)
                    .map(|d| {
                        let base = (seed * 7 + d * 5) % cb.entries as usize;
                        vq[base * dim + d] + 0.013 * ((seed + 2 * d) as f32).sin()
                    })
                    .collect();
                let fast = cb.quantize_vector(&v, 0.0);
                let mut best = 0u32;
                let mut best_d = f32::INFINITY;
                for e in 0..cb.entries as usize {
                    if cb.lengths[e] == 0 {
                        continue;
                    }
                    let mut err = 0.0f32;
                    for d in 0..dim {
                        let diff = v[d] - vq[e * dim + d];
                        err += diff * diff;
                    }
                    if err < best_d {
                        best_d = err;
                        best = e as u32;
                    }
                }
                assert_eq!(fast, best, "lattice mismatch (dim={dim})");
            }
        }
        assert!(nlat > 0, "expected lattice books");
    }
}
