# rff-codec-aac — hardening audit

**Standard**: Remade-With-Rust recursive hardening process — see the skill's `STANDARD.md`
**Registry**: 41 gates / 12 phases (`use-protection-please` v1)
**Unit**: `crates/rff-codec-aac` — library crate (the AAC codec adapter for the remade_ffmpeg_rs registry), published on crates.io
**Tier**: critical-path — forwards untrusted packet bytes to the decoder and untrusted frames / option strings to the encoder
**Mirrors**: https://crates.io/crates/rff-codec-aac (re-renders on publish; links must be absolute) — every one of these carries the generated block and **must be
re-rendered in the same pass as this file**; a stale mirror reports a posture the unit no
longer has (SKILL.md §3.1)
**Compliance**: none — an in-memory codec adapter: no persistence, no network, no personal-data handling of its own — in-scope framework ids, or `none` with the reason
(scope triage: the skill's `COMPLIANCE.md` §1)
**Architect**: Tim Almond — accountable for this unit's security design; rendered
at the foot of the block in every README and mirror
**Audit depth**: deep
**Audited**: 2026-10-04 by Claude (for Tim Almond) · **Next review**: 2027-01-04

> Source of truth for this unit's hardening status. The README's status table is
> **generated from this file** — edit here, then run:
> `python <skills>/use-protection-please/scripts/render_readme_table.py --plan docs/plans/use-protection-please.md --readme README.md`

**Status tokens**: `Completed` (evidenced pass) · `Scheduled` (owner + date in Target) ·
`Incomplete` (not done, or not evidenced) · `N/A` (out of tier — reason required in
Evidence; excluded from the totals).

---

## Threat sketch

*Assets* — memory safety and availability of the host process; integrity of decoded PCM / encoded bitstream
*Adversaries* — a malicious media supplier (packet bytes); a malicious producer of frames and option strings
*Highest-value attack path* — a frame whose `samples` claim exceeds its buffer (was a panic / unbounded allocation — fixed); everything else is delegated to rusty_aac
*Full model* — [`docs/threat-model.md`](../threat-model.md)

---

## Checklist

`★` = v1.0.0-blocking. Full probe and pass criteria per gate: the skill's `CHECKLIST.md`.

### Phase 0 — Threat modeling

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-01 | ★ Threat model documented and linked from README | Completed | `docs/threat-model.md` (adapter surface: entry points, STRIDE, the fixed DoS); codec internals in rusty_aac's model; linked from README | |
| H-02 | Threat model revisited after last major change | Completed | dated 2026-10-04 = date of the last `src/` change | |

### Phase 1 — Toolchain

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-03 | Toolchain pinned (`rust-toolchain.toml`) | Completed | repo `rust-toolchain.toml`: channel 1.95.0, rustfmt + clippy | |
| H-04 | Committed `.cargo/config.toml` hardening defaults | Completed | repo `.cargo/config.toml` (frame pointers, full RELRO, noexecstack on Linux; Windows linker defaults verified) — see rusty_aac's audit | |
| H-05 | ★ Release profile hardened (overflow-checks, LTO, panic policy) | Incomplete | no `overflow-checks` in the release profile; shares rusty_aac's waiver (same profile, same compensating controls; Tim Almond, 2026-10-04, expires 2027-04-05) | waiver expires 2027-04-05 |
| H-06 | Security toolchain available to CI and developers | Completed | `.github/workflows/aac-hardening.yml`: pinned security toolchain, SHA-pinned actions, read-only token | |

### Phase 2 — Supply chain

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-07 | ★ `Cargo.lock` committed | Completed | workspace `Cargo.lock` tracked | |
| H-08 | ★ `deny.toml` policy present and enforced | Completed | repo `deny.toml` (advisories, licenses, bans, sources); crate-closure `cargo deny check` → advisories ok, bans ok, licenses ok, sources ok (`tools/hardening/standalone_supply_chain.sh`, run in CI) | |
| H-09 | ★ Vulnerability scan clean (`cargo audit`) | Completed | crate-closure `cargo audit --deny warnings` → clean (standalone script, run in CI and daily) | |
| H-10 | ★ `cargo vet` coverage complete | Completed | `supply-chain/` (crate-scoped store): `cargo vet --locked` → 9 fully audited (2026-10-04). First-party rff-core / rff-codec / rusty_aac trusted (owner Ttimmahlax); thiserror via dtolnay's audits | |
| H-11 | Unsafe inventory measured and trending down (geiger) | Completed | `#![forbid(unsafe_code)]`: zero unsafe in this crate; dependency unsafe tracked in rusty_aac's `UNSAFE.md` | |
| H-12 | ★ SBOM generated and published with releases | Completed | CycloneDX 1.5 SBOM of the crate closure attached to the [1.1.0 release](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/releases/tag/rff-codec-aac-v1.1.0); CI uploads it on every run | |
| H-13 | Git deps pinned; no unknown registries or sources | Completed | no git dependencies; `[sources]` denies unknown registries and git | |
| H-14 | Dependency freshness reviewed, human-in-the-loop updates | Completed | `.github/dependabot.yml` covers this crate's closure (`thiserror` via rff-core), human-merged; triage 2026-10-04: thiserror current, no advisories | |

### Phase 3 — Code level

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-15 | ★ Workspace lint policy set and clean | Completed | `[lints]` pedantic + nursery; `cargo clippy -p rff-codec-aac --all-targets --all-features --no-deps -- -D warnings` → exit 0 (2026-10-04, also in CI) | |
| H-16 | ★ `unsafe` isolated, SAFETY-commented, inventoried | Completed | `#![forbid(unsafe_code)]` at the crate root | |
| H-17 | Arithmetic safety explicit | Completed | frame arithmetic bounded by the plane length (`frames * channels * bytes <= plane.len()`); the `b` option saturates at `u32::MAX` instead of truncating (`try_from`) | |
| H-18 | ★ No `unwrap`/`expect`/panic on untrusted paths; typed errors | Completed | fixed: an empty `planes` panicked, a short plane was indexed past its end, and an inflated `samples` claim sized an allocation (62212c3) — the MP3 adapter's defect class; no `unwrap`/`expect` remains outside tests | |
| H-19 | Input validation — external bytes treated as hostile | Completed | the frame's planes are authoritative over its `samples` claim; planar input needs a plane per channel; packets and extradata go to rusty_aac's validated parsers | |
| H-20 | ★ Secrets zeroized; never logged | N/A | no key material, credentials or personal data handled | |
| H-21 | Concurrency discipline | Completed | no shared mutable state, no threads, no manual `Send`/`Sync` | |

### Phase 4 — Static analysis

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-22 | Static analysis beyond the default linter runs on every PR | Completed | `tools/hardening/aac_pattern_rules.py` runs on this crate in `.github/workflows/aac-hardening.yml`: clean | |

### Phase 5 — Dynamic analysis

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-23 | ★ Tests pass under Miri | Completed | `MIRIFLAGS=-Zmiri-deterministic-floats cargo +nightly miri test -p rff-codec-aac --lib` → 3 passed, 0 failed, 3 ignored (the real-encode tests, `cfg_attr(miri, ignore = reason)`) (2026-10-04) | |
| H-24 | Critical paths pass the sanitizers (ASan/MSan/TSan) | Completed | lib tests under ASan + LSan, TSan, MSan (`-Zbuild-std`, with rusty_aac's suite): 127 passed each, 0 reports (2026-10-04); fuzz targets run under ASan | |
| H-25 | `cargo careful test` green | Completed | `cargo +nightly careful test --release -p rusty_aac -p rff-codec-aac --lib` → 127 passed (2026-10-04) | |

### Phase 6 — Fuzzing and properties

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-26 | ★ Fuzz target per public parser, decoder, or message handler | Completed | `fuzz/`: `decode_packets` (Decoder trait, packets split anywhere, frame self-consistency asserted), `encode_frames` (Encoder trait, any frame shape, claim, plane count, option), seeded from the rusty_aac corpora; campaign 2026-10-04: 132k + 38k execs under ASan + overflow checks, 0 crashes | |
| H-27 | ★ Continuous fuzzing with no open crashes | Incomplete | first campaign 2026-10-04: 132k + 38k execs, 0 crashes; covered daily by the CI fuzzing session with rusty_aac's targets. 30 days cannot exist on day one — **waived** until 2026-11-05 (Tim Almond, 2026-10-04) | waiver expires 2026-11-05 |
| H-28 | Property tests cover the documented invariants | Completed | properties: random frame shapes never panic (200 cases), hostile frame shapes are errors or bounded (poison-checked), `b` saturates | |
| H-29 | Mutation and/or differential testing on critical modules | Completed | differential test: the adapter's decoded plane equals rusty_aac's PCM bit for bit on a real stream and 20 mutated copies (`adapter_decode_matches_rusty_aac`); rusty_aac's ISO gate runs in CI | |

### Phase 7 — Formal verification

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-30 | Proof of panic-freedom / UB-freedom per `unsafe` module | N/A | no `unsafe` module exists (`#![forbid(unsafe_code)]`) | |

### Phase 8 — Build and binary

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-31 | ★ Binary hardening applied and verified | N/A | library — ships no binary | |
| H-32 | Build is reproducible or fully auditable | N/A | `bin` tier — library ships source only | |

### Phase 9 — Runtime privilege

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-33 | Least privilege documented and tested | N/A | `bin` tier — the host owns process privilege | |

### Phase 10 — Cryptography

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-34 | Vetted crypto only; no bespoke primitives | N/A | no cryptography | |
| H-35 | Side-channel discipline (constant-time, no secret branches) | N/A | no secrets processed | |
| H-36 | Post-quantum migration plan for long-lived keys | N/A | no keys | |

### Phase 11 — CI/CD, release, and operations

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-37 | CI runs the hardening gate on every PR | Completed | `.github/workflows/aac-hardening.yml` on every change to this crate: fmt, clippy `-D warnings`, tests, pattern rules, crate-closure audit + deny + vet + SBOM, fuzz corpus replay (codec + adapter targets); daily fuzzing + advisory scan; actions SHA-pinned, read-only token. Fully green: [run 37272275669](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/actions/runs/37272275669) | |
| H-38 | Releases signed, attested, and changelogged for security | Incomplete | release tags SSH-signed; commits unsigned and no allowed-signers file; CHANGELOG.md calls out the security fix | |
| H-39 | ★ `SECURITY.md` with a coordinated disclosure process | Completed | repo-root `SECURITY.md`: private advisory channel, 5-business-day acknowledgement, coordinated disclosure; linked from README | |
| H-40 | Advisory monitoring and scheduled re-audit | Completed | owner: Tim Almond; quarterly re-audit (next 2027-01-04); daily scheduled advisory scan in `.github/workflows/aac-hardening.yml` | |
| H-41 | ★ Residual risks listed and accepted; waivers time-bounded | Completed | register below: every risk has an owner, an acceptance (Tim Almond, 2026-10-04) and a review date; both waivers are time-bounded | |

### Phase 12 — Compliance controls

Only in play when a framework is declared in scope above. With none in scope, every row is
`N/A` — reason: "no compliance framework in scope". Mapping: the skill's `COMPLIANCE.md`.

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| C-01 | Data inventory — personal/health/card data touched | N/A | no compliance framework in scope | |
| C-02 | Data-flow map including third-party egress | N/A | no compliance framework in scope | |
| C-03 | Encryption in transit for all egress | N/A | no compliance framework in scope | |
| C-04 | Encryption at rest for stored sensitive data | N/A | no compliance framework in scope | |
| C-05 | Key management — generation, storage, rotation, destruction | N/A | no compliance framework in scope | |
| C-06 | Retention limits and honoured deletion | N/A | no compliance framework in scope | |
| C-07 | Audit logging of security-relevant events | N/A | no compliance framework in scope | |
| C-08 | Log hygiene — no PII, secrets, or card data in logs | N/A | no compliance framework in scope | |
| C-09 | Least-privilege access to sensitive data | N/A | no compliance framework in scope | |
| C-10 | Subprocessor and third-party inventory | N/A | no compliance framework in scope | |
| C-11 | Incident response and breach notification path | N/A | no compliance framework in scope | |
| C-12 | Change management — reviewed, approved, traceable | N/A | no compliance framework in scope | |
| C-13 | Availability commitments and their evidence | N/A | no compliance framework in scope | |
| C-14 | Machine-readable SBOM + provenance for regulators | N/A | no compliance framework in scope | |

---

## Scheduled work

In execution order. Cheapest-first is usually correct: configuration gates clear in
minutes and unblock the outcome gates behind them.

| # | Gates | Work | Owner | Target | Notes |
|---|---|---|---|---|---|
| 1 | H-27 | Daily CI fuzzing accrues 30 days; retire the waiver | Tim Almond | 2026-11-05 | automatic once the job has run 30 days clean |
| 2 | H-05 | Revisit overflow checks with rusty_aac | Tim Almond | 2027-04-05 | |
| 3 | H-38 | Sign release tags/commits | | | needs a signing key policy |

---

## Residual risk register

Every open risk carries an owner, an acceptance, and a review date (H-41).

| ID | Risk | Likelihood | Impact | Mitigation status | Accepted by | Review date |
|---|---|---|---|---|---|---|
| R-001 | A frame shape not yet reached by fuzzing panics the encoder path (DoS) | Low | Medium | adapter fuzz targets + 200-case property; continuous fuzzing accruing (H-27) | Tim Almond, 2026-10-04 | 2027-01-04 |
| R-002 | Codec-level risks inherited from rusty_aac (see its register) | Low | Medium | tracked and accepted in rusty_aac's plan | Tim Almond, 2026-10-04 | 2027-01-04 |

---

## Waivers

Time-bounded only. An expired waiver is an `Incomplete` gate, not a `Completed` one.

| Gate | Reason | Granted by | Expires |
|---|---|---|---|
| H-05 | Shares rusty_aac's waiver: same release profile, same compensating controls. | Tim Almond, 2026-10-04 | 2027-04-05 |
| H-27 | 30 days of continuous fuzzing cannot exist on day one; daily CI fuzzing accrues it. | Tim Almond, 2026-10-04 | 2026-11-05 |

---

## Audit log

Append one line per pass; never rewrite history. The trend is the point.

| Date | Depth | Auditor | Completed / Scheduled / Incomplete | ★ met | Note |
|---|---|---|---|---|---|
| 2026-10-04 | deep | Claude | see README block | see README block | first pass for 1.1.0; found + fixed the `samples`-claim DoS (62212c3, the MP3 adapter's defect class) |
