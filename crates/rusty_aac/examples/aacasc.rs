//! `aacasc <hex>` — print a parsed AudioSpecificConfig and its output layout.
#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;
fn main() {
    let hex: String = std::env::args().skip(1).collect::<Vec<_>>().join("");
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    let c = rusty_aac::config::parse(&bytes).expect("parse");
    println!("{c:#?}");
    if let Some(p) = &c.pce {
        let rows = rusty_aac::decode::layout::pce_layout(p);
        println!("rows {rows:?}");
        let oc = rusty_aac::decode::layout::OutputConfig::configure(rows, false);
        println!("outputs {:?} mask {:#x}", oc.outputs, oc.mask);
    }
}
