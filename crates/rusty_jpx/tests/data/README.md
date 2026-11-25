# Test streams

Written for this crate by OpenJPEG 2.5.4's `opj_compress` from synthetic
images (sines, a step and noise), 41 x 37 samples or so. `NAME.ref` is what
OpenJPEG 2.5.4 decodes `NAME.j2k` / `NAME.jp2` to: 16-bit little-endian words,
component after component. Both are data this project generated; nothing is
taken from the conformance suite, whose files carry their own terms.

`index8` holds small values, for the palette tests that wrap it in JP2 boxes.
