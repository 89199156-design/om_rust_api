#!/usr/bin/env bash
set -euo pipefail

OUT_DIR="${1:-/opt/1panel/apps/weather_om_api/native}"
SRC_DIR="${OM_FILE_FORMAT_SRC:-}"
REF="${OM_FILE_FORMAT_REF:-}"

mkdir -p "$OUT_DIR"

BUILD_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$BUILD_DIR"
}
trap cleanup EXIT

if [[ -n "$SRC_DIR" ]]; then
  cp -a "$SRC_DIR" "$BUILD_DIR/om-file-format"
else
  git clone https://github.com/open-meteo/om-file-format.git "$BUILD_DIR/om-file-format"
  if [[ -n "$REF" ]]; then
    git -C "$BUILD_DIR/om-file-format" checkout "$REF"
  fi
fi

SRC="$BUILD_DIR/om-file-format"
ARCH_FLAGS=()
case "$(uname -m)" in
  x86_64|amd64)
    ARCH_FLAGS=(-march=x86-64-v3)
    ;;
  aarch64|arm64)
    ARCH_FLAGS=(-march=armv8-a)
    ;;
esac

cc -w -O3 -fPIC -shared "${ARCH_FLAGS[@]}" \
  -I "$SRC/c/include" \
  "$SRC"/c/src/*.c \
  -lm \
  -o "$OUT_DIR/libomfileformat.so"

echo "$OUT_DIR/libomfileformat.so"
