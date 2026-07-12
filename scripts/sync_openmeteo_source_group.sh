#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 --group GROUP --source-host USER@HOST --source-root PATH --raw-root PATH --source-ssh-key PATH --source-known-hosts PATH [--cleanup-grace-seconds SECONDS]" >&2
  exit 2
}

GROUP=""
SOURCE_HOST=""
SOURCE_ROOT=""
RAW=""
SOURCE_SSH_KEY=""
SOURCE_KNOWN_HOSTS=""
CLEANUP_GRACE_SECONDS=300

while [ "$#" -gt 0 ]; do
  case "$1" in
    --group) GROUP="${2:-}"; shift 2 ;;
    --source-host) SOURCE_HOST="${2:-}"; shift 2 ;;
    --source-root) SOURCE_ROOT="${2:-}"; shift 2 ;;
    --raw-root) RAW="${2:-}"; shift 2 ;;
    --source-ssh-key) SOURCE_SSH_KEY="${2:-}"; shift 2 ;;
    --source-known-hosts) SOURCE_KNOWN_HOSTS="${2:-}"; shift 2 ;;
    --cleanup-grace-seconds) CLEANUP_GRACE_SECONDS="${2:-}"; shift 2 ;;
    *) usage ;;
  esac
done

case "$GROUP" in gfs|cams) ;; *) usage ;; esac
[ -n "$SOURCE_HOST" ] && [ -n "$SOURCE_ROOT" ] && [ -n "$RAW" ] || usage
[ -r "$SOURCE_SSH_KEY" ] || { echo "missing source SSH key: $SOURCE_SSH_KEY" >&2; exit 2; }
[ -r "$SOURCE_KNOWN_HOSTS" ] || { echo "missing trusted SSH known_hosts: $SOURCE_KNOWN_HOSTS" >&2; exit 2; }
case "$CLEANUP_GRACE_SECONDS" in *[!0-9]*|'') usage ;; esac
command -v rsync >/dev/null 2>&1 || { echo "rsync is required for source synchronization" >&2; exit 2; }

APP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
LOCK_FILE="$APP_DIR/data/locks/source_sync_$GROUP.lock"
mkdir -p "$(dirname "$LOCK_FILE")"
exec 9>"$LOCK_FILE"
if ! flock -n 9; then
  echo '{"status":"skipped","reason":"source sync already running"}'
  exit 0
fi

MANIFEST_STAGE=""
STAGE=""
cleanup() {
  local status=$?
  [ -z "$MANIFEST_STAGE" ] || rm -rf -- "$MANIFEST_STAGE"
  if [ "$status" -ne 0 ] && [ -n "$STAGE" ]; then
    rm -rf -- "$STAGE"
  fi
  rm -f -- "$LOCK_FILE"
  exit "$status"
}
trap cleanup EXIT

SSH_RSH="$(printf 'ssh -i %q -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=%q' "$SOURCE_SSH_KEY" "$SOURCE_KNOWN_HOSTS")"
STAGE_PARENT="$RAW/.incoming/$GROUP"
mkdir -p "$STAGE_PARENT"
MANIFEST_STAGE="$(mktemp -d "$STAGE_PARENT/.manifest.XXXXXX")"

