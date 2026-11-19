#!/usr/bin/env bash
# Build the comparison corpus: ImageMagick 7 (which writes through libtiff)
# across compression x type x depth x layout, plus libtiff-written CCITT,
# JPEG and predictor files from `tiffvs gen`.
# usage: gen_corpus.sh <src.ppm (any size)> <outdir> <tiffvs binary>
set -u
SRC=$1; OUT=$2; BIN=$3
mkdir -p "$OUT"
# A 517x389 crop (odd sizes exercise partial strips/tiles and row padding).
magick "$SRC" -crop 517x389+700+300 +repage "$OUT/src.ppm"
S="$OUT/src.ppm"
n=0
emit() { # name, args...
  local name=$1; shift
  if magick "$S" "$@" "$OUT/im_$name.tif" 2>/dev/null; then n=$((n+1)); fi
}
for comp in None LZW Zip RLE JPEG; do
  for t in TrueColor TrueColorAlpha Grayscale GrayscaleAlpha Palette ColorSeparation; do
    for depth in 8 16; do
      [ "$comp" = JPEG ] && [ "$depth" = 16 ] && continue
      [ "$t" = Palette ] && [ "$depth" = 16 ] && continue
      cs=""; [ "$t" = ColorSeparation ] && cs="-colorspace CMYK"
      base="$comp-$t-$depth"
      emit "$base" $cs -type $t -depth $depth -compress $comp
      emit "$base-tile" $cs -type $t -depth $depth -compress $comp -define tiff:tile-geometry=64x48
      emit "$base-msb" $cs -type $t -depth $depth -compress $comp -endian MSB
      emit "$base-rps7" $cs -type $t -depth $depth -compress $comp -define tiff:rows-per-strip=7
      if [ "$comp" = LZW ] || [ "$comp" = Zip ]; then
        emit "$base-pred2" $cs -type $t -depth $depth -compress $comp -define tiff:predictor=2
      fi
      if [ "$comp" != JPEG ] && [ "$t" != Palette ]; then
        emit "$base-planar" $cs -type $t -depth $depth -compress $comp -interlace Plane
        emit "$base-planar-tile" $cs -type $t -depth $depth -compress $comp -interlace Plane -define tiff:tile-geometry=64x64
      fi
    done
  done
  # Alpha flavours.
  emit "$comp-assoc" -type TrueColorAlpha -depth 8 -compress $comp -define tiff:alpha=associated
  emit "$comp-unassoc" -type TrueColorAlpha -depth 8 -compress $comp -define tiff:alpha=unassociated
done
# Floats (32 and 16-bit half, 64-bit), with and without predictor 3.
for comp in None LZW Zip; do
  for bits in 16 32 64; do
    for t in TrueColor Grayscale; do
      emit "float$bits-$comp-$t" -type $t -define quantum:format=floating-point -depth $bits -compress $comp
      emit "float$bits-$comp-$t-msb" -type $t -define quantum:format=floating-point -depth $bits -compress $comp -endian MSB
      if [ "$comp" != None ]; then
        emit "float$bits-$comp-$t-pred3" -type $t -define quantum:format=floating-point -depth $bits -compress $comp -define tiff:predictor=3
      fi
      emit "float$bits-$comp-$t-tile" -type $t -define quantum:format=floating-point -depth $bits -compress $comp -define tiff:tile-geometry=64x64
    done
  done
done
# Bilevel and fax.
for comp in None LZW Zip RLE Group4 Fax; do
  emit "bilevel-$comp" -monochrome -compress $comp
  emit "bilevel-$comp-msb" -monochrome -compress $comp -endian MSB
  emit "bilevel-$comp-lsbfill" -monochrome -compress $comp -define tiff:fill-order=lsb
done
# Low bit depths.
for d in 2 4; do
  emit "grey$d-none" -type Grayscale -depth $d -compress None
  emit "grey$d-lzw" -type Grayscale -depth $d -compress LZW
  emit "grey$d-tile" -type Grayscale -depth $d -compress None -define tiff:tile-geometry=64x64
  emit "pal$d" -type Palette -colors $((1<<d)) -depth $d -compress LZW
done
# Multipage: a 4-page mixed document.
if magick "$S" \( "$S" -monochrome -compress Group4 \) \( "$S" -type Grayscale -depth 16 \) \( "$S" -compress JPEG \) \
     -compress LZW "$OUT/im_multipage.tif" 2>/dev/null; then n=$((n+1)); fi
echo "ImageMagick wrote $n files"
"$BIN" gen "$S" "$OUT"
ls "$OUT"/*.tif | wc -l
