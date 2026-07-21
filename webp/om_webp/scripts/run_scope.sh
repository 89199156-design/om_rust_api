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
reporter="${OM_TASK_PROGRESS_REPORTER:-/opt/1panel/apps/weather_om_downloader/scripts/task_progress_reporter.py}"
log_dir="${OM_WEBP_LOG_DIR:-$app_dir/data/logs}"
ready_marker="$data_root/groups/$scope/current/ready_for_processing.json"

if [[ ! -f "$ready_marker" ]]; then
  printf '%s\n' "跳过｜任务：${scope^^} WebP｜原因：官方下载批次尚未发布"
  exit 0
fi

run_renderer() {
  "$app_dir/bin/om-webp" \
    --scope "$scope" \
    --data-root "$data_root" \
    --output-root "$output_root" \
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
