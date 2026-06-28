//! Print VP9 tile layout from an IVF file: `cargo run --example tile_info -- file.ivf`.
//! A quick check of how many tile columns a stream actually carries — the unit
//! of decode parallelism.

use std::io::Read;

use rff_codec_vp9::{parse_uncompressed_header, BitReader};

fn main() {
    let path = std::env::args().nth(1).expect("usage: tile_info <file.ivf>");
    let mut data = Vec::new();
    std::fs::File::open(&path).unwrap().read_to_end(&mut data).unwrap();

    let header_len = u16::from_le_bytes([data[6], data[7]]) as usize;
    let mut pos = header_len;
    let mut frame = 0;
    while pos + 12 <= data.len() && frame < 2 {
        let size = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
            as usize;
        pos += 12;
        let fdata = &data[pos..pos + size];
        pos += size;
        let mut r = BitReader::new(fdata);
        if let Ok(h) = parse_uncompressed_header(&mut r, &[(0, 0); 8]) {
            println!(
                "frame {frame}: key={} {}x{}  tile_cols_log2={} ({} tile columns), tile_rows_log2={}",
                h.key_frame,
                h.width,
                h.height,
                h.tile_cols_log2,
                1usize << h.tile_cols_log2,
                h.tile_rows_log2,
            );
        }
        frame += 1;
    }
}
