# Test streams

None of this is in the published package.

- `hayro/`: the JBIG2 streams of [hayro-jbig2](https://github.com/LaurenzV/hayro)'s
  test suite (`hayro-jbig2/tests/streams`, commit `e7ada135`), Apache-2.0 OR
  MIT, licences alongside. Data only; no hayro code is used.
- `serenity/`: the JBIG2 streams of [SerenityOS](https://github.com/SerenityOS/serenity)'s
  LibGfx tests (`Tests/LibGfx/test-inputs/jbig2`, commit `d6ef9596`),
  BSD-2-Clause, licence alongside. `annex-h.jbig2` is the test stream of T.88
  Annex H.1. `bitmap-symbol-manysyms` (384 kB) and
  `bitmap-symbol-symhuff-texthuff-manysyms` (1.5 MB) are left out for size;
  both matched when run by hand.
- `pdf/`: written by jbig2enc 0.30 (`jbig2 -s -p`, and `jbig2` for
  `p1-generic`) from pages of our own text rendered at 300 dpi.
  `digests.txt` has the digests of jbig2dec 0.20's output for them.

`manifest.txt` gives what each page must decode to. `ref:NAME` digests were
computed from the suites' own reference images (`bitmap.bmp` in SerenityOS;
`honeycomb.png`, `maze.png`, `sphere.png` and `truchet.png` in hayro). The
other digests have no reference image there (a crop of one, or the Annex H
pages) and are this decoder's output where it equals hayro-jbig2 0.4.0's.
