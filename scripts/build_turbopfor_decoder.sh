#!/usr/bin/env bash
set -euo pipefail

# Build a small shared library exposing TurboPFor p4nddec64 for OM v3 LUT decoding.
# This script intentionally downloads a pinned om-file-format tarball instead of using git.
# Review docs/native_turbopfor.md before production use.

REVISION="${OM_FILE_FORMAT_REVISION:-71f422b2706d8a81f1cecf52ae3073990de1ddbe}"
PREFIX="${1:-/opt/1panel/apps/weather_om_downloader/native}"
WORKDIR="$(mktemp -d)"

cleanup() {
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

mkdir -p "$PREFIX"
cd "$WORKDIR"

curl -L -o om-file-format.tar.gz "https://github.com/open-meteo/om-file-format/archive/${REVISION}.tar.gz"
tar -xzf om-file-format.tar.gz
SRC_DIR="$WORKDIR/om-file-format-${REVISION}"

gcc -O3 -march=native -fPIC -shared \
  -I"$SRC_DIR/c/include" \
  "$SRC_DIR/c/src/bitpack.c" \
  "$SRC_DIR/c/src/bitpack_def.c" \
  "$SRC_DIR/c/src/bitpack_sse.c" \
  "$SRC_DIR/c/src/bitpack_avx2.c" \
  "$SRC_DIR/c/src/bitunpack.c" \
  "$SRC_DIR/c/src/bitunpack_def.c" \
  "$SRC_DIR/c/src/bitunpack_sse.c" \
  "$SRC_DIR/c/src/bitunpack_avx2.c" \
  "$SRC_DIR/c/src/bitutil.c" \
  "$SRC_DIR/c/src/vint.c" \
  "$SRC_DIR/c/src/vp4d.c" \
  "$SRC_DIR/c/src/vp4d_def.c" \
  "$SRC_DIR/c/src/vp4d_sse.c" \
  "$SRC_DIR/c/src/vp4d_avx2.c" \
  -o "$PREFIX/libom_turbopfor.so"

echo "$PREFIX/libom_turbopfor.so"
