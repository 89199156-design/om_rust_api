#!/usr/bin/env bash
set -Eeuo pipefail

usage() {
    cat <<'EOF'
Run saved official snapshots against one resource-limited local probe API.

Required:
  --output PATH       Existing official-100 snapshot directory
  --data-root PATH    Data root to expose through the probe API

Optional:
  --models LIST       gfs,ec,cams subset (default: gfs,ec,cams)
  --port PORT         Probe loopback port (default: 18089)
  --api-binary PATH   Probe om-api binary
  --source-root PATH  Server repository root
  --field-chunk-size N
  --request-delay-seconds N
  --point-delay-seconds N
  --timeout-seconds N
  --point-limit N      Partial smoke run only (default: 100)

The runner never calls an official API. It refuses to run alongside another
non-production om-api, downloader, or WebP process, and always stops its probe.
EOF
}

output=
data_root=
models=gfs,ec,cams
port=18089
api_binary=/opt/1panel/apps/weather_om_api/source/target/release/om-api
source_root=/opt/1panel/apps/weather_om_api/source
field_chunk_size=96
request_delay_seconds=0
point_delay_seconds=0
timeout_seconds=180
point_limit=100

while (($#)); do
    case "$1" in
        --output) output=${2:?}; shift 2 ;;
        --data-root) data_root=${2:?}; shift 2 ;;
        --models) models=${2:?}; shift 2 ;;
        --port) port=${2:?}; shift 2 ;;
        --api-binary) api_binary=${2:?}; shift 2 ;;
        --source-root) source_root=${2:?}; shift 2 ;;
        --field-chunk-size) field_chunk_size=${2:?}; shift 2 ;;
        --request-delay-seconds) request_delay_seconds=${2:?}; shift 2 ;;
        --point-delay-seconds) point_delay_seconds=${2:?}; shift 2 ;;
        --timeout-seconds) timeout_seconds=${2:?}; shift 2 ;;
        --point-limit) point_limit=${2:?}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

[[ -n "$output" && -n "$data_root" ]] || {
    usage >&2
    exit 2
}
[[ "$output" = /* && "$data_root" = /* && "$source_root" = /* && "$api_binary" = /* ]] || {
    echo "all paths must be absolute" >&2
    exit 2
}
[[ -d "$output" && -d "$data_root" && -d "$source_root" && -x "$api_binary" ]] || {
    echo "output, data root, source root, or API binary is unavailable" >&2
    exit 2
}
[[ "$port" =~ ^[0-9]+$ && "$field_chunk_size" =~ ^[0-9]+$ && "$point_limit" =~ ^[0-9]+$ ]] || {
    echo "port, field chunk size, and point limit must be positive integers" >&2
    exit 2
}
((port > 0 && port < 65536 && field_chunk_size > 0 && point_limit > 0 && point_limit <= 100)) || {
    echo "port, field chunk size, or point limit is outside its valid range" >&2
    exit 2
}

if ((EUID == 0)); then
    sudo_cmd=()
else
    sudo_cmd=(sudo)
fi

exec 9>/run/lock/om-official-100-validation.lock
flock -n 9 || {
    echo "another official-100 validation runner is active" >&2
    exit 1
}

mapfile -t existing_api_pids < <(pgrep -x om-api || true)
if ((${#existing_api_pids[@]} > 1)); then
    echo "refusing to start a third om-api process; active PIDs: ${existing_api_pids[*]}" >&2
    exit 1
fi
if pgrep -af '(^|/)(om-webp|om-download|om_downloader)( |$)' >/dev/null; then
    echo "refusing validation while a downloader or WebP process is active" >&2
    pgrep -af '(^|/)(om-webp|om-download|om_downloader)( |$)' >&2 || true
    exit 1
fi

suffix="$(date -u +%Y%m%dT%H%M%SZ)-$$"
probe_unit="om-api-validation-${suffix}"
validator_unit="om-official-100-validation-${suffix}"
local_base="http://127.0.0.1:${port}"

cleanup() {
    "${sudo_cmd[@]}" systemctl stop "${probe_unit}.service" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

"${sudo_cmd[@]}" systemd-run \
    --unit="$probe_unit" \
    --collect \
    --property=Type=simple \
    --property=User=ubuntu \
    --property=Group=ubuntu \
    --property="WorkingDirectory=$source_root" \
    --property=Restart=no \
    --property=KillMode=control-group \
    --property=OOMPolicy=stop \
    --property=MemoryHigh=1100M \
    --property=MemoryMax=1400M \
    --property=CPUQuota=100% \
    --property=IOWeight=10 \
    --property=Nice=10 \
    --property=IOSchedulingClass=best-effort \
    --property=IOSchedulingPriority=7 \
    --property=TasksMax=64 \
    --property=LimitNOFILE=65536 \
    --setenv="OM_API_BIND=127.0.0.1:$port" \
    --setenv="OM_DATA_ROOT=$data_root" \
    --setenv=OM_DEM_ROOT=/opt/1panel/apps/weather_om_api/static \
    --setenv=OM_MODEL_STATIC_ROOT=/opt/1panel/apps/weather_om_api \
    --setenv=OM_OMFILE_LIB=/opt/1panel/apps/weather_om_api/native/libomfileformat.so \
    --setenv=OM_SNAPSHOT_REFRESH_SECONDS=3600 \
    "$api_binary"

probe_ready=false
for _ in $(seq 1 60); do
    if ! "${sudo_cmd[@]}" systemctl is-active --quiet "${probe_unit}.service"; then
        "${sudo_cmd[@]}" journalctl -u "${probe_unit}.service" --no-pager -n 100 >&2
        exit 1
    fi
    http_code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 3 "$local_base/" || true)"
    if [[ "$http_code" != 000 ]]; then
        probe_ready=true
        break
    fi
    sleep 1
done
[[ "$probe_ready" == true ]] || {
    echo "probe API did not become ready" >&2
    exit 1
}

"${sudo_cmd[@]}" systemd-run \
    --unit="$validator_unit" \
    --wait \
    --pipe \
    --collect \
    --property=Type=exec \
    --property=User=ubuntu \
    --property=Group=ubuntu \
    --property="WorkingDirectory=$source_root" \
    --property=Restart=no \
    --property=KillMode=control-group \
    --property=OOMPolicy=stop \
    --property=MemoryHigh=320M \
    --property=MemoryMax=384M \
    --property=CPUQuota=25% \
    --property=IOWeight=10 \
    --property=Nice=15 \
    --property=IOSchedulingClass=best-effort \
    --property=IOSchedulingPriority=7 \
    --property=TasksMax=32 \
    /usr/bin/python3 scripts/validation/official_100_point_compare.py validate \
    --models "$models" \
    --output "$output" \
    --local-base "$local_base" \
    --timeout "$timeout_seconds" \
    --retries 0 \
    --field-chunk-size "$field_chunk_size" \
    --request-delay-seconds "$request_delay_seconds" \
    --point-delay-seconds "$point_delay_seconds" \
    --min-available-memory-mib 768 \
    --max-io-full-pressure-avg10 10 \
    --resource-wait-timeout-seconds 900 \
    --resource-poll-seconds 5 \
    --max-local-om-api-processes 2 \
    --point-limit "$point_limit"