pull_remote_file() {
  local destination_root="$1"
  local remote_rel="$2"
  local local_rel="$3"
  case "$remote_rel" in /*|*..*) echo "refusing unsafe remote relative path: $remote_rel" >&2; return 2 ;; esac
  case "$local_rel" in /*|*..*) echo "refusing unsafe local relative path: $local_rel" >&2; return 2 ;; esac
  mkdir -p "$destination_root/$(dirname "$local_rel")"
  rsync -a --whole-file --partial --timeout=180 -e "$SSH_RSH" \
    "$SOURCE_HOST:$SOURCE_ROOT/$remote_rel" "$destination_root/$local_rel"
}

remote_group_manifest_exists() {
  local remote_path="$SOURCE_ROOT/groups/$GROUP/latest.json"
  local escaped_remote_path
  printf -v escaped_remote_path '%q' "$remote_path"
  ssh -i "$SOURCE_SSH_KEY" -o BatchMode=yes -o StrictHostKeyChecking=yes \
    -o "UserKnownHostsFile=$SOURCE_KNOWN_HOSTS" "$SOURCE_HOST" \
    "test -f $escaped_remote_path"
}

group_needs_sync() {
  /usr/bin/python3 - "$1" "$2" <<'PY'
import json
import sys

remote_path, local_path = sys.argv[1], sys.argv[2]
with open(remote_path, encoding="utf-8") as handle:
    remote = json.load(handle)
if remote.get("status") != "complete":
    raise SystemExit(1)
try:
    with open(local_path, encoding="utf-8") as handle:
        local = json.load(handle)
except FileNotFoundError:
    raise SystemExit(0)
if local.get("status") != "complete":
    raise SystemExit(0)
keys = (
    "coverage_id", "latest_complete_run", "required_start_utc", "required_end_utc",
    "public_start_utc", "valid_time_count", "files", "bytes", "downloaded_bytes",
)
remote_products = remote.get("product_manifests") or {}
local_products = local.get("product_manifests") or {}
if not isinstance(remote_products, dict) or not isinstance(local_products, dict):
    raise SystemExit(0)
for product, remote_summary in remote_products.items():
    local_summary = local_products.get(product) or {}
    if not isinstance(remote_summary, dict) or any(
        local_summary.get(key) != remote_summary.get(key) for key in keys
    ):
        raise SystemExit(0)
raise SystemExit(1)
PY
}

sync_product_files() {
  local manifest="$1"
  local destination_root="$2"
  local file_list
  file_list="$(mktemp)"
  /usr/bin/python3 - "$manifest" > "$file_list" <<'PY'
import json
import sys
from pathlib import PurePosixPath

with open(sys.argv[1], encoding="utf-8") as handle:
    manifest = json.load(handle)
if manifest.get("status") != "complete":
    raise SystemExit("product manifest is not complete")
product = str(manifest.get("model") or "")
if not product:
    raise SystemExit("manifest missing model")
for item in manifest.get("files") or []:
    rel = PurePosixPath(product) / str(item.get("path", ""))
    if rel.is_absolute() or ".." in rel.parts:
        raise SystemExit(f"unsafe manifest file path: {rel}")
    print(rel.as_posix())
PY
  rsync -a --whole-file --partial --timeout=180 --files-from="$file_list" -e "$SSH_RSH" \
    "$SOURCE_HOST:$SOURCE_ROOT/" "$destination_root/"
  rm -f "$file_list"
}

coverage_needs_transfer() {
  /usr/bin/python3 - "$1" "$2" <<'PY'
import hashlib
import json
import sys
from pathlib import Path, PurePosixPath

manifest_path, raw_root = sys.argv[1:]
with open(manifest_path, encoding="utf-8") as handle:
    remote = json.load(handle)
product = str(remote.get("model") or "")
coverage_id = str(remote.get("coverage_id") or "")
if not product or not coverage_id or remote.get("status") != "complete":
    raise SystemExit(0)
local_manifest_path = Path(raw_root) / product / "coverages" / coverage_id / "latest.json"
try:
    with local_manifest_path.open(encoding="utf-8") as handle:
        local = json.load(handle)
except (FileNotFoundError, json.JSONDecodeError):
    raise SystemExit(0)
keys = (
    "model", "status", "coverage_id", "config_fingerprint", "latest_complete_run",
    "required_start_utc", "public_start_utc", "required_end_utc", "valid_time_count",
    "bytes", "downloaded_bytes",
)
if any(local.get(key) != remote.get(key) for key in keys):
    raise SystemExit(0)
files = remote.get("files")
if not isinstance(files, list) or not files:
    raise SystemExit(0)
for item in files:
    rel = PurePosixPath(str(item.get("path") or ""))
    if rel.is_absolute() or ".." in rel.parts:
        raise SystemExit(0)
    path = Path(raw_root) / product / Path(*rel.parts)
    if not path.is_file() or path.stat().st_size != int(item.get("bytes") or -1):
        raise SystemExit(0)
    expected = str(item.get("sha256") or "")
    if expected:
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
        if digest.hexdigest() != expected:
            raise SystemExit(0)
raise SystemExit(1)
PY
}

if remote_group_manifest_exists; then
  :
else
  manifest_check_status=$?
  if [ "$manifest_check_status" -eq 1 ]; then
    echo '{"status":"skipped","reason":"remote group manifest unavailable"}'
    exit 0
  fi
  echo '{"status":"error","reason":"remote group manifest check failed"}' >&2
  exit "$manifest_check_status"
fi
pull_remote_file "$MANIFEST_STAGE" "groups/$GROUP/latest.json" "groups/$GROUP/latest.json"
GROUP_MANIFEST="$MANIFEST_STAGE/groups/$GROUP/latest.json"
if ! /usr/bin/python3 - "$GROUP_MANIFEST" "$GROUP" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    manifest = json.load(handle)
latest_complete_run = manifest.get("latest_complete_run")
summaries = manifest.get("product_manifests")
require_matching_runs = True
ready = (
    manifest.get("group") == sys.argv[2]
    and manifest.get("status") == "complete"
    and isinstance(latest_complete_run, str)
    and bool(latest_complete_run)
    and isinstance(summaries, dict)
    and all(
        isinstance(summary, dict)
        and summary.get("status") == "complete"
        and isinstance(summary.get("latest_complete_run"), str)
        and bool(summary.get("latest_complete_run"))
        and (not require_matching_runs or summary.get("latest_complete_run") == latest_complete_run)
        for summary in summaries.values()
    )
)
raise SystemExit(0 if ready else 1)
PY
then
  echo '{"status":"skipped","reason":"remote group is not complete"}'
  exit 0
fi

if [ "$GROUP" = "cams" ]; then
  RELEASE_STAGE="$MANIFEST_STAGE/groups/cams/releases"
  mkdir -p "$RELEASE_STAGE"
  if ! rsync -a --whole-file --partial --timeout=180 -e "$SSH_RSH" \
    "$SOURCE_HOST:$SOURCE_ROOT/groups/cams/releases/" "$RELEASE_STAGE/"; then
    echo '{"status":"skipped","reason":"remote CAMS release window unavailable"}'
    exit 0
  fi

  mapfile -t COVERAGES < <(/usr/bin/python3 - "$RELEASE_STAGE" <<'PY'
import json
import sys
from pathlib import Path

release_root = Path(sys.argv[1])
releases = []
for path in release_root.glob("*.json"):
    with path.open(encoding="utf-8") as handle:
        release = json.load(handle)
    run = str(release.get("latest_complete_run") or "")
    summaries = release.get("product_manifests")
    if release.get("group") != "cams" or release.get("status") != "complete":
        continue
    if not run or not isinstance(summaries, dict):
        continue
    if set(summaries) != {"cams_global", "cams_global_greenhouse_gases"}:
        continue
    if any(
        not isinstance(summary, dict)
        or summary.get("status") != "complete"
        or summary.get("latest_complete_run") != run
        or not summary.get("coverage_id")
        for summary in summaries.values()
    ):
        continue
    releases.append((run, str(release.get("synced_at") or ""), path.name, summaries))
releases.sort(reverse=True)
selected = []
seen = set()
for run, synced_at, name, summaries in releases:
    if run in seen:
        continue
    seen.add(run)
    selected.append((run, summaries))
    if len(selected) == 3:
        break
if len(selected) != 3:
    raise SystemExit("source does not expose three complete CAMS releases")
for _run, summaries in reversed(selected):
    for product in ("cams_global", "cams_global_greenhouse_gases"):
        print(f"{product}\t{summaries[product]['coverage_id']}")
PY
  )

  for item in "${COVERAGES[@]}"; do
    IFS=$'\t' read -r product coverage_id <<< "$item"
    manifest_rel="$product/coverages/$coverage_id/latest.json"
    pull_remote_file "$MANIFEST_STAGE" "$manifest_rel" "$manifest_rel"
    if coverage_needs_transfer "$MANIFEST_STAGE/$manifest_rel" "$RAW"; then
      sync_product_files "$MANIFEST_STAGE/$manifest_rel" "$MANIFEST_STAGE"
    fi
  done

  cd "$APP_DIR"
  /usr/bin/python3 -m om_downloader.cli \
    --sync-openmeteo-group-releases-from-source cams \
    --source-stage-root "$MANIFEST_STAGE" \
    --output "$RAW" \
    --retain-complete-releases 3
  exit 0
fi

if ! group_needs_sync "$GROUP_MANIFEST" "$RAW/groups/$GROUP/current/ready_for_processing.json"; then
  # The product files are already current, but the CLI still records the
  # complete group release. This makes a first post-upgrade sync preserve the
  # existing batch for daily CAMS history without retransferring it.
  cd "$APP_DIR"
  /usr/bin/python3 -m om_downloader.cli \
    --sync-openmeteo-group-from-source "$GROUP" \
    --source-stage-root "$MANIFEST_STAGE" \
    --output "$RAW" \
    --cleanup-grace-seconds "$CLEANUP_GRACE_SECONDS"
  exit 0
fi

cd "$APP_DIR"
RELEASE_ID="$(/usr/bin/python3 -m om_downloader.cli --print-openmeteo-group-release-id "$GROUP" --source-stage-root "$MANIFEST_STAGE" | /usr/bin/python3 -c 'import json, sys; print(json.load(sys.stdin)["release_id"])')"
STAGE="$STAGE_PARENT/$RELEASE_ID/source"
case "$STAGE" in "$STAGE_PARENT"/*) ;; *) echo "unsafe stage path" >&2; exit 2 ;; esac
rm -rf -- "$STAGE"
mkdir -p "$STAGE"
mv "$MANIFEST_STAGE/groups" "$STAGE/"
MANIFEST_STAGE=""
GROUP_MANIFEST="$STAGE/groups/$GROUP/latest.json"

mapfile -t PRODUCTS < <(/usr/bin/python3 - "$GROUP_MANIFEST" "$GROUP" <<'PY'
import json
import sys

manifest_path, group = sys.argv[1:]
allowed = {
    "gfs": ("gfs013_surface", "gfs025", "gfs_pressure_profile"),
    "cams": ("cams_global", "cams_global_greenhouse_gases"),
}[group]
minimum = {
    "gfs": ("gfs013_surface", "gfs025", "gfs_pressure_profile"),
    "cams": ("cams_global",),
}[group]
with open(manifest_path, encoding="utf-8") as handle:
    manifest = json.load(handle)
summaries = manifest.get("product_manifests")
if not isinstance(summaries, dict):
    raise SystemExit("group manifest has invalid product summaries")
unexpected = sorted(set(summaries) - set(allowed))
missing = [product for product in minimum if product not in summaries]
if unexpected or missing:
    raise SystemExit(
        "group manifest product mismatch: "
        + ", ".join([*(f"unexpected={item}" for item in unexpected), *(f"missing={item}" for item in missing)])
    )
for product in allowed:
    if product in summaries:
        print(product)
PY
)
[ "${#PRODUCTS[@]}" -gt 0 ] || { echo "group manifest has no products" >&2; exit 2; }
for product in "${PRODUCTS[@]}"; do
  pull_remote_file "$STAGE" "$product/latest.json" "$product/latest.json"
  sync_product_files "$STAGE/$product/latest.json" "$STAGE"
done

/usr/bin/python3 -m om_downloader.cli \
  --sync-openmeteo-group-from-source "$GROUP" \
  --source-stage-root "$STAGE" \
  --output "$RAW" \
  --cleanup-grace-seconds "$CLEANUP_GRACE_SECONDS"
rm -rf -- "$STAGE"
STAGE=""
