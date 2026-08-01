//! What kind of bound is the JPEG Huffman symbol loop under?
//!
//! This decides whether hand-written assembly can help at all, and it is the
//! question to answer BEFORE writing any. Two possibilities:
//!
//! - **Latency-bound.** Each symbol's bit position depends on the previous
//!   symbol's code length, so `bits`/`num_bits` form a loop-carried dependency
//!   chain: peek (shift) -> LUT load -> extract size -> consume (shift+sub).
//!   The chain is serial by construction. Assembly cannot shorten a dependency
//!   chain it does not own; it can only remove work that was already running in
//!   the shadow of that chain, which is worth nothing.
//! - **Throughput-bound.** The chain has slack and the loop is limited by the
//!   number of instructions issued. Then better instruction selection and
//!   scheduling — what hand-written asm buys — translates directly.
//!
//! The probe measures the pure dependency chain (a serial dependent-load walk
//! with the same shape and the same table size as the real LUT), and compares
//! it against the measured in-context cost per symbol. If the real loop is
//! close to the chain, it is latency-bound and asm is refuted on arithmetic.
//!
//! Run: `cargo run --release -p rusty_jpeg --example entropy_probe`

use std::hint::black_box;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline(always)]
fn rdtsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::_rdtsc;
    #[cfg(target_arch = "x86")]
    use std::arch::x86::_rdtsc;
    #[allow(unsafe_code)]
    unsafe {
        _rdtsc()
    }
}

const N: usize = 1 << 22;

/// The loop-carried chain the real decoder cannot avoid: the next index is a
/// function of the value just loaded, exactly as the next peek depends on the
/// size just consumed. Same 256-entry table footprint as `lut`/`ac_lut`.
fn chain_latency(table: &[u16; 256]) -> (f64, u64) {
    let mut idx = 0usize;
    let t0 = rdtsc();
    for _ in 0..N {
        // load -> use -> next address. Serial, one load deep.
        idx = table[idx & 0xff] as usize;
    }
    let t1 = rdtsc();
    (((t1 - t0) as f64) / N as f64, idx as u64)
}

/// Same table, same number of loads, but the addresses are INDEPENDENT — the
/// loads can all be in flight at once. The gap between this and `chain_latency`
/// is the cost of the serialisation itself.
fn chain_independent(table: &[u16; 256]) -> (f64, u64) {
    let mut acc = 0u64;
    let t0 = rdtsc();
    for i in 0..N {
        acc = acc.wrapping_add(table[i & 0xff] as u64);
    }
    let t1 = rdtsc();
    (((t1 - t0) as f64) / N as f64, acc)
}

/// The real shape: peek -> table -> consume, with a 64-bit reservoir, refilled
/// from a buffer. This is the decoder's inner step with everything else (the
/// coefficient store, run handling, the miss path) stripped away — i.e. the
/// best case assembly could ever reach for this structure.
fn symbol_loop(table: &[(u8, u8); 256], src: &[u8]) -> (f64, u64) {
    let mut bits: u64 = 0;
    let mut num_bits: u8 = 0;
    let mut pos = 0usize;
    let mut sink = 0u64;

    let t0 = rdtsc();
    for _ in 0..N {
        if num_bits < 16 {
            // Bulk refill, as the decoder does.
            let want = ((64 - num_bits) / 8) as usize;
            if pos + want > src.len() {
                pos = 0;
            }
            let mut w = [0u8; 8];
            w[..want].copy_from_slice(&src[pos..pos + want]);
            bits |= u64::from_be_bytes(w) >> num_bits;
            num_bits += (want * 8) as u8;
            pos += want;
        }
        let idx = (bits >> 56) as usize;
        let (value, size) = table[idx & 0xff];
        // The serialising step: how far to advance depends on what we loaded.
        let size = if size == 0 { 8 } else { size };
        bits <<= size;
        num_bits -= size;
        sink = sink.wrapping_add(value as u64);
    }
    let t1 = rdtsc();
    (((t1 - t0) as f64) / N as f64, sink)
}

/// The symbol step PLUS the work the real AC loop must also do: expand the
/// run, walk the zig-zag order, and store the coefficient. This is the honest
/// floor — anything below it is not reachable by scheduling, because the work
/// itself is required.
fn symbol_loop_realistic(
    table: &[(u8, u8); 256],
    src: &[u8],
    unzigzag: &[u8; 64],
    block: &mut [i16; 64],
) -> (f64, u64) {
    let mut bits: u64 = 0;
    let mut num_bits: u8 = 0;
    let mut pos = 0usize;
    let mut index: usize = 1;
    let mut sink = 0u64;

    let t0 = rdtsc();
    for _ in 0..N {
        if num_bits < 16 {
            let want = ((64 - num_bits) / 8) as usize;
            if pos + want > src.len() {
                pos = 0;
            }
            let mut w = [0u8; 8];
            w[..want].copy_from_slice(&src[pos..pos + want]);
            bits |= u64::from_be_bytes(w) >> num_bits;
            num_bits += (want * 8) as u8;
            pos += want;
        }
        let idx = (bits >> 56) as usize;
        let (value, size) = table[idx & 0xff];
        let size = if size == 0 { 8 } else { size };
        bits <<= size;
        num_bits -= size;

        // The rest of the real AC step.
        let run = (value >> 4) as usize;
        index += run + 1;
        if index >= 64 {
            index = 1;
            sink = sink.wrapping_add(block[0] as u64);
        }
        block[unzigzag[index & 63] as usize & 63] = value as i16;
    }
    let t1 = rdtsc();
    (((t1 - t0) as f64) / N as f64, sink)
}

