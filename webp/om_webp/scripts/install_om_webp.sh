#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="${1:-/opt/1panel/apps/weather_om_webp}"
DATA_DIR="${OM_WEBP_DATA_ROOT:-/data/om_webp}"
INSTALL_OWNER="${OM_WEBP_USER:-ubuntu}"
STRICT_DATA_ROOT="${OM_STRICT_DATA_ROOT:-/data}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd -P)"
BUILD_TARGET_DIR="${OM_WEBP_CARGO_TARGET_DIR:-$REPOSITORY_ROOT/target}"
BIN_DIR="$INSTALL_DIR/bin"
SOURCE_REVISION_FILE="$INSTALL_DIR/source-revision"
BUILD_INFO_FILE="$BIN_DIR/om-webp.build-info"
BINARIES=(om-webp om-grid-verify om-webp-api-verify om-webp-inspect)

if [[ "$BUILD_TARGET_DIR" != /* ]]; then
  echo "OM_WEBP_CARGO_TARGET_DIR must be an absolute path: $BUILD_TARGET_DIR" >&2
  exit 2
fi
if [[ "$DATA_DIR" != /* ]]; then
  echo "OM_WEBP_DATA_ROOT must be an absolute path: $DATA_DIR" >&2
  exit 2
fi
if ! mountpoint -q -- "$STRICT_DATA_ROOT"; then
  echo "strict data root is not mounted: $STRICT_DATA_ROOT" >&2
  exit 1
fi
if [ "$(stat -c %d /)" = "$(stat -c %d "$STRICT_DATA_ROOT")" ]; then
  echo "strict data root shares the system filesystem: $STRICT_DATA_ROOT" >&2
  exit 1
fi
strict_real="$(readlink -f -- "$STRICT_DATA_ROOT")"
data_real="$(readlink -m -- "$DATA_DIR")"
data_ancestor="$DATA_DIR"
while [ ! -e "$data_ancestor" ]; do
  next_ancestor="$(dirname -- "$data_ancestor")"
  if [ "$next_ancestor" = "$data_ancestor" ]; then
    echo "WebP data path has no existing ancestor: $DATA_DIR" >&2
    exit 1
  fi
  data_ancestor="$next_ancestor"
done
case "$data_real" in
  "$strict_real"|"$strict_real"/*) ;;
  *)
    if [ "$(stat -c %d "$data_ancestor")" != "$(stat -c %d "$strict_real")" ]; then
      echo "WebP data path is not on the strict data filesystem: $DATA_DIR" >&2
      exit 1
    fi
    ;;
esac
if ! id "$INSTALL_OWNER" >/dev/null 2>&1; then
  echo "WebP service owner does not exist: $INSTALL_OWNER" >&2
  exit 1
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required to build the WebP production binaries" >&2
  exit 1
fi
if ! command -v sha256sum >/dev/null 2>&1; then
  echo "sha256sum is required to record the deployed binary identity" >&2
  exit 1
fi

SUDO=""
if [[ "$(id -u)" -ne 0 ]]; then
  SUDO="sudo"
fi
run_privileged() {
  if [[ -n "$SUDO" ]]; then
    "$SUDO" "$@"
  else
    "$@"
  fi
}
if ! git -C "$REPOSITORY_ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "WebP production deployment requires a Git worktree: $REPOSITORY_ROOT" >&2
  exit 1
fi

SOURCE_REVISION="$(git -C "$REPOSITORY_ROOT" rev-parse --verify 'HEAD^{commit}')"
WORKTREE_STATUS="$(git -C "$REPOSITORY_ROOT" status --porcelain=v1 --untracked-files=all)"
if [[ -n "$WORKTREE_STATUS" ]]; then
  echo "refusing to deploy from a dirty source worktree: $REPOSITORY_ROOT" >&2
  printf '%s\n' "$WORKTREE_STATUS" >&2
  exit 1
fi
if [[ ! "$SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]]; then
  echo "source revision must be a full lowercase 40-character Git SHA" >&2
  exit 1
fi

cargo build --release \
  --manifest-path "$REPOSITORY_ROOT/webp/om_webp/Cargo.toml" \
  --target-dir "$BUILD_TARGET_DIR" \
  --bin om-webp \
  --bin om-grid-verify \
  --bin om-webp-api-verify \
  --bin om-webp-inspect

run_privileged install -d -m 0755 "$BIN_DIR" "$INSTALL_DIR/logs"
run_privileged install -d -o "$INSTALL_OWNER" -g "$INSTALL_OWNER" -m 0775 \
  "$DATA_DIR" "$DATA_DIR/current" "$DATA_DIR/releases" "$DATA_DIR/staging"
run_privileged chown -R "$INSTALL_OWNER:$INSTALL_OWNER" "$INSTALL_DIR/logs"
for binary in "${BINARIES[@]}"; do
  build_binary="$BUILD_TARGET_DIR/release/$binary"
  if [[ ! -f "$build_binary" ]]; then
    echo "cargo build completed but binary is missing: $build_binary" >&2
    exit 1
  fi
  run_privileged install -m 0755 -- "$build_binary" "$BIN_DIR/$binary"
done

run_privileged ln -sfn -- "$REPOSITORY_ROOT/webp" "$INSTALL_DIR/source"
run_privileged ln -sfn -- \
  "$REPOSITORY_ROOT/webp/om_webp/scripts" "$INSTALL_DIR/scripts"
run_privileged ln -sfn -- \
  "$REPOSITORY_ROOT/webp/om_webp/README.md" "$INSTALL_DIR/README.md"

REVISION_TMP="$(mktemp)"
BUILD_INFO_TMP="$(mktemp)"
REVISION_STAGED="$INSTALL_DIR/.source-revision.tmp.$$"
BUILD_INFO_STAGED="$BIN_DIR/.om-webp.build-info.tmp.$$"
cleanup_metadata_tmp() {
  rm -f -- "$REVISION_TMP" "$BUILD_INFO_TMP"
  run_privileged rm -f -- "$REVISION_STAGED" "$BUILD_INFO_STAGED"
}
trap cleanup_metadata_tmp EXIT
printf '%s\n' "$SOURCE_REVISION" > "$REVISION_TMP"
{
  printf 'git_revision=%s\n' "$SOURCE_REVISION"
  for binary in "${BINARIES[@]}"; do
    printf '%s_sha256=%s\n' \
      "${binary//-/_}" \
      "$(sha256sum -- "$BIN_DIR/$binary" | awk '{print $1}')"
  done
  printf 'built_at_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$BUILD_INFO_TMP"
run_privileged install -m 0644 -- "$REVISION_TMP" "$REVISION_STAGED"
run_privileged install -m 0644 -- "$BUILD_INFO_TMP" "$BUILD_INFO_STAGED"
run_privileged mv -f -- "$REVISION_STAGED" "$SOURCE_REVISION_FILE"
run_privileged mv -f -- "$BUILD_INFO_STAGED" "$BUILD_INFO_FILE"
REVISION_STAGED=""
BUILD_INFO_STAGED=""
rm -f -- "$REVISION_TMP" "$BUILD_INFO_TMP"
REVISION_TMP=""
BUILD_INFO_TMP=""
trap - EXIT

echo "installed=$INSTALL_DIR"
echo "data=$DATA_DIR"
echo "source_revision=$SOURCE_REVISION"
echo "source_revision_file=$SOURCE_REVISION_FILE"
