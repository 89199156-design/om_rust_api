#!/usr/bin/env bash
set -euo pipefail

SRC_DIR="${OM_FILE_FORMAT_SRC:-}"
DEFAULT_REF="71f422b2706d8a81f1cecf52ae3073990de1ddbe"
REF="${OM_FILE_FORMAT_REF:-$DEFAULT_REF}"
REQUIRED_SYMBOLS=(
  om_variable_init
  om_decoder_init
  om_decoder_init_index_read
  om_decoder_next_index_read
  om_decoder_init_data_read
  om_decoder_next_data_read
  om_decoder_read_buffer_size
  om_decoder_decode_chunks
  om_error_string
  om_encoder_init
  om_encoder_count_chunks
  om_encoder_count_chunks_in_array
  om_encoder_chunk_buffer_size
  om_encoder_compressed_chunk_buffer_size
  om_encoder_compress_chunk
  om_encoder_lut_buffer_size
  om_encoder_compress_lut
  om_header_write_size
  om_header_write
  om_trailer_size
  om_trailer_write
  om_variable_write_numeric_array_size
  om_variable_write_numeric_array
)

verify_library() {
  local library_path="$1"
  local symbol_file="$2"
  local symbol

  if [[ ! -f "$library_path" ]] || [[ ! -s "$library_path" ]]; then
    echo "native om-file-format library is missing or empty: $library_path" >&2
    return 1
  fi
  if command -v nm >/dev/null 2>&1; then
    nm -D --defined-only "$library_path" | awk 'NF >= 3 { print $3 }' \
      | sed 's/@.*$//' | sort -u > "$symbol_file"
  elif command -v readelf >/dev/null 2>&1; then
    readelf --wide --syms "$library_path" \
      | awk '$7 != "UND" && $8 != "" { print $8 }' \
      | sed 's/@.*$//' | sort -u > "$symbol_file"
  else
    echo "nm or readelf is required to verify the native om-file-format ABI." >&2
    return 1
  fi

  for symbol in "${REQUIRED_SYMBOLS[@]}"; do
    if ! grep -Fqx -- "$symbol" "$symbol_file"; then
      echo "native om-file-format ABI is missing required symbol: $symbol" >&2
      return 1
    fi
  done
}

if [[ "${1:-}" == "--verify" ]]; then
  if [[ "$#" -ne 2 ]]; then
    echo "usage: $0 --verify LIBRARY" >&2
    exit 2
  fi
  VERIFY_DIR="$(mktemp -d)"
  trap 'rm -rf -- "$VERIFY_DIR"' EXIT
  verify_library "$2" "$VERIFY_DIR/exported-symbols.txt"
  echo "verified=$2"
  exit 0
fi

OUT_DIR="${1:-/opt/1panel/apps/weather_om_api/native}"
mkdir -p "$OUT_DIR"

BUILD_DIR="$(mktemp -d)"
INSTALL_TMP=""
REVISION_TMP=""
cleanup() {
  if [[ -n "$INSTALL_TMP" ]]; then
    rm -f -- "$INSTALL_TMP"
  fi
  if [[ -n "$REVISION_TMP" ]]; then
    rm -f -- "$REVISION_TMP"
  fi
  rm -rf -- "$BUILD_DIR"
}
trap cleanup EXIT

if [[ -n "$SRC_DIR" ]]; then
  if [[ ! -d "$SRC_DIR" ]]; then
    echo "OM_FILE_FORMAT_SRC does not exist: $SRC_DIR" >&2
    exit 2
  fi
  cp -a "$SRC_DIR" "$BUILD_DIR/om-file-format"
else
  mkdir -p "$BUILD_DIR/om-file-format"
  git -C "$BUILD_DIR/om-file-format" init --quiet
  git -C "$BUILD_DIR/om-file-format" remote add origin \
    https://github.com/open-meteo/om-file-format.git
  git -C "$BUILD_DIR/om-file-format" fetch --depth=1 origin "$REF"
  git -C "$BUILD_DIR/om-file-format" checkout --quiet --detach FETCH_HEAD
fi

SRC="$BUILD_DIR/om-file-format"
if [[ ! -d "$SRC/c/include" ]] || ! compgen -G "$SRC/c/src/*.c" >/dev/null; then
  echo "om-file-format C sources are incomplete: $SRC" >&2
  exit 1
fi

