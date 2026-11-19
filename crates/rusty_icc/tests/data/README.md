# Test profiles

Repository-only test data; excluded from the published crate by the
`include` list in `Cargo.toml`.

| file | what it exercises | source | licence |
|---|---|---|---|
| `CGATS001Compat-v2-micro.icc` | v2 CMYK, `A2B0` `lut16`, Lab PCS | [saucecontrol/Compact-ICC-Profiles](https://github.com/saucecontrol/Compact-ICC-Profiles) (via the `hayro-jpeg2000` 0.3.5 crate) | CC0 1.0 |
| `ProPhoto-v2-micro.icc` | v2 matrix/TRC, sampled `curv` | saucecontrol/Compact-ICC-Profiles (via `hayro-jpeg2000`) | CC0 1.0 |
| `LAB.icc` | v4 abstract Lab identity (`mAB` curves only), `chad` | made with LittleCMS by the `hayro-jpeg2000` authors | CC0 1.0 |
| `sRGB-v4.icc`, `sGrey-v4.icc` | v4 `para` curves, `mluc` | saucecontrol/Compact-ICC-Profiles (via the `krilla` 0.8.2 crate) | CC0 1.0 |
| `sRGB-v2-magic.icc`, `sGrey-v2-magic.icc` | v2 `curv`, `desc`/`text` | saucecontrol/Compact-ICC-Profiles (via `krilla`) | CC0 1.0 |
| `sRGB_v4_ICC_preference.icc` | v4 `mAB`/`mBA` with 17^3 CLUTs, Lab PCS, `A2B0`/`A2B1`/`B2A0`/`B2A1` | International Color Consortium (via the `pdf-writer` 0.15 crate's examples) | ICC profile terms: may be copied, distributed, embedded, made, used and sold without restriction |

Coated FOGRA39 is used by the oracle and proofing tests when available but is
**not** committed: Adobe's bundling licence forbids modification and limits
redistribution. Point `RUSTY_ICC_FOGRA39` at a copy, or the tests find the one
the `printpdf` crate ships if it is in the local cargo registry; without it
those tests print a skip line.

The camera-style LUT input profiles (`lut16` Lab and `mAB` XYZ) are generated
by `tests/common/mod.rs` with this crate's own encoder.
