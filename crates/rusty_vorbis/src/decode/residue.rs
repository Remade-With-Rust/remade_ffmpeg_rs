//! Residue decode (Vorbis I §8), straight into the channel spectra.
//!
//! Semantics follow lewton 0.10.2's `residue_packet_decode_inner`: the
//! begin/end limits are clamped to the vector size (the corrected spec), the
//! classification words are read in pass 0, end of packet ends the residue
//! decode normally (everything decoded so far is kept), and in formats 1/2 a
//! codebook vector that would run past the end of the channel vector ends that
//! partition. Residue 2 decodes into one interleaved vector and is
//! de-interleaved into the channels afterwards.

use super::bits::BitReader;
use super::header::{Codebook, Residue};

/// The packet is undecodable (classbook with zero dimensions).
pub(crate) struct BadPacket;

/// Add one partition of format 1/2 VQ vectors into `v[..]` starting at 0.
/// Returns `true` on end of packet.
#[inline(always)]
fn partition_12<const D: usize>(
    book: &Codebook,
    br: &mut BitReader,
    v: &mut [f32],
    psize: usize,
) -> bool {
    let dim = if D == 0 { book.dim } else { D };
    let mut i = 0;
    while i < psize {
        let e = book.decode(br);
        if br.eop() {
            return true;
        }
        let row = &book.vq[e as usize * dim..e as usize * dim + dim];
        let Some(dst) = v.get_mut(i..i + dim) else {
            break;
        };
        for (d, s) in dst.iter_mut().zip(row) {
            *d += *s;
        }
        i += dim;
    }
    false
}

/// Format 0: vector `i` scatters with stride `psize / dim`.
#[inline]
fn partition_0(book: &Codebook, br: &mut BitReader, v: &mut [f32], psize: usize) -> bool {
    let dim = book.dim;
    let step = psize / dim;
    for i in 0..step {
        let e = book.decode(br);
        if br.eop() {
            return true;
        }
        for (j, &s) in book.row(e).iter().enumerate() {
            v[i + j * step] += s;
        }
    }
    false
}

#[inline]
fn partition(kind: u8, book: &Codebook, br: &mut BitReader, v: &mut [f32], psize: usize) -> bool {
    if kind == 0 {
        return partition_0(book, br, v, psize);
    }
    match book.dim {
        1 => partition_12::<1>(book, br, v, psize),
        2 => partition_12::<2>(book, br, v, psize),
        4 => partition_12::<4>(book, br, v, psize),
        8 => partition_12::<8>(book, br, v, psize),
        _ => partition_12::<0>(book, br, v, psize),
    }
}

/// Decode `nvec` vectors of `size` scalars each into `out` (`nvec * size`),
/// skipping vectors with `skip[j]` set. `classes` is reusable scratch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_inner(
    res: &Residue,
    books: &[Codebook],
    br: &mut BitReader,
    size: usize,
    skip: &[bool],
    out: &mut [f32],
    classes: &mut Vec<u8>,
) -> Result<(), BadPacket> {
    let nvec = skip.len();
    let begin = (res.begin as usize).min(size);
    let end = (res.end as usize).min(size);
    let n_to_read = end - begin;
    if n_to_read == 0 {
        return Ok(());
    }
    let classbook = &books[res.classbook as usize];
    let cpc = classbook.dim;
    if cpc == 0 {
        return Err(BadPacket);
    }
    let psize = res.partition_size as usize;
    let partitions = n_to_read / psize;
    let stride = partitions + cpc;
    classes.clear();
    classes.resize(nvec * stride, 0);
    let ncls = res.classifications;
    let kind = res.kind;
    'done: for pass in 0..8 {
        let mut pc = 0;
        while pc < partitions {
            if pass == 0 {
                for j in 0..nvec {
                    if skip[j] {
                        continue;
                    }
                    if !classbook.usable() {
                        break 'done;
                    }
                    let mut temp = classbook.decode(br);
                    if br.eop() {
                        break 'done;
                    }
                    let row = &mut classes[j * stride + pc..j * stride + pc + cpc];
                    for c in row.iter_mut().rev() {
                        *c = (temp % ncls) as u8;
                        temp /= ncls;
                    }
                }
            }
            for _ in 0..cpc {
                if pc >= partitions {
                    break;
                }
                let offs = begin + pc * psize;
                for j in 0..nvec {
                    if skip[j] {
                        continue;
                    }
                    let cls = classes[j * stride + pc] as usize;
                    let b = res.books[cls][pass];
                    if b < 0 {
                        continue;
                    }
                    let book = &books[b as usize];
                    if !book.usable() {
                        break 'done;
                    }
                    let v = &mut out[j * size + offs..(j + 1) * size];
                    if partition(kind, book, br, v, psize) {
                        break 'done;
                    }
                }
                pc += 1;
            }
        }
    }
    Ok(())
}