SOURCE_REVISION="unversioned-local-source"
if git -C "$SRC" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  SOURCE_STATUS="$(git -C "$SRC" status --porcelain=v1 --untracked-files=all)"
  if [[ -n "$SOURCE_STATUS" ]]; then
    echo "refusing to build om-file-format from a dirty source worktree: $SRC" >&2
    printf '%s\n' "$SOURCE_STATUS" >&2
    exit 1
  fi
  SOURCE_REVISION="$(git -C "$SRC" rev-parse --verify 'HEAD^{commit}')"
elif [[ -z "$SRC_DIR" ]]; then
  SOURCE_REVISION="$REF"
fi

ARCH_FLAGS=()
case "$(uname -m)" in
  x86_64|amd64)
    ARCH_FLAGS=(-march=x86-64-v3)
    ;;
  aarch64|arm64)
    ARCH_FLAGS=(-march=armv8-a)
    ;;
esac

STAGED_LIBRARY="$BUILD_DIR/libomfileformat.so"
CC_COMMAND="${CC:-cc}"
if ! command -v "$CC_COMMAND" >/dev/null 2>&1; then
  echo "C compiler is unavailable: $CC_COMMAND" >&2
  exit 1
fi
BUILD_JOBS="${OM_FILE_FORMAT_BUILD_JOBS:-}"
if [[ -z "$BUILD_JOBS" ]]; then
  BUILD_JOBS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || printf '1')"
  if [[ -r /proc/meminfo ]]; then
    AVAILABLE_MEMORY_KIB="$(awk '$1 == "MemAvailable:" { print $2 }' /proc/meminfo)"
    if [[ "$AVAILABLE_MEMORY_KIB" =~ ^[0-9]+$ ]]; then
      MEMORY_BUILD_JOBS=$((AVAILABLE_MEMORY_KIB / 1572864))
      if ((MEMORY_BUILD_JOBS < 1)); then
        MEMORY_BUILD_JOBS=1
      fi
      if ((BUILD_JOBS > MEMORY_BUILD_JOBS)); then
        BUILD_JOBS="$MEMORY_BUILD_JOBS"
      fi
    fi
  fi
fi
if [[ ! "$BUILD_JOBS" =~ ^[1-9][0-9]*$ ]]; then
  echo "OM_FILE_FORMAT_BUILD_JOBS must be a positive integer: $BUILD_JOBS" >&2
  exit 2
fi
if ((BUILD_JOBS > 4)); then
  BUILD_JOBS=4
fi

OBJECT_DIR="$BUILD_DIR/objects"
mkdir -p "$OBJECT_DIR"
OBJECTS=()
PIDS=()
for source_file in "$SRC"/c/src/*.c; do
  object_file="$OBJECT_DIR/$(basename "${source_file%.c}").o"
  OBJECTS+=("$object_file")
  "$CC_COMMAND" -w -O3 -fPIC "${ARCH_FLAGS[@]}" \
    -I "$SRC/c/include" \
    -c "$source_file" \
    -o "$object_file" &
  PIDS+=("$!")
  if ((${#PIDS[@]} == BUILD_JOBS)); then
    batch_status=0
    for compile_pid in "${PIDS[@]}"; do
      wait "$compile_pid" || batch_status=1
    done
    if ((batch_status != 0)); then
      echo "failed to compile native om-file-format sources" >&2
      exit 1
    fi
    PIDS=()
  fi
done
batch_status=0
for compile_pid in "${PIDS[@]}"; do
  wait "$compile_pid" || batch_status=1
done
if ((batch_status != 0)); then
  echo "failed to compile native om-file-format sources" >&2
  exit 1
fi
"$CC_COMMAND" -shared "${ARCH_FLAGS[@]}" "${OBJECTS[@]}" \
  -lm \
  -o "$STAGED_LIBRARY"
verify_library "$STAGED_LIBRARY" "$BUILD_DIR/exported-symbols.txt"

INSTALL_TMP="$(mktemp "$OUT_DIR/.libomfileformat.so.tmp.XXXXXX")"
install -m 0755 -- "$STAGED_LIBRARY" "$INSTALL_TMP"
mv -f -- "$INSTALL_TMP" "$OUT_DIR/libomfileformat.so"
INSTALL_TMP=""

REVISION_TMP="$(mktemp "$OUT_DIR/.om-file-format.source-revision.tmp.XXXXXX")"
printf '%s\n' "$SOURCE_REVISION" > "$REVISION_TMP"
chmod 0644 "$REVISION_TMP"
mv -f -- "$REVISION_TMP" "$OUT_DIR/om-file-format.source-revision"
REVISION_TMP=""

echo "$OUT_DIR/libomfileformat.so"
