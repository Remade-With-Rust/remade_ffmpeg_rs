# Publishing plan — the `rff-*` family to crates.io

**Goal.** Make `cargo add remade-ffmpeg` and `cargo install rff-cli` work, so downstream
apps can embed the engine without a git dependency. (A git dep is not just
inconvenient — it *blocks the consumer from publishing at all*, since crates.io
rejects any crate whose dependency graph contains a git source.)

**Scope.** 47 new `rff-*` crates, plus `rusty_jpeg` (§5) — 48 new names. The codec layer is already done — `rusty_vp9`,
`rusty_mp3`, `rusty_aac`, `rusty_vorbis`, `rusty-opus`, `rusty_h264`,
`rusty_av1e`, `rusty_av1d` are all live, and `Cargo.lock` has zero git sources.
This plan covers only the `rff-*` workspace.

**Excluded, deliberately:**

| Crate | Why |
|---|---|
| `rff-ui` | MPL-2.0 webview deps; already scoped out of the `cargo-deny` gate and never part of published binaries |

It carries an explicit `publish = false` so it can never leak into a
`--workspace` publish.

**`rff-codec-openh264` IS published**, despite being the tree's only C/FFI crate
— it has to be. `rff` declares it as an *optional* dependency (the
`h264-openh264` feature), and **crates.io must resolve optional dependencies
too**: leaving it unpublished makes `rff` itself unpublishable
(`no matching package named 'rff-codec-openh264' found`). Nobody compiles C
unless they explicitly enable that feature, and its README leads with a warning
pointing at the pure-Rust `rff-codec-h264` instead.

---

## 0. Decisions — settled ✅

Publishes are **irrevocable per version** — a wrong name or version can only be
yanked, never reused. Both decisions below are now landed in the tree.

### 0.1 Version: `0.0.1` → `0.1.0` ✅ done

`[workspace.package] version` plus all 45 `version = "0.0.1"` pins in
`[workspace.dependencies]` bumped to `0.1.0`. `0.1.0` signals pre-1.0 without
implying "not yet started" and leaves `0.0.x` free.

Found and fixed on the way: **`rff-codec-mp3` had drifted off the workspace
inheritance pattern** — it hardcoded `version`/`edition`/`license` and was
missing `repository` and `rust-version` entirely. Now inherits like every
other crate.

The `rusty_*` crates keep their independent `0.1.x` cadence — unchanged.

### 0.2 Binary names: `rff` / `rffprobe` by default ✅ done

`cargo install rff-cli` put executables named `ffmpeg` and `ffprobe` on a user's
`PATH`, shadowing a real FFmpeg install. Inside a source checkout that's a
documented compatibility choice; distributed through crates.io it is louder.

Now: `rff` and `rffprobe` are the default binaries, with the FFmpeg-compatible
names behind an opt-in `drop-in-names` feature. Same program either way —
`src/bin/rff.rs` and `src/bin/ffmpeg.rs` are both thin shims over
`rff_cli::ffmpeg::run` (two targets cannot share one source file without a Cargo
warning, hence the separate shims).

```sh
cargo install rff-cli                            # rff, rffprobe
cargo install rff-cli --features drop-in-names   # + ffmpeg, ffprobe
```

**Blast radius, all fixed:** the rename broke four call sites that referenced the
built `ffmpeg.exe` — `.github/workflows/release.yml` (now builds with
`drop-in-names` and archives both name pairs, since a release download *is* the
drop-in distribution), `Prometheus/crates/prom-trial/src/tools.rs` (now prefers
`rff` and falls back to `ffmpeg`), `tools/quality/corpus_eval.sh`, and the root
README's Install + Quick start sections.

### 0.3 Name squatting check — ⚠️ PARTLY OVERTAKEN, 2026-08-27

The original check found `rff`, `rff-core`, `rff-codec`, `rff-format`,
`rff-cli`, `rff-io`, `rff-server` all unclaimed. That is **no longer true of
the bare name**: `rff` on crates.io is now **owned by Andrew Stewart
(`stewart`), at 0.3.0** — an unrelated crate. We never published it, and the
window closed while the rest of the family went out.

The `rff-*` prefix itself is safe: `rff-core`, `rff-codec`, `rff-format` and
every `rff-format-*` / `rff-codec-*` published so far are owned by
`Ttimmahlax`. `rff-cli`, `rff-server` and `rff-targets` are still free.

**Resolution:** the facade crate is published as **`remade-ffmpeg`**, keeping
`[lib] name = "rff"` so downstream code is unchanged — `use rff::...` still
compiles, only the dependency line differs:

```toml
remade-ffmpeg = "0.2"
```

Alternatives checked and free at the time of the decision: `remade-ffmpeg`,
`remade_ffmpeg`, `rff-engine`. (`rffmpeg` is taken by `nrbnlulu`.)

