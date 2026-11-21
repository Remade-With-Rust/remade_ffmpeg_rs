//! `svg2png <in.svg> <out.png> [scale]`: renders an SVG with rusty_svgtree.

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

use rusty_svgtree::rusty_raster::{Pixmap, Transform};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("usage: svg2png <in.svg> <out.png> [scale]");
        std::process::exit(2);
    };
    let scale: f32 = args.next().map_or(Ok(1.0), |s| s.parse())?;
    let data = std::fs::read(&input)?;
    let tree = rusty_svgtree::Tree::from_data(&data, &rusty_svgtree::Options::default())?;
    let w = (tree.size().width() * scale).ceil() as u32;
    let h = (tree.size().height() * scale).ceil() as u32;
    let mut pixmap = Pixmap::new(w, h).ok_or("canvas size")?;
    rusty_svgtree::render(
        &tree,
        Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.save_png(&output)?;
    Ok(())
}
