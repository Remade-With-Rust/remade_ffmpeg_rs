//! `cargo run --release --example tiffinfo -- in.tif [out.tif]`
//!
//! Lists every page (and SubIFD) of a TIFF; with an output path, re-encodes
//! the first page losslessly with Deflate + horizontal predictor where the
//! layout allows it.

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

use std::io::{BufReader, BufWriter};

use rusty_tiff::encoder::{colortype, Compression, Predictor, TiffEncoder};
use rusty_tiff::{ColorType, Decoder, DecodingResult, TiffError};

fn describe<R: std::io::Read + std::io::Seek>(d: &mut Decoder<R>, label: &str) {
    match d.info() {
        Ok(i) => println!(
            "{label}: {}x{} {:?} compression={} photometric={} {} planar={} bits={} spp={}",
            i.width,
            i.height,
            d.colortype(),
            i.compression,
            i.photometric,
            if i.tiled { "tiled" } else { "strips" },
            i.planar,
            i.bits_per_sample,
            i.samples_per_pixel
        ),
        Err(e) => println!("{label}: not an image ({e})"),
    }
    match d.read_image() {
        Ok(px) => println!("  decoded {} samples", px.len()),
        Err(e) => println!("  decode failed: {e}"),
    }
}

fn main() -> Result<(), TiffError> {
    let mut args = std::env::args().skip(1);
    let Some(input) = args.next() else {
        eprintln!("usage: tiffinfo in.tif [out.tif]");
        std::process::exit(2);
    };
    let mut d = Decoder::new(BufReader::new(std::fs::File::open(&input)?))?;
    println!(
        "{input}: {:?}{}",
        d.byte_order(),
        if d.is_bigtiff() { " BigTIFF" } else { "" }
    );
    let mut page = 0;
    loop {
        describe(&mut d, &format!("page {page}"));
        if let Ok(Some(icc)) = d.icc_profile() {
            println!("  ICC profile: {} bytes", icc.len());
        }
        let subs = d.sub_ifds().unwrap_or_default();
        let here = d.current_ifd().offset();
        for (k, off) in subs.iter().enumerate() {
            if d.seek_to_ifd(*off).is_ok() {
                describe(&mut d, &format!("  subifd {k}"));
            }
        }
        if !subs.is_empty() {
            d.seek_to_ifd(here)?;
        }
        if !d.more_images() {
            break;
        }
        d.next_image()?;
        page += 1;
    }

    if let Some(out) = args.next() {
        d.seek_to_image(0)?;
        let (w, h) = d.dimensions()?;
        let ct = d.colortype()?;
        let px = d.read_image()?;
        let icc = d.icc_profile()?;
        let mut f = BufWriter::new(std::fs::File::create(&out)?);
        let mut enc = TiffEncoder::new(&mut f)?
            .with_compression(Compression::deflate())
            .with_predictor(Predictor::Horizontal);
        macro_rules! write {
            ($ct:ty, $v:expr) => {{
                let mut img = enc.new_image::<$ct>(w, h)?;
                if let Some(icc) = icc {
                    img.icc_profile(icc);
                }
                img.write_data($v)?;
            }};
        }
        match (ct, &px) {
            (ColorType::Gray(8), DecodingResult::U8(v)) => write!(colortype::Gray8, v),
            (ColorType::Gray(16), DecodingResult::U16(v)) => write!(colortype::Gray16, v),
            (ColorType::RGB(8), DecodingResult::U8(v)) => write!(colortype::RGB8, v),
            (ColorType::RGB(16), DecodingResult::U16(v)) => write!(colortype::RGB16, v),
            (ColorType::RGBA(8), DecodingResult::U8(v)) => write!(colortype::RGBA8, v),
            (ColorType::RGBA(16), DecodingResult::U16(v)) => write!(colortype::RGBA16, v),
            (ColorType::CMYK(8), DecodingResult::U8(v)) => write!(colortype::CMYK8, v),
            (ColorType::CMYK(16), DecodingResult::U16(v)) => write!(colortype::CMYK16, v),
            _ => {
                eprintln!("re-encode of {ct:?} not handled by this example");
                std::process::exit(1);
            }
        }
        println!("wrote {out}");
    }
    Ok(())
}