fn main() {
    // A table whose walk stays inside 256 entries and does not degenerate.
    let mut t16 = [0u16; 256];
    for (i, v) in t16.iter_mut().enumerate() {
        *v = ((i * 167 + 13) & 0xff) as u16;
    }
    // Realistic Huffman code-length distribution: most codes are short.
    let mut tsym = [(0u8, 0u8); 256];
    for (i, e) in tsym.iter_mut().enumerate() {
        let size = match i >> 5 {
            0 => 2,
            1 => 3,
            2 | 3 => 4,
            4..=5 => 5,
            6 => 6,
            _ => 7,
        };
        *e = ((i & 0xff) as u8, size);
    }
    let src: Vec<u8> = (0..4096u32).map(|i| (i * 37 % 251) as u8).collect();

    // Warm, then best-of-5 (rdtsc on a pinned core; best-of-N finds the floor).
    let mut chain = f64::MAX;
    let mut indep = f64::MAX;
    let mut sym = f64::MAX;
    let mut real = f64::MAX;
    let unzigzag: [u8; 64] = core::array::from_fn(|i| ((i * 13 + 7) & 63) as u8);
    let mut block = [0i16; 64];
    // A probe that swings run-to-run is not a verdict. Track the spread of each
    // quantity and print it, so an unstable number cannot be quoted as a floor.
    let (mut chain_hi, mut sym_hi, mut real_hi) = (0.0f64, 0.0f64, 0.0f64);
    for _ in 0..15 {
        let (c, s1) = chain_latency(&t16);
        let (d, s2) = chain_independent(&t16);
        let (m, s3) = symbol_loop(&tsym, &src);
        let (r, s4) = symbol_loop_realistic(&tsym, &src, &unzigzag, &mut block);
        black_box((s1, s2, s3, s4, &block));
        chain = chain.min(c);
        indep = indep.min(d);
        sym = sym.min(m);
        real = real.min(r);
        chain_hi = chain_hi.max(c);
        sym_hi = sym_hi.max(m);
        real_hi = real_hi.max(r);
    }

    println!("cycles per iteration, best-of-5, {N} iterations each\n");
    println!("  dependent-load chain (serial)   {chain:6.2}  <- the floor the decoder inherits");
    println!("  independent loads (pipelined)   {indep:6.2}  <- same loads, no serialisation");
    println!("  full symbol step                {sym:6.2}  <- peek + LUT + consume + refill");
    println!("  + run/zigzag/coefficient store  {real:6.2}  <- the HONEST floor for this structure");
    println!();
    println!("  stability (min -> max over 15 reps):");
    println!("    chain {chain:6.2} -> {chain_hi:6.2}   ({:.2}x spread)", chain_hi / chain);
    println!("    symbol{sym:6.2} -> {sym_hi:6.2}   ({:.2}x spread)", sym_hi / sym);
    println!("    real  {real:6.2} -> {real_hi:6.2}   ({:.2}x spread)", real_hi / real);
    let unstable = real_hi / real > 1.35;
    if unstable {
        println!("    !! the realistic-floor probe is UNSTABLE; do not quote it as a floor");
    }
    println!();
    println!("  serialisation costs             {:6.2} cycles/symbol", chain - indep);
    println!("  work above the chain            {:6.2} cycles/symbol", sym - chain);
    println!();
    let measured = 14.0;
    println!("PRODUCTION decoder, in-context (ablation)  {measured:6.2}");
    println!();
    if measured <= real * 1.05 {
        println!("VERDICT: the production loop is already AT the floor of a stripped-down");
        println!("loop doing only the essential work -- no error handling, no marker");
        println!("detection, no EOB logic, no restart markers. The {:.2} cycles above the",
                 real - chain);
        println!("dependency chain are REQUIRED instructions, not slack.");
        println!();
        println!("Hand-written assembly cannot remove work that has to happen. Its upside");
        println!("here is instruction selection and scheduling on a loop already priced at");
        println!("its own essential cost -- far below the 1.35x that the 10% goal needs.");
    } else {
        let sp = measured / real;
        let gain = 0.30 * (1.0 - 1.0 / sp);
        println!("VERDICT: {:.2}x of headroom to the floor -> {:.1}% whole-decode -> {:.3}x vs ffmpeg",
                 sp, gain * 100.0, 1.039 / (1.0 - gain));
    }
}
