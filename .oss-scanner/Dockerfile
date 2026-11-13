# Scanner image for https://github.com/Remade-With-Rust/remade_ffmpeg_rs (main), for Anthropic's OSS Scanner
# (https://github.com/anthropics/oss-scanner).
#
# The scanner clones this repository and builds this file with the checkout as the build context. The audit that
# follows runs with NO network, so every crate, toolchain and test vector is fetched here. What the image leaves
# behind, and how to drive it, is described in .oss-scanner/threat_model.md.
#
# What gets built (in /src/target, dev profile at opt-level 1: debug assertions and overflow checks stay on, so an
# arithmetic overflow on hostile input panics instead of wrapping silently):
#   - the default workspace members (the engine, every demuxer/decoder/encoder crate, the rff/rffprobe/ffmpeg/ffprobe
#     CLIs, rff-server) with their bins, examples and every test binary;
#   - the 15 libFuzzer targets under crates/*/fuzz, built with cargo-fuzz (AddressSanitizer, debug assertions).
# Left out on purpose, as in CI: rff-ui (Dioxus front-end, needs a webview toolchain, never shipped) and
# rff-codec-openh264 (opt-in C/FFI shim, not a default member).

# The exact toolchain rust-toolchain.toml pins, so nothing is downloaded at first use.
FROM rust:1.95.0-bookworm

ENV DEBIAN_FRONTEND=noninteractive
# nasm: the h264/AV1 assembly paths in the default build. clang + gdb: for reproducing crashes.
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates build-essential clang nasm git pkg-config python3 curl gdb \
    && rm -rf /var/lib/apt/lists/*

# A dated nightly for cargo-fuzz (libFuzzer + AddressSanitizer need -Z flags). Dated, so a nightly regression cannot
# break the scan build. Bump deliberately.
ARG FUZZ_NIGHTLY=nightly-2026-10-01
ARG CARGO_FUZZ_VERSION=0.13.2
RUN rustup component add rustfmt clippy \
    && rustup toolchain install "${FUZZ_NIGHTLY}" --profile minimal --component rust-src \
    && cargo +"${FUZZ_NIGHTLY}" install cargo-fuzz --locked --version "${CARGO_FUZZ_VERSION}"

# scanner contract: the checkout lives inside the image, at /src
COPY . /src
WORKDIR /src

# Line tables only, and no incremental cache: ~170 test binaries each link the whole engine, and full DWARF plus the
# incremental cache made a 27 GB image whose export alone took 12 minutes of the 45-minute build budget. Backtraces and
# gdb still resolve file:line.
ENV RUST_BACKTRACE=1 \
    CARGO_TERM_COLOR=never \
    FUZZ_NIGHTLY=${FUZZ_NIGHTLY} \
    CARGO_PROFILE_DEV_OPT_LEVEL=1 \
    CARGO_PROFILE_DEV_DEBUG=line-tables-only \
    CARGO_INCREMENTAL=0

# Every crate in the workspace lock (all targets, all features), plus each fuzz crate's own lock (they are separate
# workspaces so libfuzzer-sys never enters the normal build). The fuzz locks are not --locked: they trail the workspace's
# version bumps, and cargo only has to re-point the path dependencies, which it does here while the network is up.
RUN cargo fetch --locked \
    && for d in crates/*/fuzz; do cargo fetch --manifest-path "$d/Cargo.toml" || exit 1; done

# Conformance vectors the tests read but the repository does not vendor:
#   /src/vp9-vectors          libvpx VP9 vectors  -> crates/rff/tests/vp9_conformance.rs (VP9_VECTORS_DIR)
#   /opt/mp3-vectors/vectors  ISO MP3 vectors     -> crates/rusty_mp3/tests/iso_vectors.rs (MP3_ISO_VECTORS)
RUN bash scripts/fetch-vp9-vectors.sh /src/vp9-vectors \
    && sh scripts/fetch-mp3-vectors.sh /opt/mp3-vectors

# Everything is fetched. From here on cargo must not reach a registry.
ENV CARGO_NET_OFFLINE=true \
    VP9_VECTORS_DIR=/src/vp9-vectors \
    MP3_ISO_VECTORS=/opt/mp3-vectors/vectors

# The default members, as CI builds them: libraries, binaries, examples and every test binary. A step that fails fails
# the build, and primary_contact gets the log.
RUN cargo build --locked --bins --examples \
    && cargo test --locked --no-run

# The libFuzzer targets: crates/rff/fuzz (demux, decode across every format), the AAC and MP3 codec adapters, and the
# rusty_aac / rusty_mp3 cores. Binaries land in crates/<crate>/fuzz/target/x86_64-unknown-linux-gnu/release/<target>.
RUN for d in crates/*/fuzz; do \
      (cd "$(dirname "$d")" && cargo +"${FUZZ_NIGHTLY}" fuzz build -O --debug-assertions) || exit 1; \
    done

ENV ASAN_OPTIONS=allocator_may_return_null=1:abort_on_error=1:detect_leaks=1:symbolize=1

# Run the test suite so the log shows the image works, without letting a failure abort the build.
RUN timeout 1200 cargo test --locked --no-fail-fast \
    || echo "WARNING: cargo test reported failures (or hit the time limit)"
