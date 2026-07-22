#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="${1:-/opt/1panel/apps/weather_om_webp}"
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
if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required to build the WebP production binaries" >&2
  exit 1
fi
if ! command -v sha256sum >/dev/null 2>&1; then
  echo "sha256sum is required to record the deployed binary identity" >&2
  exit 1
fi
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

install -d -m 0755 "$BIN_DIR" "$INSTALL_DIR/data/current" \
  "$INSTALL_DIR/data/logs" "$INSTALL_DIR/data/releases" \
  "$INSTALL_DIR/data/staging"
for binary in "${BINARIES[@]}"; do
  build_binary="$BUILD_TARGET_DIR/release/$binary"
  if [[ ! -f "$build_binary" ]]; then
    echo "cargo build completed but binary is missing: $build_binary" >&2
    exit 1
  fi
  install -m 0755 -- "$build_binary" "$BIN_DIR/$binary"
done

ln -sfn -- "$REPOSITORY_ROOT/webp" "$INSTALL_DIR/source"
ln -sfn -- "$REPOSITORY_ROOT/webp/om_webp/scripts" "$INSTALL_DIR/scripts"
ln -sfn -- "$REPOSITORY_ROOT/webp/om_webp/README.md" "$INSTALL_DIR/README.md"

REVISION_TMP="$(mktemp "$INSTALL_DIR/.source-revision.tmp.XXXXXX")"
BUILD_INFO_TMP="$(mktemp "$BIN_DIR/.om-webp.build-info.tmp.XXXXXX")"
cleanup_metadata_tmp() {
  rm -f -- "$REVISION_TMP" "$BUILD_INFO_TMP"
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
chmod 0644 "$REVISION_TMP" "$BUILD_INFO_TMP"
mv -f -- "$REVISION_TMP" "$SOURCE_REVISION_FILE"
mv -f -- "$BUILD_INFO_TMP" "$BUILD_INFO_FILE"
REVISION_TMP=""
BUILD_INFO_TMP=""
trap - EXIT

echo "installed=$INSTALL_DIR"
echo "source_revision=$SOURCE_REVISION"
echo "source_revision_file=$SOURCE_REVISION_FILE"
