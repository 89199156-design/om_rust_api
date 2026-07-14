#!/usr/bin/env bash
set -euo pipefail

OUT_DIR="${1:-/opt/1panel/apps/weather_om_api/native}"
SRC_DIR="${OM_FILE_FORMAT_SRC:-}"
REF="${OM_FILE_FORMAT_REF:-}"

if ! command -v python3 >/dev/null 2>&1; then
  printf '%s\n' "python3 is required to write decoder build provenance." >&2
  exit 2
fi
if [[ -z "$SRC_DIR" && -z "$REF" ]]; then
  printf '%s\n' "OM_FILE_FORMAT_REF is required when OM_FILE_FORMAT_SRC is not set." >&2
  exit 2
fi

mkdir -p "$OUT_DIR"

BUILD_DIR="$(mktemp -d)"
OUTPUT_TEMP="$OUT_DIR/.libomfileformat.so.tmp.$$"
cleanup() {
  rm -rf "$BUILD_DIR"
  rm -f "$OUTPUT_TEMP"
}
trap cleanup EXIT

if [[ -n "$SRC_DIR" ]]; then
  if [[ ! -d "$SRC_DIR/c/include" || ! -d "$SRC_DIR/c/src" ]]; then
    printf '%s\n' "OM_FILE_FORMAT_SRC is not an om-file-format source tree: $SRC_DIR" >&2
    exit 2
  fi
  if git -C "$SRC_DIR" rev-parse HEAD >/dev/null 2>&1; then
    if ! git -C "$SRC_DIR" diff --quiet || ! git -C "$SRC_DIR" diff --cached --quiet; then
      printf '%s\n' "OM_FILE_FORMAT_SRC must be a clean Git worktree." >&2
      exit 2
    fi
    SOURCE_REVISION="$(git -C "$SRC_DIR" rev-parse HEAD)"
  else
    printf '%s\n' "OM_FILE_FORMAT_SRC must be a Git worktree with a resolvable commit." >&2
    exit 2
  fi
  if [[ -n "$REF" && "$SOURCE_REVISION" != "$(git -C "$SRC_DIR" rev-parse "$REF^{commit}")" ]]; then
    printf '%s\n' "OM_FILE_FORMAT_SRC HEAD does not match OM_FILE_FORMAT_REF." >&2
    exit 2
  fi
  SOURCE_MODE="local-clean-worktree"
  cp -a "$SRC_DIR" "$BUILD_DIR/om-file-format"
else
  mkdir -p "$BUILD_DIR/om-file-format"
  git -C "$BUILD_DIR/om-file-format" init --quiet
  git -C "$BUILD_DIR/om-file-format" remote add origin https://github.com/open-meteo/om-file-format.git
  # Fetch the pinned object explicitly. Open-Meteo may pin a valid commit that
  # is no longer reachable from the repository's current default branch.
  git -C "$BUILD_DIR/om-file-format" fetch --depth 1 origin "$REF"
  git -C "$BUILD_DIR/om-file-format" checkout --detach FETCH_HEAD
  SOURCE_REVISION="$(git -C "$BUILD_DIR/om-file-format" rev-parse HEAD)"
  SOURCE_MODE="official-git-clone"
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

BUILD_JOBS="${OM_FILE_FORMAT_BUILD_JOBS:-$(nproc)}"
if [[ ! "$BUILD_JOBS" =~ ^[1-9][0-9]*$ ]]; then
  printf '%s\n' "OM_FILE_FORMAT_BUILD_JOBS must be a positive integer." >&2
  exit 2
fi
OBJECT_DIR="$BUILD_DIR/objects"
mkdir -p "$OBJECT_DIR"
export CC_BIN="${CC:-cc}"
export INCLUDE_DIR="$SRC/c/include"
export OBJECT_DIR
export ARCH_CFLAG="${ARCH_FLAGS[*]}"
find "$SRC/c/src" -maxdepth 1 -type f -name '*.c' -print0 \
  | sort -z \
  | xargs -0 -r -n 1 -P "$BUILD_JOBS" bash -c '
      source_file="$1"
      object_file="$OBJECT_DIR/$(basename "${source_file%.c}").o"
      # ARCH_CFLAG is either empty or one compiler flag selected above.
      "$CC_BIN" -w -O3 -fPIC $ARCH_CFLAG -I "$INCLUDE_DIR" \
        -c "$source_file" -o "$object_file"
    ' _

BUILT_ARTIFACT="$BUILD_DIR/libomfileformat.so"
"$CC_BIN" -shared "$OBJECT_DIR"/*.o -lm -o "$BUILT_ARTIFACT"

ARTIFACT="$OUT_DIR/libomfileformat.so"
ARTIFACT_SHA256="$(sha256sum "$BUILT_ARTIFACT" | awk '{print $1}')"
COMPILER="$(cc --version | head -n 1)"
ARCHITECTURE="$(uname -m)"
install -m 0755 "$BUILT_ARTIFACT" "$OUTPUT_TEMP"
mv -f "$OUTPUT_TEMP" "$ARTIFACT"
python3 - \
  "$OUT_DIR/libomfileformat.build.json" \
  "$ARTIFACT_SHA256" \
  "$SOURCE_REVISION" \
  "$REF" \
  "$SOURCE_MODE" \
  "$ARCHITECTURE" \
  "$COMPILER" <<'PY'
import json
import os
import sys

path, artifact_sha256, source_revision, requested_ref, source_mode, architecture, compiler = sys.argv[1:]
payload = {
    "artifact": "libomfileformat.so",
    "artifact_sha256": artifact_sha256,
    "source_repository": "https://github.com/open-meteo/om-file-format.git",
    "source_revision": source_revision,
    "requested_ref": requested_ref,
    "source_mode": source_mode,
    "architecture": architecture,
    "compiler": compiler,
}
temporary = f"{path}.tmp.{os.getpid()}"
with open(temporary, "w", encoding="utf-8") as handle:
    json.dump(payload, handle, ensure_ascii=False, indent=2, sort_keys=True)
    handle.write("\n")
os.replace(temporary, path)
PY

printf '%s\n' "$ARTIFACT"
printf '%s\n' "$OUT_DIR/libomfileformat.build.json"
