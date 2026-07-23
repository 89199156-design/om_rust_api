#!/usr/bin/env bash
set -euo pipefail

ZIP_PATH="${1:?usage: install_from_zip.sh /path/weather_om_downloader_deploy.zip [install_dir]}"
INSTALL_DIR="${2:-/opt/1panel/apps/weather_om_downloader}"
BACKUP_DIR="${INSTALL_DIR}.bak.$(date -u +%Y%m%dT%H%M%SZ)"
DOWNLOAD_ROOT="${OM_DOWNLOADER_DATA_ROOT:-/data/om_downloader}"
RAW_ROOT="${OM_DATA_ROOT:-/data/om_raw}"
INSTALL_OWNER="${OM_DOWNLOADER_USER:-${SUDO_USER:-ubuntu}}"
STRICT_DATA_ROOT="${OM_STRICT_DATA_ROOT:-/data}"

for required_path in "$DOWNLOAD_ROOT" "$RAW_ROOT"; do
  if [[ "$required_path" != /* ]]; then
    echo "data paths must be absolute: $required_path" >&2
    exit 2
  fi
done
if ! mountpoint -q -- "$STRICT_DATA_ROOT"; then
  echo "strict data root is not mounted: $STRICT_DATA_ROOT" >&2
  exit 1
fi
if [ "$(stat -c %d /)" = "$(stat -c %d "$STRICT_DATA_ROOT")" ]; then
  echo "strict data root shares the system filesystem: $STRICT_DATA_ROOT" >&2
  exit 1
fi
strict_real="$(readlink -f -- "$STRICT_DATA_ROOT")"
for required_path in "$DOWNLOAD_ROOT" "$RAW_ROOT"; do
  resolved_path="$(readlink -m -- "$required_path")"
  case "$resolved_path" in
    "$strict_real"|"$strict_real"/*) ;;
    *)
      echo "runtime data path escapes strict data root: $required_path" >&2
      exit 1
      ;;
  esac
done
if ! id "$INSTALL_OWNER" >/dev/null 2>&1; then
  echo "downloader owner does not exist: $INSTALL_OWNER" >&2
  exit 1
fi

SUDO=""
if [[ "$(id -u)" -ne 0 ]]; then
  SUDO="sudo"
fi
run_privileged() {
  if [[ -n "$SUDO" ]]; then
    "$SUDO" "$@"
  else
    "$@"
  fi
}

if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required" >&2
  exit 1
fi

if ! command -v unzip >/dev/null 2>&1; then
  echo "unzip is required" >&2
  exit 1
fi

if ! command -v gcc >/dev/null 2>&1; then
  echo "gcc is required to build native TurboPFor decoder" >&2
  exit 1
fi

if [ ! -f "$ZIP_PATH" ]; then
  echo "zip package not found: $ZIP_PATH" >&2
  exit 1
fi

mkdir -p "$(dirname "$INSTALL_DIR")"
if [ -d "$INSTALL_DIR" ]; then
  mv "$INSTALL_DIR" "$BACKUP_DIR"
  echo "backup: $BACKUP_DIR"
fi
mkdir -p "$INSTALL_DIR"
unzip -q "$ZIP_PATH" -d "$INSTALL_DIR"

cd "$INSTALL_DIR"
if [ -d "$BACKUP_DIR/data" ]; then
  echo "legacy system-disk data preserved at $BACKUP_DIR/data"
fi
mkdir -p native logs
run_privileged install -d -o "$INSTALL_OWNER" -g "$INSTALL_OWNER" -m 0775 \
  "$DOWNLOAD_ROOT" \
  "$RAW_ROOT" \
  "$RAW_ROOT/ecmwf_ifs025" \
  "$RAW_ROOT/groups/ecmwf" \
  "$RAW_ROOT/groups/ecmwf/current" \
  "$RAW_ROOT/groups/ecmwf/releases"
bash scripts/build_turbopfor_decoder.sh "$INSTALL_DIR/native"

export OM_TURBOPFOR_LIB="$INSTALL_DIR/native/libom_turbopfor.so"
python3 -c "from om_downloader.om_native import load_default_turbopfor_decoder; load_default_turbopfor_decoder(); print('native_decoder_ok')"
python3 -m om_downloader.cli --inspect-product-catalog gfs025 --config config/models.json --now "$(date -u +%Y-%m-%dT%H:00:00Z)"
python3 -m om_downloader.cli --inspect-product-catalog cams_global --config config/models.json --now "$(date -u +%Y-%m-%dT%H:00:00Z)"
python3 -m om_downloader.cli --inspect-product-catalog cams_global_greenhouse_gases --config config/models.json --now "$(date -u +%Y-%m-%dT%H:00:00Z)"
python3 -m om_downloader.cli --inspect-product-catalog ecmwf_ifs025 --config config/models.json --now "$(date -u +%Y-%m-%dT%H:00:00Z)"

if [ "$(id -u)" -eq 0 ]; then
  chown -R "$INSTALL_OWNER:$INSTALL_OWNER" "$INSTALL_DIR"
fi

cat <<EOF
installed: $INSTALL_DIR

Create 1Panel plan tasks from the commands in README.md.
Do not add system scheduler entries; keep scheduling visible in 1Panel.
EOF
