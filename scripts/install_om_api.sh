#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="${1:-/opt/1panel/apps/weather_om_api}"
DATA_ROOT="${OM_DATA_ROOT:-/data/om_raw}"
BIND_ADDR="${OM_API_BIND:-127.0.0.1:8088}"
SERVICE_NAME="${OM_API_SERVICE_NAME:-weather-om-api}"
INSTALL_OWNER="${OM_API_USER:-ubuntu}"
API_DEM_ROOT="${OM_API_DEM_ROOT:-$INSTALL_DIR/static}"
MODEL_STATIC_ROOT="${OM_API_MODEL_STATIC_ROOT:-$INSTALL_DIR}"
STRICT_DATA_ROOT="${OM_STRICT_DATA_ROOT:-}"
DEM_LATITUDE_CHUNK_MIN=0
DEM_LATITUDE_CHUNK_MAX=58
GFS013_STATIC_URL="${OM_GFS013_STATIC_URL:-https://openmeteo.s3.amazonaws.com/data/ncep_gfs013/static/HSURF.om}"
GFS013_STATIC_SHA256="${OM_GFS013_STATIC_SHA256:-203745df4dfa10069e1a39206350e006818a0eea644bb19c1668c0f32f7475e0}"
GFS013_STATIC_PATH="$MODEL_STATIC_ROOT/static/ncep_gfs013/HSURF.om"
GFS025_STATIC_URL="${OM_GFS025_STATIC_URL:-https://openmeteo.s3.amazonaws.com/data/ncep_gfs025/static/HSURF.om}"
GFS025_STATIC_SHA256="${OM_GFS025_STATIC_SHA256:-fdd9587e606e64d6d85474c703b9898669d230aac1574fc460cc3087227e868d}"
GFS025_STATIC_PATH="$MODEL_STATIC_ROOT/static/ncep_gfs025/HSURF.om"
ECMWF025_STATIC_URL="${OM_ECMWF025_STATIC_URL:-https://openmeteo.s3.amazonaws.com/data/ecmwf_ifs025/static/HSURF.om}"
ECMWF025_STATIC_SHA256="${OM_ECMWF025_STATIC_SHA256:-935d56ba000b438b61504fbc271bfaa8f70db2acb541d58d5b466a24d294a9fb}"
ECMWF025_STATIC_PATH="$MODEL_STATIC_ROOT/static/ecmwf_ifs025/HSURF.om"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
APP_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
BUILD_TARGET_DIR="${OM_API_CARGO_TARGET_DIR:-$APP_ROOT/target}"
if [[ "$BUILD_TARGET_DIR" != /* ]]; then
  echo "OM_API_CARGO_TARGET_DIR must be an absolute path: $BUILD_TARGET_DIR" >&2
  exit 2
fi
BUILD_BINARY="$BUILD_TARGET_DIR/release/om-api"
MATERIALIZER_BUILD_BINARY="$BUILD_TARGET_DIR/release/om-native-materialize"
if [[ "$API_DEM_ROOT" != /* ]]; then
  echo "OM_API_DEM_ROOT must be an absolute path: $API_DEM_ROOT" >&2
  exit 2
fi
if [[ "$MODEL_STATIC_ROOT" != /* ]]; then
  echo "OM_API_MODEL_STATIC_ROOT must be an absolute path: $MODEL_STATIC_ROOT" >&2
  exit 2
fi
if [ -n "$STRICT_DATA_ROOT" ]; then
  if ! mountpoint -q -- "$STRICT_DATA_ROOT"; then
    echo "strict data root is not mounted: $STRICT_DATA_ROOT" >&2
    exit 1
  fi
  if [ "$(stat -c %d /)" = "$(stat -c %d "$STRICT_DATA_ROOT")" ]; then
    echo "strict data root shares the system filesystem: $STRICT_DATA_ROOT" >&2
    exit 1
  fi
  strict_real="$(readlink -f -- "$STRICT_DATA_ROOT")"
  data_real="$(readlink -m -- "$DATA_ROOT")"
  case "$data_real" in
    "$strict_real"|"$strict_real"/*) ;;
    *)
      echo "OM data root escapes strict data root: $DATA_ROOT" >&2
      exit 1
      ;;
  esac
fi
DEM_STATIC_DIR="$API_DEM_ROOT/copernicus_dem90/static"
BIN_DIR="$INSTALL_DIR/bin"
NATIVE_DIR="$INSTALL_DIR/native"
SOURCE_REVISION_FILE="$INSTALL_DIR/source-revision"
SOURCE_ARCHIVE_DIR="$INSTALL_DIR/source-archives"
ENV_FILE="$INSTALL_DIR/${SERVICE_NAME}.env"
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
PINNED_OM_FILE_FORMAT_REF="71f422b2706d8a81f1cecf52ae3073990de1ddbe"

resolve_source_revision() {
  local requested_revision="${OM_API_SOURCE_REVISION:-}"
  local resolved_revision=""
  local repository_revision=""
  local worktree_status=""

  if command -v git >/dev/null 2>&1 \
    && git -C "$APP_ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    repository_revision="$(git -C "$APP_ROOT" rev-parse --verify 'HEAD^{commit}')"
    worktree_status="$(git -C "$APP_ROOT" status --porcelain=v1 --untracked-files=all)"
    if [[ -n "$worktree_status" ]]; then
      echo "refusing to deploy from a dirty source worktree: $APP_ROOT" >&2
      printf '%s\n' "$worktree_status" >&2
      return 1
    fi
    if [[ -n "$requested_revision" ]] \
      && [[ "$requested_revision" != "$repository_revision" ]]; then
      echo "OM_API_SOURCE_REVISION does not match source HEAD: requested=$requested_revision head=$repository_revision" >&2
      return 1
    fi
    resolved_revision="${requested_revision:-$repository_revision}"
    if ! git -C "$APP_ROOT" rev-parse --verify "${resolved_revision}^{commit}" >/dev/null 2>&1; then
      echo "source revision is not resolvable in the repository: $resolved_revision" >&2
      return 1
    fi
  else
    if [[ -z "$requested_revision" ]]; then
      echo "OM_API_SOURCE_REVISION is required when APP_ROOT is not a Git worktree: $APP_ROOT" >&2
      return 1
    fi
    resolved_revision="$requested_revision"
  fi

  if [[ ! "$resolved_revision" =~ ^[0-9a-f]{40}$ ]]; then
    echo "source revision must be a full lowercase 40-character Git SHA: $resolved_revision" >&2
    return 1
  fi
  printf '%s\n' "$resolved_revision"
}

SOURCE_REVISION="$(resolve_source_revision)"
BUILD_REVISION="$SOURCE_REVISION"
archive_name="om_weather_server-${SOURCE_REVISION}.tar.gz"

validate_required_dem_chunks() {
  if [ ! -d "$DEM_STATIC_DIR" ]; then
    echo "Copernicus DEM90 static directory does not exist: $DEM_STATIC_DIR" >&2
    exit 1
  fi

  local latitude
  local chunk_path
  for ((latitude = DEM_LATITUDE_CHUNK_MIN; latitude <= DEM_LATITUDE_CHUNK_MAX; latitude++)); do
    chunk_path="$DEM_STATIC_DIR/lat_${latitude}.om"
    if [ ! -f "$chunk_path" ] || [ ! -s "$chunk_path" ]; then
      echo "required Copernicus DEM90 chunk is missing or empty: $chunk_path" >&2
      exit 1
    fi
  done
}

validate_required_dem_chunks

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required. Install Rust first, for example with rustup." >&2
  exit 1
fi

if ! command -v cc >/dev/null 2>&1; then
  echo "cc is required to build the native om-file-format codec." >&2
  exit 1
fi

if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required to install official static model data." >&2
  exit 1
fi

if ! command -v sha256sum >/dev/null 2>&1; then
  echo "sha256sum is required to verify official static model data." >&2
  exit 1
fi

if [ ! -d "$DATA_ROOT" ]; then
  echo "OM data root does not exist: $DATA_ROOT" >&2
  exit 1
fi

if [ ! -d "$API_DEM_ROOT/copernicus_dem90/static" ]; then
  echo "Copernicus DEM root does not exist: $API_DEM_ROOT/copernicus_dem90/static" >&2
  echo "Set OM_API_DEM_ROOT to the shared DEM root before installing the API." >&2
  exit 1
fi

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  SUDO="sudo"
fi

mkdir -p "$BIN_DIR" "$NATIVE_DIR"

run_privileged() {
  if [ -n "$SUDO" ]; then
    "$SUDO" "$@"
  else
    "$@"
  fi
}

run_privileged install -d -m 0755 "$MODEL_STATIC_ROOT"
system_device="$(stat -c %d /)"
for fixed_root in "$API_DEM_ROOT" "$MODEL_STATIC_ROOT"; do
  if [ "$(stat -c %d "$fixed_root")" != "$system_device" ]; then
    echo "fixed model/static root must use the system filesystem: $fixed_root" >&2
    exit 1
  fi
done

install_corresponding_source_archive() (
  set -euo pipefail

  local archive_tmp=""
  local archive_staged=""
  local checksum_tmp=""
  local checksum_staged=""
  local actual_sha256=""
  local supplied_archive="${OM_API_SOURCE_ARCHIVE:-}"
  local supplied_sha256="${OM_API_SOURCE_ARCHIVE_SHA256:-}"

  cleanup() {
    set +e
    if [ -n "$archive_tmp" ]; then
      rm -f -- "$archive_tmp"
    fi
    if [ -n "$checksum_tmp" ]; then
      rm -f -- "$checksum_tmp"
    fi
    if [ -n "$archive_staged" ]; then
      run_privileged rm -f -- "$archive_staged"
    fi
    if [ -n "$checksum_staged" ]; then
      run_privileged rm -f -- "$checksum_staged"
    fi
  }
  trap cleanup EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM

  archive_tmp="$(mktemp)"
  if command -v git >/dev/null 2>&1 \
    && git -C "$APP_ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    if ! command -v gzip >/dev/null 2>&1; then
      echo "gzip is required to build the corresponding source archive" >&2
      exit 1
    fi
    git -C "$APP_ROOT" archive \
      --format=tar \
      --prefix="om_weather_server-${SOURCE_REVISION}/" \
      "$SOURCE_REVISION" \
      | gzip -n -9 > "$archive_tmp"
  else
    if [ -z "$supplied_archive" ] || [ -z "$supplied_sha256" ]; then
      echo "OM_API_SOURCE_ARCHIVE and OM_API_SOURCE_ARCHIVE_SHA256 are required outside a Git worktree" >&2
      exit 1
    fi
    if [ ! -f "$supplied_archive" ] || [ ! -s "$supplied_archive" ]; then
      echo "supplied corresponding source archive is missing or empty: $supplied_archive" >&2
      exit 1
    fi
    if [[ ! "$supplied_sha256" =~ ^[0-9a-f]{64}$ ]]; then
      echo "OM_API_SOURCE_ARCHIVE_SHA256 must be a lowercase SHA-256 digest" >&2
      exit 1
    fi
    cp -- "$supplied_archive" "$archive_tmp"
    actual_sha256="$(sha256sum -- "$archive_tmp" | awk '{print $1}')"
    if [ "$actual_sha256" != "$supplied_sha256" ]; then
      echo "supplied corresponding source archive checksum mismatch: $actual_sha256" >&2
      exit 1
    fi
  fi

  actual_sha256="$(sha256sum -- "$archive_tmp" | awk '{print $1}')"
  checksum_tmp="$(mktemp)"
  printf '%s  %s\n' "$actual_sha256" "$archive_name" > "$checksum_tmp"

  run_privileged install -d -m 0755 "$SOURCE_ARCHIVE_DIR"
  archive_staged="$(run_privileged mktemp "$SOURCE_ARCHIVE_DIR/.${archive_name}.tmp.XXXXXX")"
  checksum_staged="$(run_privileged mktemp "$SOURCE_ARCHIVE_DIR/.${archive_name}.sha256.tmp.XXXXXX")"
  run_privileged install -m 0644 -- "$archive_tmp" "$archive_staged"
  run_privileged install -m 0644 -- "$checksum_tmp" "$checksum_staged"
  if [ "$(run_privileged sha256sum -- "$archive_staged" | awk '{print $1}')" != "$actual_sha256" ]; then
    echo "staged corresponding source archive checksum mismatch" >&2
    exit 1
  fi
  run_privileged mv -f -- "$archive_staged" "$SOURCE_ARCHIVE_DIR/$archive_name"
  archive_staged=""
  run_privileged mv -f -- "$checksum_staged" "$SOURCE_ARCHIVE_DIR/$archive_name.sha256"
  checksum_staged=""
  printf 'source_archive=%s\n' "$SOURCE_ARCHIVE_DIR/$archive_name"
)

install_verified_static_asset() (
  set -euo pipefail

  if [ "$#" -ne 4 ]; then
    echo "usage: install_verified_static_asset LABEL URL SHA256 TARGET" >&2
    exit 2
  fi

  local label="$1"
  local source_url="$2"
  local expected_sha256="$3"
  local target_path="$4"
  local target_dir
  local download_tmp=""
  local staged_path=""
  local actual_sha256=""
  local remove_invalid_target=0

  if [[ ! "$expected_sha256" =~ ^[0-9a-f]{64}$ ]]; then
    echo "official $label static elevation SHA-256 is invalid: $expected_sha256" >&2
    exit 2
  fi

  cleanup() {
    set +e
    if [ -n "$download_tmp" ]; then
      rm -f -- "$download_tmp"
    fi
    if [ -n "$staged_path" ]; then
      run_privileged rm -f -- "$staged_path"
    fi
    if [ "$remove_invalid_target" -eq 1 ]; then
      run_privileged rm -f -- "$target_path"
    fi
  }
  trap cleanup EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM

  if [ -f "$target_path" ]; then
    actual_sha256="$(run_privileged sha256sum -- "$target_path" | awk '{print $1}')"
    if [ "$actual_sha256" = "$expected_sha256" ]; then
      printf 'verified=%s\n' "$target_path"
      exit 0
    fi
    remove_invalid_target=1
  fi

  target_dir="$(dirname -- "$target_path")"
  run_privileged install -d -m 0755 "$target_dir"
  download_tmp="$(mktemp)"
  curl --fail --location --silent --show-error --retry 4 \
    --connect-timeout 15 --max-time 180 \
    --output "$download_tmp" "$source_url"

  actual_sha256="$(sha256sum -- "$download_tmp" | awk '{print $1}')"
  if [ "$actual_sha256" != "$expected_sha256" ]; then
    echo "official $label static elevation checksum mismatch: $actual_sha256" >&2
    exit 1
  fi

  staged_path="$(run_privileged mktemp "${target_path}.tmp.XXXXXX")"
  run_privileged install -m 0644 "$download_tmp" "$staged_path"
  actual_sha256="$(run_privileged sha256sum -- "$staged_path" | awk '{print $1}')"
  if [ "$actual_sha256" != "$expected_sha256" ]; then
    echo "staged $label static elevation checksum mismatch: $actual_sha256" >&2
    exit 1
  fi

  run_privileged mv -f -- "$staged_path" "$target_path"
  staged_path=""
  remove_invalid_target=0
  printf 'installed=%s\n' "$target_path"
)

install_verified_static_asset \
  "GFS013" "$GFS013_STATIC_URL" "$GFS013_STATIC_SHA256" "$GFS013_STATIC_PATH"
install_verified_static_asset \
  "GFS025" "$GFS025_STATIC_URL" "$GFS025_STATIC_SHA256" "$GFS025_STATIC_PATH"
install_verified_static_asset \
  "ECMWF025" "$ECMWF025_STATIC_URL" "$ECMWF025_STATIC_SHA256" "$ECMWF025_STATIC_PATH"

install_corresponding_source_archive

OM_FILE_FORMAT_REF="${OM_FILE_FORMAT_REF:-$PINNED_OM_FILE_FORMAT_REF}"
OM_FILE_FORMAT_REVISION_FILE="$NATIVE_DIR/om-file-format.source-revision"
INSTALLED_OM_FILE_FORMAT_REVISION=""
if [ -f "$OM_FILE_FORMAT_REVISION_FILE" ]; then
  INSTALLED_OM_FILE_FORMAT_REVISION="$(tr -d '\r\n' < "$OM_FILE_FORMAT_REVISION_FILE")"
fi
if [ "${OM_REBUILD_OMFILE:-0}" = "1" ] \
  || [ -n "${OM_FILE_FORMAT_SRC:-}" ] \
  || [ ! -f "$NATIVE_DIR/libomfileformat.so" ] \
  || [ "$INSTALLED_OM_FILE_FORMAT_REVISION" != "$OM_FILE_FORMAT_REF" ]; then
  bash "$APP_ROOT/scripts/build_omfileformat_decoder.sh" "$NATIVE_DIR"
else
  bash "$APP_ROOT/scripts/build_omfileformat_decoder.sh" \
    --verify "$NATIVE_DIR/libomfileformat.so"
  echo "reusing=$NATIVE_DIR/libomfileformat.so"
fi

OM_BUILD_REVISION="$BUILD_REVISION" cargo build --release \
  --bin om-api \
  --bin om-native-materialize \
  --manifest-path "$APP_ROOT/om_api/Cargo.toml" \
  --target-dir "$BUILD_TARGET_DIR"
if [ ! -f "$BUILD_BINARY" ]; then
  echo "cargo build completed but om-api binary is missing: $BUILD_BINARY" >&2
  exit 1
fi
if [ ! -f "$MATERIALIZER_BUILD_BINARY" ]; then
  echo "cargo build completed but om-native-materialize binary is missing: $MATERIALIZER_BUILD_BINARY" >&2
  exit 1
fi
install -m 0755 -- "$BUILD_BINARY" "$BIN_DIR/om-api"
install -m 0755 -- "$MATERIALIZER_BUILD_BINARY" "$BIN_DIR/om-native-materialize"

BUILD_INFO_TMP="$(mktemp "$BIN_DIR/.om-api.build-info.tmp.XXXXXX")"
cleanup_build_info_tmp() {
  rm -f -- "$BUILD_INFO_TMP"
}
trap cleanup_build_info_tmp EXIT
{
  printf 'git_revision=%s\n' "$SOURCE_REVISION"
  printf 'binary_sha256=%s\n' "$(sha256sum -- "$BIN_DIR/om-api" | awk '{print $1}')"
  printf 'built_at_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "$BUILD_INFO_TMP"
chmod 0644 "$BUILD_INFO_TMP"
mv -f -- "$BUILD_INFO_TMP" "$BIN_DIR/om-api.build-info"
BUILD_INFO_TMP=""
trap - EXIT

SOURCE_REVISION_TMP="$(mktemp "$INSTALL_DIR/.source-revision.tmp.XXXXXX")"
cleanup_source_revision_tmp() {
  rm -f -- "$SOURCE_REVISION_TMP"
}
trap cleanup_source_revision_tmp EXIT
printf '%s\n' "$SOURCE_REVISION" > "$SOURCE_REVISION_TMP"
chmod 0644 "$SOURCE_REVISION_TMP"
mv -f -- "$SOURCE_REVISION_TMP" "$SOURCE_REVISION_FILE"
SOURCE_REVISION_TMP=""
trap - EXIT

ENV_FILE_TMP="$(mktemp)"
cleanup_env_file_tmp() {
  rm -f -- "$ENV_FILE_TMP"
}
trap cleanup_env_file_tmp EXIT
cat > "$ENV_FILE_TMP" <<EOF
OM_DATA_ROOT=$DATA_ROOT
OM_DEM_ROOT=$API_DEM_ROOT
OM_MODEL_STATIC_ROOT=$MODEL_STATIC_ROOT
OM_API_BIND=$BIND_ADDR
OM_OMFILE_LIB=$NATIVE_DIR/libomfileformat.so
OM_SNAPSHOT_REFRESH_SECONDS=30
RUST_LOG=info,tower_http=warn
EOF
run_privileged install -m 0644 -- "$ENV_FILE_TMP" "$ENV_FILE"
rm -f -- "$ENV_FILE_TMP"
ENV_FILE_TMP=""
trap - EXIT

$SUDO tee "$SERVICE_FILE" >/dev/null <<EOF
[Unit]
Description=Open-Meteo point API
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$INSTALL_OWNER
Group=$INSTALL_OWNER
WorkingDirectory=$INSTALL_DIR
EnvironmentFile=$ENV_FILE
ExecStart=$BIN_DIR/om-api
Restart=always
RestartSec=3
LimitNOFILE=65536
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

$SUDO install -m 0644 "$APP_ROOT/nginx/om_client_api.conf" /etc/nginx/snippets/om_client_api.conf

if [ ! -e /etc/nginx/sites-enabled/om-client-api ]; then
  $SUDO tee /etc/nginx/sites-available/om-client-api >/dev/null <<'EOF'
server {
    listen 80 default_server;
    listen [::]:80 default_server;
    server_name _;

    include /etc/nginx/snippets/om_client_api.conf;

    location / {
        return 404;
    }
}
EOF
  $SUDO ln -s /etc/nginx/sites-available/om-client-api /etc/nginx/sites-enabled/om-client-api
fi

$SUDO systemctl daemon-reload
$SUDO systemctl enable "$SERVICE_NAME"
$SUDO systemctl restart "$SERVICE_NAME"
$SUDO nginx -t
$SUDO systemctl reload nginx

echo "installed=$INSTALL_DIR"
echo "service=$SERVICE_NAME"
echo "bind=$BIND_ADDR"
echo "build_revision=$BUILD_REVISION"
echo "source_archive=$SOURCE_ARCHIVE_DIR/$archive_name"
echo "source_revision=$SOURCE_REVISION"
echo "source_revision_file=$SOURCE_REVISION_FILE"
