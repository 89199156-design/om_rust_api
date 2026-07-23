#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 --raw-root PATH [--dem-root PATH]" >&2
  exit 2
}

RAW_ROOT=""
DEM_ROOT="${OM_DEM_ROOT:-/opt/1panel/apps/weather_om_api/static}"
MODEL_STATIC_ROOT="${OM_MODEL_STATIC_ROOT:-/opt/1panel/apps/weather_om_api}"
STRICT_DATA_ROOT="${OM_STRICT_DATA_ROOT:-/data}"
MINIMUM_FREE_BYTES="${OM_DATA_MIN_FREE_BYTES:-10737418240}"
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
[ -d "$MODEL_STATIC_ROOT" ] \
  || { echo "model static root is unavailable: $MODEL_STATIC_ROOT" >&2; exit 1; }
command -v flock >/dev/null 2>&1 || { echo "flock is required" >&2; exit 1; }
command -v sha256sum >/dev/null 2>&1 || { echo "sha256sum is required" >&2; exit 1; }
mountpoint -q -- "$STRICT_DATA_ROOT" \
  || { echo "strict data root is not mounted: $STRICT_DATA_ROOT" >&2; exit 1; }
[ "$(stat -c %d /)" != "$(stat -c %d "$STRICT_DATA_ROOT")" ] \
  || { echo "strict data root shares the system filesystem: $STRICT_DATA_ROOT" >&2; exit 1; }
strict_real="$(readlink -f -- "$STRICT_DATA_ROOT")"
raw_real="$(readlink -f -- "$RAW_ROOT")"
case "$raw_real" in
  "$strict_real"|"$strict_real"/*) ;;
  *) echo "OM raw root escapes strict data root: $RAW_ROOT" >&2; exit 1 ;;
esac
[ "$(stat -c %d "$DEM_ROOT")" = "$(stat -c %d /)" ] \
  || { echo "fixed DEM root must stay on the system filesystem: $DEM_ROOT" >&2; exit 1; }
[ "$(stat -c %d "$MODEL_STATIC_ROOT")" = "$(stat -c %d /)" ] \
  || { echo "fixed model static root must stay on the system filesystem: $MODEL_STATIC_ROOT" >&2; exit 1; }
for static_contract in \
  "static/ncep_gfs013/HSURF.om:1455544:203745df4dfa10069e1a39206350e006818a0eea644bb19c1668c0f32f7475e0" \
  "static/ncep_gfs025/HSURF.om:408440:fdd9587e606e64d6d85474c703b9898669d230aac1574fc460cc3087227e868d"
do
  IFS=: read -r relative_path expected_bytes expected_sha256 <<<"$static_contract"
  static_path="$MODEL_STATIC_ROOT/$relative_path"
  [ -f "$static_path" ] \
    || { echo "fixed model elevation is unavailable: $static_path" >&2; exit 1; }
  [ "$(stat -c %s "$static_path")" = "$expected_bytes" ] \
    || { echo "fixed model elevation size mismatch: $static_path" >&2; exit 1; }
  [ "$(sha256sum "$static_path" | awk '{print $1}')" = "$expected_sha256" ] \
    || { echo "fixed model elevation checksum mismatch: $static_path" >&2; exit 1; }
done
export OM_NATIVE_MINIMUM_FREE_BYTES="$MINIMUM_FREE_BYTES"

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
  --model-static-root "$MODEL_STATIC_ROOT" \
  --producer-revision-file "$REVISION_FILE"
