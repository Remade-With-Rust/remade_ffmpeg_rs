# remade_ffmpeg_rs — project instructions

## Allocator convention: rusty_alloc is the primary allocator

Every encoder-carrying **binary** in this workspace uses our own
[rusty_alloc](https://github.com/Remade-With-Rust/rusty_alloc) (pure-Rust
remake of mimalloc v2.4.5) as its `#[global_allocator]`. This is the project
default — all future encoders run under it as primary.

How it is wired:

- Workspace dependency: `rusty_alloc-api` in the root `Cargo.toml`
  `[workspace.dependencies]` (the safe `GlobalAlloc` surface over the
  `rusty_alloc` core).
- CLI: declared once in `crates/rff-cli/src/lib.rs`, which covers all four
  binaries (`rff`, `rffprobe`, `ffmpeg`, `ffprobe`).

Rules for new code:

- New binary, bench, or example that runs an encoder (including the
  standalone `rusty_*` codec crates' benches/examples and any new CLI/server
  binary): add `rusty_alloc-api.workspace = true` and set

  ```rust
  #[global_allocator]
  static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;
  ```

- **Never** set the global allocator in a library crate that external users
  consume (the published `rusty_vp9`/`rusty_mp3`/`rusty_aac`/`rusty-opus`
  etc. lib targets): a library must not hijack a downstream binary's
  allocator choice. It belongs in `[[bin]]`/bench/example roots only.
- Performance measurements (codec-* skill campaigns, best-of-N benches,
  A/B arms) must run under rusty_alloc, since it is what ships — an arm
  measured under the system allocator is not comparable.

## Build gotcha

The CLI executables are built by the `rff-cli` package, not `rff` (a
library). `cargo build -p rff` never relinks the exe — always build/test
with `-p rff-cli` and verify the binary mtime before trusting a CLI A/B.
