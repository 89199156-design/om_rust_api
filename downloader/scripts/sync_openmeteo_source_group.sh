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

case "$GROUP" in
  gfs) RETENTION=4 ;;
  cams) RETENTION=3 ;;
  *) usage ;;
esac
[ -n "$SOURCE_HOST" ] && [ -n "$SOURCE_ROOT" ] && [ -n "$RAW" ] || usage
[ -r "$SOURCE_SSH_KEY" ] || { echo "missing source SSH key: $SOURCE_SSH_KEY" >&2; exit 2; }
[ -r "$SOURCE_KNOWN_HOSTS" ] || { echo "missing trusted SSH known_hosts: $SOURCE_KNOWN_HOSTS" >&2; exit 2; }
case "$CLEANUP_GRACE_SECONDS" in *[!0-9]*|'') usage ;; esac
command -v rsync >/dev/null 2>&1 || { echo "rsync is required for source synchronization" >&2; exit 2; }

APP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
RAW="$(readlink -m "$RAW")"
STAGE_ROOT="$(readlink -m "$RAW/.source_sync_stage/$GROUP")"
case "$STAGE_ROOT" in
  "$RAW"/.source_sync_stage/*) ;;
  *) echo "unsafe source stage root: $STAGE_ROOT" >&2; exit 2 ;;
esac

LOCK_FILE="$APP_DIR/data/locks/source_sync_$GROUP.lock"
mkdir -p "$(dirname "$LOCK_FILE")" "$RAW" "$STAGE_ROOT"
exec 9>"$LOCK_FILE"
if ! flock -n 9; then
  echo '{"status":"skipped","reason":"source reconciliation already running"}'
  exit 0
fi

WORK_DIRS=()
cleanup() {
  local status=$?
  local path
  for path in "${WORK_DIRS[@]}"; do
    rm -rf -- "$path"
  done
  rm -f -- "$LOCK_FILE"
  exit "$status"
}
trap cleanup EXIT

SSH_RSH="$(printf 'ssh -i %q -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=%q' "$SOURCE_SSH_KEY" "$SOURCE_KNOWN_HOSTS")"

source_reconciliation_running() {
  local lock_path
  lock_path="$(readlink -m "$SOURCE_ROOT/../locks/$GROUP"_reconcile.lock)"
  ssh -i "$SOURCE_SSH_KEY" \
    -o BatchMode=yes \
    -o StrictHostKeyChecking=yes \
    -o UserKnownHostsFile="$SOURCE_KNOWN_HOSTS" \
    "$SOURCE_HOST" /usr/bin/python3 - "$lock_path" <<'PY'
import os
import re
import sys
import time
from pathlib import Path

path = Path(sys.argv[1])
try:
    payload = path.read_text(encoding="utf-8")
    modified_at = path.stat().st_mtime
except FileNotFoundError:
    raise SystemExit(1)

match = re.search(r"(?:^|\s)pid=(\d+)(?:\s|$)", payload)
if match:
    try:
        os.kill(int(match.group(1)), 0)
    except ProcessLookupError:
        raise SystemExit(1)
    except PermissionError:
        pass
    raise SystemExit(0)
raise SystemExit(0 if time.time() - modified_at < 30 else 1)
PY
}

skip_source_change() {
  printf '{"group":"%s","status":"skipped","reason":"source publication changed during synchronization; retry on next schedule"}\n' "$GROUP"
  exit 0
}

pull_remote_file() {
  local relative_path="$1"
  case "$relative_path" in
    /*|*..*)
      echo "refusing unsafe remote relative path: $relative_path" >&2
      return 2
      ;;
  esac
  mkdir -p "$STAGE_ROOT/$(dirname "$relative_path")"
  rsync -a --whole-file --partial --partial-dir=.rsync-partial --timeout=180 \
    -e "$SSH_RSH" \
    "$SOURCE_HOST:$SOURCE_ROOT/$relative_path" \
    "$STAGE_ROOT/$relative_path"
}

select_target_releases() {
  local selected_path="$1"
  local release_root="$STAGE_ROOT/groups/$GROUP/releases"
  mkdir -p "$release_root"
  if ! rsync -a --delete --include='*.json' --exclude='*' --timeout=180 \
    -e "$SSH_RSH" \
    "$SOURCE_HOST:$SOURCE_ROOT/groups/$GROUP/releases/" \
    "$release_root/"; then
    echo "{\"group\":\"$GROUP\",\"status\":\"skipped\",\"reason\":\"source release index unavailable\"}"
    return 3
  fi

  /usr/bin/python3 - "$STAGE_ROOT" "$GROUP" "$RETENTION" "$selected_path" <<'PY'
import json
import sys
from pathlib import Path

stage_root = Path(sys.argv[1])
group = sys.argv[2]
retention = int(sys.argv[3])
selected_path = Path(sys.argv[4])
required_products = {
    "gfs": {"gfs013_surface", "gfs025", "gfs_pressure_profile"},
    "cams": {"cams_global", "cams_global_greenhouse_gases"},
}[group]
releases = []
for path in (stage_root / "groups" / group / "releases").glob("*.json"):
    payload = json.loads(path.read_text(encoding="utf-8"))
    run = str(payload.get("latest_complete_run") or "")
    summaries = payload.get("product_manifests")
    if (
        payload.get("group") != group
        or payload.get("status") != "complete"
        or not run
        or not isinstance(summaries, dict)
        or set(summaries) != required_products
    ):
        continue
    if any(
        not isinstance(summary, dict)
        or summary.get("status") != "complete"
        or not summary.get("latest_complete_run")
        or not summary.get("coverage_id")
        for summary in summaries.values()
    ):
        continue
    if group == "gfs" and any(
        summary.get("latest_complete_run") != run for summary in summaries.values()
    ):
        continue
    releases.append(
        (
            run,
            str(payload.get("synced_at") or ""),
            str(payload.get("release_id") or path.stem),
            path,
        )
    )
releases.sort(reverse=True)
selected = []
seen_runs = set()
for run, _synced_at, _release_id, path in releases:
    if run in seen_runs:
        continue
    seen_runs.add(run)
    selected.append(path)
    if len(selected) == retention:
        break
if len(selected) != retention:
    print(
        json.dumps(
            {
                "group": group,
                "status": "skipped",
                "reason": (
                    f"source target set incomplete: expected {retention} distinct releases, "
                    f"found {len(selected)}"
                ),
            },
            ensure_ascii=False,
        )
    )
    raise SystemExit(3)
selected_path.write_text(
    "".join(f"{path.name}\n" for path in selected),
    encoding="utf-8",
)
PY
}

pull_target_manifests() {
  local selected_path="$1"
  local manifest_list="$2"
  local status=0

  /usr/bin/python3 - "$STAGE_ROOT" "$GROUP" "$selected_path" >"$manifest_list" <<'PY'
import json
import sys
from pathlib import Path, PurePosixPath

stage_root = Path(sys.argv[1])
group = sys.argv[2]
selected_path = Path(sys.argv[3])
paths = set()
for release_name in selected_path.read_text(encoding="utf-8").splitlines():
    release = json.loads(
        (stage_root / "groups" / group / "releases" / release_name).read_text(encoding="utf-8")
    )
    for product, summary in (release.get("product_manifests") or {}).items():
        coverage_id = str((summary or {}).get("coverage_id") or "")
        relative = PurePosixPath(product) / "coverages" / coverage_id / "latest.json"
        if not product or not coverage_id or relative.is_absolute() or ".." in relative.parts:
            raise SystemExit(f"unsafe product manifest path: {relative}")
        paths.add(relative.as_posix())
for path in sorted(paths):
    print(path)
PY

  while IFS= read -r relative_path; do
    [ -n "$relative_path" ] || continue
    pull_remote_file "$relative_path" || status=$?
    if [ "$status" -ne 0 ]; then
      break
    fi
  done <"$manifest_list"
  return "$status"
}

prepare_missing_payloads() {
  local selected_path="$1"
  local payload_list="$2"

  /usr/bin/python3 - "$STAGE_ROOT" "$RAW" "$GROUP" "$selected_path" >"$payload_list" <<'PY'
import hashlib
import json
import os
import shutil
import sys
from pathlib import Path, PurePosixPath

stage_root = Path(sys.argv[1])
raw_root = Path(sys.argv[2])
group = sys.argv[3]
selected_path = Path(sys.argv[4])
selected_coverages = set()
records = []

for release_name in selected_path.read_text(encoding="utf-8").splitlines():
    release = json.loads(
        (stage_root / "groups" / group / "releases" / release_name).read_text(encoding="utf-8")
    )
    for product, summary in (release.get("product_manifests") or {}).items():
        coverage_id = str((summary or {}).get("coverage_id") or "")
        manifest_path = stage_root / product / "coverages" / coverage_id / "latest.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        if (
            manifest.get("model") != product
            or manifest.get("coverage_id") != coverage_id
            or manifest.get("status") != "complete"
        ):
            raise SystemExit(f"invalid product manifest: {product}/{coverage_id}")
        selected_coverages.add((product, coverage_id))
        records.append((product, manifest))

for product_root in stage_root.iterdir():
    coverages_root = product_root / "coverages"
    if not coverages_root.is_dir():
        continue
    for coverage_path in coverages_root.iterdir():
        if coverage_path.is_dir() and (product_root.name, coverage_path.name) not in selected_coverages:
            shutil.rmtree(coverage_path)

def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()

missing = set()
for product, manifest in records:
    for item in manifest.get("files") or []:
        relative = PurePosixPath(str(item.get("path") or ""))
        if relative.is_absolute() or not relative.parts or ".." in relative.parts:
            raise SystemExit(f"unsafe payload path: {relative}")
        source = raw_root / product / Path(*relative.parts)
        destination = stage_root / product / Path(*relative.parts)
        expected_bytes = int(item.get("bytes") or -1)
        expected_sha = str(item.get("sha256") or "")
        valid = source.is_file() and source.stat().st_size == expected_bytes
        if valid and expected_sha:
            valid = sha256(source) == expected_sha
        destination.parent.mkdir(parents=True, exist_ok=True)
        if valid:
            destination.unlink(missing_ok=True)
            try:
                os.link(source, destination)
            except OSError:
                shutil.copy2(source, destination)
        else:
            destination.unlink(missing_ok=True)
            missing.add((PurePosixPath(product) / relative).as_posix())

for path in sorted(missing):
    print(path)
PY
}

WORK_ROOT="$(mktemp -d)"
WORK_DIRS+=("$WORK_ROOT")
SELECTED_PATH="$WORK_ROOT/selected.txt"
MANIFEST_LIST="$WORK_ROOT/manifests.txt"
PAYLOAD_LIST="$WORK_ROOT/payloads.txt"

if source_reconciliation_running; then
  printf '{"group":"%s","status":"skipped","reason":"source reconciliation is running"}\n' "$GROUP"
  exit 0
fi
if ! select_target_releases "$SELECTED_PATH"; then
  exit 0
fi
if source_reconciliation_running; then
  printf '{"group":"%s","status":"skipped","reason":"source reconciliation started during synchronization"}\n' "$GROUP"
  exit 0
fi
manifest_status=0
pull_target_manifests "$SELECTED_PATH" "$MANIFEST_LIST" || manifest_status=$?
if [ "$manifest_status" -eq 23 ]; then
  skip_source_change
elif [ "$manifest_status" -ne 0 ]; then
  exit "$manifest_status"
fi
prepare_missing_payloads "$SELECTED_PATH" "$PAYLOAD_LIST"

if [ -s "$PAYLOAD_LIST" ]; then
  rsync -a --whole-file --partial --partial-dir=.rsync-partial --timeout=180 \
    --files-from="$PAYLOAD_LIST" -e "$SSH_RSH" \
    "$SOURCE_HOST:$SOURCE_ROOT/" "$STAGE_ROOT/"
fi

cd "$APP_DIR"
/usr/bin/python3 -m om_downloader.cli \
  --sync-openmeteo-group-releases-from-source "$GROUP" \
  --source-stage-root "$STAGE_ROOT" \
  --output "$RAW" \
  --retain-complete-releases "$RETENTION"

rm -rf -- "$STAGE_ROOT"
