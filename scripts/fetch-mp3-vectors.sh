#!/usr/bin/env sh
# Fetch the MPEG audio conformance vectors (ISO 11172-4 / 13818-4 Layer III,
# plus nonstandard edge cases) with reference PCM, from minimp3's repository.
# They are NOT vendored here: their redistribution terms are ISO's.
#
#   scripts/fetch-mp3-vectors.sh [dest]          (default: ./mp3-vectors)
#   MP3_ISO_VECTORS=<dest>/vectors cargo test -p rusty_mp3 --release --test iso_vectors
set -eu
dest="${1:-mp3-vectors}"
if [ -d "$dest/vectors" ]; then
    echo "already present: $dest/vectors"
    exit 0
fi
git clone --depth 1 --filter=blob:none --sparse https://github.com/lieff/minimp3.git "$dest"
git -C "$dest" sparse-checkout set vectors
echo "vectors at $dest/vectors"
