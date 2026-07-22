#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 --raw-root PATH [--dem-root PATH]" >&2
  exit 2
}

RAW_ROOT=""
DEM_ROOT="${OM_DEM_ROOT:-/data/om_static}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --raw-root) RAW_ROOT="${2:-}"; shift 2 ;;
    --dem-root) DEM_ROOT="${2:-}"; shift 2 ;;
    *) usage ;;
  esac
done
[ -n "$RAW_ROOT" ] || usage

API_APP_DIR="${OM_API_APP_DIR:-/opt/1panel/apps/weather_om_api}"
MATERIALIZER="$API_APP_DIR/bin/om-native-materialize"
OMFILE_LIB="$API_APP_DIR/native/libomfileformat.so"
REVISION_FILE="$API_APP_DIR/source-revision"

[ -x "$MATERIALIZER" ] || { echo "native GFS materializer is unavailable: $MATERIALIZER" >&2; exit 1; }
[ -r "$OMFILE_LIB" ] || { echo "official OM codec is unavailable: $OMFILE_LIB" >&2; exit 1; }
[ -r "$REVISION_FILE" ] || { echo "API source revision is unavailable: $REVISION_FILE" >&2; exit 1; }
[ -d "$RAW_ROOT" ] || { echo "OM raw root is unavailable: $RAW_ROOT" >&2; exit 1; }
[ -d "$DEM_ROOT" ] || { echo "DEM root is unavailable: $DEM_ROOT" >&2; exit 1; }
command -v flock >/dev/null 2>&1 || { echo "flock is required" >&2; exit 1; }

mkdir -p "$RAW_ROOT/locks"
exec 9>"$RAW_ROOT/locks/gfs_native_materialize.lock"
if ! flock -n 9; then
  printf '%s\n' '{"group":"gfs","status":"skipped","reason":"native materialization already running"}'
  exit 0
fi

exec "$MATERIALIZER" \
  --omfile-lib "$OMFILE_LIB" \
  build-and-publish \
  --data-root "$RAW_ROOT" \
  --dem-root "$DEM_ROOT" \
  --producer-revision-file "$REVISION_FILE"
