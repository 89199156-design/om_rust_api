#!/usr/bin/env bash
set -euo pipefail

scope="${1:?usage: run_scope.sh gfs|cams}"
case "$scope" in
  gfs|cams) ;;
  *) echo "invalid scope: $scope" >&2; exit 2 ;;
esac

app_dir="${OM_WEBP_APP_DIR:-/opt/1panel/apps/weather_om_webp}"
data_root="${OM_DATA_ROOT:-/data/om_raw}"
output_root="${OM_WEBP_DATA_ROOT:-$app_dir/data}"
public_root="${OM_WEBP_PUBLIC_ROOT:-/opt/1panel/apps/weather/data}"
decoder_lib="${OM_OMFILE_LIB:-/opt/1panel/apps/weather_om_api/native/libomfileformat.so}"
workers="${OM_WEBP_WORKERS:-2}"
frames="${OM_WEBP_FRAMES:-121}"

exec "$app_dir/bin/om-webp" \
  --scope "$scope" \
  --data-root "$data_root" \
  --output-root "$output_root" \
  --public-root "$public_root" \
  --decoder-lib "$decoder_lib" \
  --workers "$workers" \
  --frames "$frames"