**Lesson:** a name-availability check has a shelf life. Claim the flagship name
first, not last — the publish order put `rff` in wave 4, behind 42 other
crates, and that delay is exactly what cost it.

---

## 1. Pre-flight ✅ done (all of it independent of the upload)

### 1.1 READMEs — 47 of them ✅ done

No `rff-*` crate had a README; all four `rusty_*` crates did. crates.io
renders the README as the crate's landing page, so a crate without one shows a
bare description line.

Template is [`crates/rusty_mp3/README.md`](../crates/rusty_mp3/README.md) —
six sections:

1. `# <crate-name>` + the three badges (Remade With Rust / Mata Network / Apache-2.0)
2. One paragraph: what it is, pure-Rust/no-FFI, licence
3. Capability bullets (what's implemented, what's measured, known gaps stated plainly)
4. A compiling usage example
5. `## Part of Remade With Rust` — boilerplate, byte-identical across every crate
6. `## License`

Sections 1, 5 and 6 are mechanical. Sections 2–4 fall into **five archetypes**,
which is what makes 46 tractable:

| Archetype | Count | Example shows |
|---|---:|---|
| Foundation (`rff-core`, `rff-codec`, `rff-format`, `rff-io`, `rff-subtitle`) | 5 | the trait being implemented |
| Codec adapter (`rff-codec-*`) | 15 | `register()` + the knobs, linking to the standalone `rusty_*` crate where one exists |
| Format adapter (`rff-format-*`) | 21 | demux loop / mux write |
| Engine (`rff`, `rff-filter`, `rff-resample`, `rff-auth`) | 4 | transcode via the facade |
| Front-end (`rff-cli`, `rff-server`) | 2 | shell commands, not Rust |

**Every example was compiled, not eyeballed.** All 44 Rust blocks were extracted
into a scratch crate with path dependencies on all 46 crates and built. The first
pass had **seven real errors** — proof the check was worth running:

| README | Wrong | Actual API |
|---|---|---|
| `rff-core` | `AudioFrame { samples: vec![0.0; n] }` | `samples` is a `usize` count *per channel*; the bytes live in `planes: Vec<Vec<u8>>` |
| `rff-format`, ×20 adapters | `demuxer.streams()` | `demuxer.read_header()? -> Vec<Stream>` |
| `rff-subtitle` | `cue.start` / `cue.end` | `cue.start_ms` / `cue.end_ms` |
| `rff-auth` | `authenticate(Some("..."))` | `authenticate(&str)` |
| `rff-filter` | `VideoFrame { ..Default::default() }` | `VideoFrame` has no `Default`; `planes` + `strides` are required |
| **`rff-format-hls`** | `rff_format_hls::register(...)` | **there is no `register`** — see below |

