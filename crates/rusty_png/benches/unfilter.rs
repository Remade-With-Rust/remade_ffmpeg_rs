//! Usage example:
//!
//! ```
//! $ alias bench="rustup run nightly cargo bench"
//! $ bench --bench=unfilter --features=benchmarks,unstable -- --save-baseline my_baseline
//! ... tweak something, say the Sub filter ...
//! $ bench --bench=unfilter --features=benchmarks,unstable -- filter=Sub --baseline my_baseline
//! ```

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use rand::Rng;
use rusty_png::benchable_apis::unfilter;
use rusty_png::FilterType;

fn unfilter_all(c: &mut Criterion) {
    let bpps = [1, 2, 3, 4, 6, 8];
    let filters = [
        FilterType::Sub,
        FilterType::Up,
        FilterType::Avg,
        FilterType::Paeth,
    ];
    for &filter in filters.iter() {
        for &bpp in bpps.iter() {
            bench_unfilter(c, filter, bpp);
        }
    }
}

criterion_group!(benches, unfilter_all);
criterion_main!(benches);

fn bench_unfilter(c: &mut Criterion, filter: FilterType, bpp: u8) {
    let mut group = c.benchmark_group("unfilter");

    fn get_random_bytes<R: Rng>(rng: &mut R, n: usize) -> Vec<u8> {
        use rand::Fill;
        let mut result = vec![0u8; n];
        result.as_mut_slice().try_fill(rng).unwrap();
        result
    }
    let mut rng = rand::thread_rng();
    let row_size = 4096 * (bpp as usize);
    let two_rows = get_random_bytes(&mut rng, row_size * 2);

    group.throughput(Throughput::Bytes(row_size as u64));
    group.bench_with_input(
        format!("filter={filter:?}/bpp={bpp}"),
        &two_rows,
        |b, two_rows| {
            let (prev_row, curr_row) = two_rows.split_at(row_size);
            let mut curr_row = curr_row.to_vec();
            b.iter(|| unfilter(filter, bpp, prev_row, curr_row.as_mut_slice()));
        },
    );
}

// Primary allocator for this target: our rusty_alloc, the pure-Rust mimalloc
// remake. Codec hot paths allocate heavily and the system heap dominates the
// profile there (measured 1.38x end-to-end on AV2 decode). Per project
// convention this belongs in binary/bench/example roots, never in a library --
// a library that declares one hijacks every dependent's allocator choice.
#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;
