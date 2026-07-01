//! VP9 encoder — tile + frame assembly (Floor 2, brick B8).
//!
//! [`assemble_tiles`] packs the per-tile boolean-coded buffers with the 4-byte
//! big-endian size prefixes the decoder's
//! [`decode_tiles`](crate::decode) reads (all tiles but the last). [`assemble_frame`]
//! concatenates the uncompressed header, the compressed header, and the tile
//! data into the final frame. Gated by a split round-trip that mirrors the
//! decoder's exact framing and recovers each tile's boolean data intact.

/// Pack per-tile boolean-coded buffers into the frame's tile data: each tile but
/// the last is prefixed with its 4-byte big-endian length (libvpx tile framing).
pub fn assemble_tiles(tiles: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let n = tiles.len();
    for (i, t) in tiles.iter().enumerate() {
        if i + 1 != n {
            out.extend_from_slice(&(t.len() as u32).to_be_bytes());
        }
        out.extend_from_slice(t);
    }
    out
}

/// Concatenate a full VP9 frame: the uncompressed header, the compressed header
/// (whose byte length is the uncompressed header's `header_size`), then the
/// assembled tile data.
pub fn assemble_frame(
    uncompressed_header: &[u8],
    compressed_header: &[u8],
    tile_data: &[u8],
) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(uncompressed_header.len() + compressed_header.len() + tile_data.len());
    out.extend_from_slice(uncompressed_header);
    out.extend_from_slice(compressed_header);
    out.extend_from_slice(tile_data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BoolDecoder;
    use crate::encode::BoolEncoder;

    /// Split tile data exactly as `decode_tiles` does — the gate for `assemble_tiles`.
    fn split_tiles(data: &[u8], n: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut off = 0usize;
        for i in 0..n {
            if i + 1 == n {
                out.push(data[off..].to_vec());
            } else {
                let sz =
                    u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
                        as usize;
                off += 4;
                out.push(data[off..off + sz].to_vec());
                off += sz;
            }
        }
        out
    }

    #[test]
    fn tile_framing_roundtrips_and_preserves_bool_data() {
        for &n in &[1usize, 2, 4, 8] {
            let mut tiles = Vec::new();
            let mut expected = Vec::new();
            for ti in 0..n {
                let mut enc = BoolEncoder::new();
                let bools: Vec<(u32, u8)> = (0..50 + ti * 7)
                    .map(|k| ((k & 1) as u32, (1 + (k * 13) % 255) as u8))
                    .collect();
                for &(b, p) in &bools {
                    enc.write_bool(b, p);
                }
                tiles.push(enc.finish());
                expected.push(bools);
            }
            let assembled = assemble_tiles(&tiles);
            // Framing recovers the exact tile buffers...
            let split = split_tiles(&assembled, n);
            assert_eq!(split, tiles, "tile split for n={n}");
            // ...and each tile's boolean stream decodes intact.
            for (ti, bools) in expected.iter().enumerate() {
                let mut bd = BoolDecoder::new(&split[ti]).unwrap();
                for &(b, p) in bools {
                    assert_eq!(bd.read_bool(p), b, "tile {ti} bool");
                }
            }
        }
    }

    #[test]
    fn frame_layout_places_headers_and_tiles() {
        let uncompressed = vec![0xAAu8; 12];
        let compressed = vec![0xBBu8; 37];
        let tiles = vec![0xCCu8; 100];
        let frame = assemble_frame(&uncompressed, &compressed, &tiles);
        assert_eq!(&frame[..12], &uncompressed[..]);
        assert_eq!(&frame[12..12 + 37], &compressed[..]);
        assert_eq!(&frame[12 + 37..], &tiles[..]);
    }
}
