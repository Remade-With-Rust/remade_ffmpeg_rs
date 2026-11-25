`img.raw` is a generated 123x40 bitmap (0 = black, rows padded with zeros);
the `.fax` files are it, encoded by Ghostscript's `CCITTFaxEncode` with the
parameters in their names. See `tests/ccitt_filter.rs`.
