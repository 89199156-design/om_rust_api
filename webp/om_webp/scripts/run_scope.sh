#!/usr/bin/env bash
set -euo pipefail

requested_scope="${1:?usage: run_scope.sh gfs|cams|ecmwf_ifs025}"
case "$requested_scope" in
  gfs)
    scope="gfs"
    ready_group="gfs"
    default_workers=2
    ;;
  cams)
    scope="cams"
    ready_group="cams"
    default_workers=2
    ;;
  ecmwf_ifs025|ecmwf|ec)
    scope="ecmwf_ifs025"
    ready_group="ecmwf"
    default_workers=1
    ;;
  *) echo "invalid scope: $requested_scope" >&2; exit 2 ;;
esac

app_dir="${OM_WEBP_APP_DIR:-/opt/1panel/apps/weather_om_webp}"
data_root="${OM_DATA_ROOT:-/data/om_raw}"
output_root="${OM_WEBP_DATA_ROOT:-/data/om_webp}"
strict_data_root="${OM_STRICT_DATA_ROOT:-/data}"
minimum_free_bytes="${OM_DATA_MIN_FREE_BYTES:-10737418240}"
public_root="${OM_WEBP_PUBLIC_ROOT:-/opt/1panel/apps/weather/data}"
decoder_lib="${OM_OMFILE_LIB:-/opt/1panel/apps/weather_om_api/native/libomfileformat.so}"
dem_root="${OM_DEM_ROOT:-/opt/1panel/apps/weather_om_api/static}"
model_static_root="${OM_MODEL_STATIC_ROOT:-/opt/1panel/apps/weather_om_api}"
workers="${OM_WEBP_WORKERS:-$default_workers}"
frames="${OM_WEBP_FRAMES:-121}"
minimum_open_files="${OM_WEBP_MIN_OPEN_FILES:-65536}"
reporter="${OM_TASK_PROGRESS_REPORTER:-$app_dir/scripts/task_progress_reporter.py}"
log_dir="${OM_WEBP_LOG_DIR:-$app_dir/logs}"
ready_marker="$data_root/groups/$ready_group/current/ready_for_processing.json"

export OM_DEM_ROOT="$dem_root"
export OM_MODEL_STATIC_ROOT="$model_static_root"

if [[ "$data_root" != /* ]]; then
  echo "OM_DATA_ROOT must be an absolute read-only source path: $data_root" >&2
  exit 2
fi
if [[ ! -d "$data_root" ]]; then
  echo "OM_DATA_ROOT does not exist: $data_root" >&2
  exit 1
fi

if [[ ! "$minimum_open_files" =~ ^[1-9][0-9]*$ ]]; then
  echo "OM_WEBP_MIN_OPEN_FILES must be a positive integer: $minimum_open_files" >&2
  exit 2
fi
current_open_files="$(ulimit -Sn)"
hard_open_files="$(ulimit -Hn)"
if [[ "$current_open_files" != "unlimited" ]] \
  && (( current_open_files < minimum_open_files )); then
  if [[ "$hard_open_files" != "unlimited" ]] \
    && (( hard_open_files < minimum_open_files )); then
    echo "WebP open-file hard limit is too low: hard=$hard_open_files required=$minimum_open_files" >&2
    exit 1
  fi
  ulimit -Sn "$minimum_open_files"
fi

if [[ ! -f "$ready_marker" ]]; then
  printf '%s\n' "跳过｜任务：${scope^^} WebP｜原因：官方下载批次尚未发布"
  exit 0
fi

run_renderer() {
  "$app_dir/bin/om-webp" \
    --scope "$scope" \
    --data-root "$data_root" \
    --output-root "$output_root" \
    --strict-data-root "$strict_data_root" \
    --minimum-free-bytes "$minimum_free_bytes" \
    --public-root "$public_root" \
    --decoder-lib "$decoder_lib" \
    --workers "$workers" \
    --frames "$frames"
}

if [[ ! -f "$reporter" ]]; then
  run_renderer
  exit $?
fi

(
  trap 'task_rc=$?; trap - EXIT; printf "\036WEATHER_TASK_RC=%s\n" "$task_rc"; exit "$task_rc"' EXIT
  run_renderer
) 2>&1 | /usr/bin/python3 "$reporter" \
  --task "${scope^^} WebP 构建" \
  --default-stage "生成 WebP" \
  --watch-root "$output_root/staging" \
  --log-file "$log_dir/om_${scope}_webp.log"