That last one is a genuine documentation finding, not just a typo: **HLS is not a
`FormatRegistry` format.** It is the one `rff-format-*` crate with no `register()`
(and it is absent from `rff`'s `register_builtin_formats`), because segmenting
needs to own an output *directory*, not a single byte sink — so it is driven
directly via `HlsSegmenter`, which implements `Muxer`. Its README now says so.

### 1.2 Manifest metadata ✅ done

Only the four `rusty_*` crates carried discoverability metadata. All 46 `rff-*`
crates now match that shape (see `crates/rusty_mp3/Cargo.toml`):

```toml
homepage    = "https://github.com/Remade-With-Rust/remade_ffmpeg_rs"
readme      = "README.md"
keywords    = [...]   # max 5, max 20 chars each
categories  = [...]   # must match crates.io's fixed slug list exactly
```

`description`, `license`, `repository` and `rust-version` are already present on
every crate — that part is done.

Categories worth using: `multimedia`, `multimedia::audio`, `multimedia::video`,
`multimedia::encoding`, `encoding`, `command-line-utilities` (for `rff-cli`),
`web-programming::http-server` (for `rff-server`), `parser-implementations` (the
containers) and `authentication` (`rff-auth`). An invalid slug is a hard publish
error; the generator asserts the keyword rules (≤ 5 keywords, ≤ 20 chars each,
alphanumeric-leading) before writing.

### 1.3 Package hygiene ✅ done

> **`cargo package -p <one-crate>` does not work here** and that is not a bug:
> verification resolves dependencies from the registry, so any crate with an
> unpublished sibling fails with `no matching package named 'rff-core' found`.
> Use `--workspace`, which co-packages and resolves siblings locally. Note you
> cannot `--exclude rusty_jpeg` either — `rff-codec-jpeg` depends on it (§5):
>
> ```sh
> cargo package --workspace --exclude rff-ui --allow-dirty
> ```
>
> `--list` on a single crate still works fine for inspecting contents.

```sh
cargo package -p <crate> --list       # what actually ships (works standalone)
cargo package --workspace ...         # builds every .crate and verifies each compiles from its tarball
```

Watch for:

- **Test fixtures / corpora sneaking in.** The repo has `corpus/`,
  `video-tests/`, `vp9-vectors/` at the root, so they're outside the package
  roots — but check each crate's own `tests/` and any generated artifacts.
  Add `exclude = [...]` where needed (`rusty_mp3` already does this for
  `lab-results/`).
- **The 10 MB per-crate limit.**
- **`rff` and `rff-codec-vp9` have `dev-dependencies`.** `rff-codec-vp9`'s are
  path deps (`rff-core`, `rff-codec`) for its benchmark — those resolve through
  the workspace versions and are fine, but they must be published *before* it,
  which the wave order already handles.

**Result:** `cargo package --workspace --exclude rff-ui --allow-dirty` produces
**52 `.crate` files** (the 48 publishable + the 4 already-live `rusty_*`) and
verifies each one *compiles from its own tarball* — zero errors. Largest is
`rusty_vp9` at 324 KB, far under the 10 MB ceiling. Every README is included in
its package (confirmed via `--list`).

### 1.4 Full verification pass

```sh
cargo build --workspace --exclude rff-ui
cargo test  --workspace --exclude rff-ui
cargo build --no-default-features      # the pure-Rust, no-nasm path
cargo deny check                       # the no-copyleft gate
```

The published artefact is what compiles *from the tarball*, not from the
worktree — `cargo package` (not just `cargo build`) is the gate that catches a
file you forgot to include.

---

## 2. The rate limit — this is the schedule driver

From the crates.io server source (`src/rate_limiter.rs`):

| Action | Burst | Refill |
|---|---:|---|
| **PublishNew** (a name never seen before) | **5** | **1 per 10 minutes** |
| PublishUpdate (new version of an existing crate) | 30 | 1 per minute |

All 48 are *new* names, so every one draws on the PublishNew bucket.

**Unmitigated cost:** 5 immediately, then 43 × 10 min = **430 minutes ≈ 7 h 10 m**
of babysat, sequential uploading. The bucket is currently full (last publish was
2026-07-29; 5 tokens refill in 50 minutes).

### Path A — request a limit increase (recommended)

Email **help@crates.io** ahead of time: state that you're publishing a 46-crate
workspace split of a single project, list the crate names, and give the
repository URL. This is a routine, frequently granted request — a workspace
being split into per-codec crates is exactly the case the exemption exists for.

With the increase, the whole run collapses to one command:

```sh
cargo publish --workspace --exclude rff-ui
```

Cargo 1.95 (the pinned toolchain) resolves the dependency order itself and waits
for index propagation between packages. **Send the email before doing the README
work** — the lead time overlaps with step 1 for free.

### Path B — drip-feed (fallback if the request is declined or slow)

A driver that publishes one crate at a time in wave order, sleeps 10 minutes
between uploads, treats "crate version already uploaded" as success (so the
script is resumable after any interruption), and aborts loudly on anything else.
~7 hours unattended. Write it to log each success so a resume starts from the
right place.

Do **not** use `cargo publish --workspace` for this path: it aborts the whole
run on a 429 and does not back off.

---

## 3. Publish order

Computed from the actual manifests. Each wave depends only on earlier waves, so
within a wave the order is free.

| Wave | n | Crates |
|---:|---:|---|
| 0 | 4 + `rusty_jpeg` | `rff-core`, `rff-auth`, `rff-resample`, `rff-subtitle` — **plus `rusty_jpeg`**, see §5: `rff-codec-jpeg` now depends on it, so it is a hard prerequisite, not an independent side crate |
| 1 | 4 | `rff-codec`, `rff-format`, `rff-filter`, `rff-io` |
| 2 | 35 | all 16 `rff-codec-*` (including openh264) + 19 `rff-format-*` (minus hls) |
| 3 | 1 | `rff-format-hls` (needs `rff-format-ts`) |
| 4 | 1 | `rff` (depends on all 42 above, openh264 included as an optional dep) |
| 5 | 2 | `rff-cli`, `rff-server` |

**Wave 0 first crate = `rff-core`.** It has no local dependencies and claims the
`rff-` prefix. If anything is going to go wrong with metadata or packaging, it
goes wrong there, cheaply.

Note the shape: wave 2 is 35 of the 48. Under Path B that single wave is 5.5 of
the 7 hours; under Path A it's a few minutes.

---

## 4. Post-publish

1. **Verify a clean consumer.** In an empty directory outside the workspace:
   ```sh
   cargo new /tmp/rff-smoke && cd /tmp/rff-smoke
   cargo add remade-ffmpeg && cargo build
   cargo install rff-cli && rff -codecs
   ```
   This is the only test that proves the published graph resolves — the
   workspace's own build always passes via path deps and will happily hide a
   missing version pin.
2. **Update the root README `Install` section** — replace
   `cargo install --path crates/rff-cli` with `cargo install rff-cli`, and add
   `cargo add remade-ffmpeg` for the library path.
3. **Tag the release** `v0.1.0` and cut a GitHub Release; the README already
   promises prebuilt binaries there.
4. **`docs.rs`** builds automatically, with **default features**. `rff`'s
   default set includes `h264-asm`, which assembles `rusty_h264`'s vendored SIMD
   kernels with `nasm` — and docs.rs does not provide `nasm`, so the docs build
   would fail. Already fixed in the tree: `rff` and `rff-cli` carry

   ```toml
   [package.metadata.docs.rs]
   no-default-features = true
   ```

   (`rff` also adds `features = ["https"]` so the TLS input path is documented).
   Note `rff-codec-h264` does **not** need this — its own `asm` feature is
   non-default, so it builds clean on docs.rs as-is.

5. **Bump the stale `rusty_h264` pin.** `crates/rff-codec-h264` requests
   `rusty_h264 = "0.2"` and resolves to **0.2.1**, while **0.5.1** is current on
   crates.io. Not a publish blocker — 0.2.1 is still on the index — but shipping
   `rff` 0.1.0 against a codec three minor versions behind is worth a deliberate
   look before, not after, the upload.

6. **Refresh the yanked transitive dep.** `Cargo.lock` pins **`spin` 0.9.8**,
   which is **yanked** upstream. It arrives only through the opt-in `https`
   feature (`rustls-rustcrypto → rsa → num-bigint-dig → lazy_static → spin`).
   Harmless for resolution, but `cargo package` warns about it on every crate.

---

## 5. `rusty_jpeg` — SHIPPED (0.1.2), and the one crate with its own repo

> **Status update.** `rusty_jpeg` is live on crates.io (0.1.0 → 0.1.2) and now
> has a **dedicated public repo**,
> [Remade-With-Rust/rusty_jpeg](https://github.com/Remade-With-Rust/rusty_jpeg),
> which its `repository`/`homepage` point at. It is the only in-house codec
> crate not homed on the monorepo — the others still point at
> `remade_ffmpeg_rs`.
>
> **`crates/rusty_jpeg/` remains the source of truth.** The dedicated repo is a
> mirror, refreshed by `scripts/sync-rusty-jpeg-mirror.sh`; never edit it
> directly. The script re-runs the crate's tests *outside* the workspace before
> committing, because a manifest leaning on workspace inheritance would only
> fail once it was public.
>
> The wave-0 ordering below still holds: `rff-codec-jpeg` consumes `rusty_jpeg`,
> so it must be on the index first — which it now is.

### Original plan (kept for context)


A `crates/rusty_jpeg/` appeared in the tree during this planning work: a
**vendored merge** of `jpeg-decoder` 0.3.2 and `jpeg-encoder` 0.7.0 into one
codec, with the encoder's AVX2 kernels (dead upstream behind a non-default
feature) turned on by default and the encoder gated against the decoder as a
round-trip oracle.

**It is on the critical path, not off to one side.** `crates/rff-codec-jpeg` has
been switched to consume it, so `rusty_jpeg` must be published *before* that
crate — a `cargo package --workspace --exclude rusty_jpeg` fails outright with
`no matching package named 'rusty_jpeg' found`. Treat it as a wave-0 crate.

Notes for whoever ships it:

- Its manifest declared `readme = "README.md"` with **no such file** — a hard
  publish error. A README now exists, aligned to the `rusty_mp3` template.
- Licence is `(MIT OR Apache-2.0) AND IJG`, **inherited, not chosen**. The IJG
  clause attaches to specific forward-DCT files. `NOTICE.md` names them and the
  README carries the required "based in part on the work of the Independent JPEG
  Group" acknowledgement. This is a different posture from the in-house crates
  and worth a second look before upload.
- It is not yet in `default-members`.
- It is a **new name**, so it draws on the same PublishNew bucket: 47 rather than
  46, one more 10-minute slot under Path B.

## 6. Open follow-up (not in this plan's scope)

**FLAC encode is the last in-house codec still buried in an adapter.**
[`crates/rff-codec-flac/src/encode.rs`](../crates/rff-codec-flac/src/encode.rs)
is 1,453 lines of real encoder (LPC, stereo decorrelation, partitioned Rice,
MD5) at ffmpeg parity. `rusty_flac` is an available name. Unlike Vorbis there's
no first-mover claim — `flacenc` 0.5.1 already exists — so the pitch is
ffmpeg-parity and zero dependencies, not novelty. Worth doing after the `rff-*`
family lands, not before.
