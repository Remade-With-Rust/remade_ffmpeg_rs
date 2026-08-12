# NOTICE — attribution and provenance

`rusty_gif` is a performance fork carried in-tree by Mata Network's
Remade With Rust project. It merges two upstream crates:

## image-rs/image-gif (`gif` 0.14.2)

- Upstream: <https://github.com/image-rs/image-gif>, crates.io `gif` 0.14.2
- Authors: The image-rs Developers
- License: MIT OR Apache-2.0 (both texts retained in this crate root)
- Vendored as the crate body (`src/` except `neuquant.rs`); the upstream
  README is preserved as `UPSTREAM-README.md`.

## image-rs/color_quant (`color_quant` 1.1.0)

- Upstream: <https://github.com/image-rs/color_quant>, crates.io `color_quant` 1.1.0
- Copyright (c) 2014 The Piston Developers; NeuQuant algorithm
  Copyright (c) 1994 Anthony Dekker
- License: MIT (license text and the Dekker NeuQuant notice are retained
  verbatim at the top of `src/neuquant.rs`, as its license requires)
- Vendored as `src/neuquant.rs` (upstream `src/lib.rs` + `src/math.rs`).

The LZW layer remains the external pure-Rust `weezl` crate (MIT/Apache-2.0),
unmodified.

All local changes relative to upstream are logged in `UPSTREAM-CHANGES.md`.
