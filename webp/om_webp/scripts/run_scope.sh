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
    default_workers=2
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
memory_max="${OM_WEBP_MEMORY_MAX:-1536M}"
cpu_quota="${OM_WEBP_CPU_QUOTA:-150%}"
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

target_run="$(
  /usr/bin/python3 - "$ready_marker" <<'PY'
import json
import sys
from pathlib import Path

payload = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
print(payload.get("latest_complete_run") or "")
PY
)"
if [[ ! "$target_run" =~ ^20[0-9]{8}$ ]]; then
  echo "WebP ready marker has no canonical latest_complete_run: $ready_marker" >&2
  exit 1
fi

run_renderer() {
  local -a renderer=(
    "$app_dir/bin/om-webp"
    --scope "$scope" \
    --data-root "$data_root" \
    --output-root "$output_root" \
    --strict-data-root "$strict_data_root" \
    --minimum-free-bytes "$minimum_free_bytes" \
    --public-root "$public_root" \
    --decoder-lib "$decoder_lib" \
    --workers "$workers" \
    --frames "$frames"
  )

  if [[ ! "$memory_max" =~ ^[1-9][0-9]*(K|M|G|T)?$ ]]; then
    echo "OM_WEBP_MEMORY_MAX must be a positive systemd size: $memory_max" >&2
    return 2
  fi
  if [[ ! "$cpu_quota" =~ ^[1-9][0-9]*%$ ]]; then
    echo "OM_WEBP_CPU_QUOTA must be a positive percentage: $cpu_quota" >&2
    return 2
  fi
  if [[ "$(id -u)" -ne 0 || ! -d /run/systemd/system ]] \
    || ! command -v systemd-run >/dev/null 2>&1; then
    echo "WebP production memory guard requires root and systemd-run" >&2
    return 1
  fi

  systemd-run --quiet --pipe --wait --collect \
    --unit="weather-om-webp-${scope//_/-}-$$" \
    --property="MemoryMax=$memory_max" \
    --property="MemorySwapMax=0" \
    --property="CPUQuota=$cpu_quota" \
    --property="LimitNOFILE=$minimum_open_files" \
    --property="Nice=15" \
    --property="IOSchedulingClass=idle" \
    --setenv="OM_DEM_ROOT=$dem_root" \
    --setenv="OM_MODEL_STATIC_ROOT=$model_static_root" \
    "${renderer[@]}"
}

if [[ ! -f "$reporter" ]]; then
  run_renderer
  exit $?
fi

(
  trap 'task_rc=$?; trap - EXIT; printf "\036WEATHER_TASK_RC=%s\n" "$task_rc"; exit "$task_rc"' EXIT
  printf '\036WEATHER_TASK_TARGET_RUN=%s\n' "$target_run"
  run_renderer
) 2>&1 | /usr/bin/python3 "$reporter" \
  --task "${scope^^} WebP 构建" \
  --default-stage "生成 WebP" \
  --watch-root "$output_root/staging" \
  --log-file "$log_dir/om_${scope}_webp.log"
