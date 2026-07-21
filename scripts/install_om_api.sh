#!/usr/bin/env bash
set -euo pipefail

INSTALL_DIR="${1:-/opt/1panel/apps/weather_om_api}"
DATA_ROOT="${OM_DATA_ROOT:-/data/om_raw}"
BIND_ADDR="${OM_API_BIND:-127.0.0.1:8088}"
SERVICE_NAME="${OM_API_SERVICE_NAME:-weather-om-api}"
INSTALL_OWNER="${OM_API_USER:-ubuntu}"
GFS013_STATIC_URL="${OM_GFS013_STATIC_URL:-https://openmeteo.s3.amazonaws.com/data/ncep_gfs013/static/HSURF.om}"
GFS013_STATIC_SHA256="${OM_GFS013_STATIC_SHA256:-203745df4dfa10069e1a39206350e006818a0eea644bb19c1668c0f32f7475e0}"
GFS013_STATIC_PATH="$DATA_ROOT/static/ncep_gfs013/HSURF.om"
GFS025_STATIC_URL="${OM_GFS025_STATIC_URL:-https://openmeteo.s3.amazonaws.com/data/ncep_gfs025/static/HSURF.om}"
GFS025_STATIC_SHA256="${OM_GFS025_STATIC_SHA256:-fdd9587e606e64d6d85474c703b9898669d230aac1574fc460cc3087227e868d}"
GFS025_STATIC_PATH="$DATA_ROOT/static/ncep_gfs025/HSURF.om"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN_DIR="$INSTALL_DIR/bin"
NATIVE_DIR="$INSTALL_DIR/native"
ENV_FILE="$INSTALL_DIR/${SERVICE_NAME}.env"
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required. Install Rust first, for example with rustup." >&2
  exit 1
fi

if ! command -v cc >/dev/null 2>&1; then
  echo "cc is required to build the native om-file-format decoder." >&2
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

if [ "${OM_REBUILD_OMFILE:-0}" = "1" ] || [ ! -f "$NATIVE_DIR/libomfileformat.so" ]; then
  bash "$APP_ROOT/scripts/build_omfileformat_decoder.sh" "$NATIVE_DIR"
else
  echo "reusing=$NATIVE_DIR/libomfileformat.so"
fi

cargo build --release --manifest-path "$APP_ROOT/om_api/Cargo.toml"
install -m 0755 "$APP_ROOT/om_api/target/release/om-api" "$BIN_DIR/om-api"

cat > "$ENV_FILE" <<EOF
OM_DATA_ROOT=$DATA_ROOT
OM_API_BIND=$BIND_ADDR
OM_OMFILE_LIB=$NATIVE_DIR/libomfileformat.so
OM_SNAPSHOT_REFRESH_SECONDS=30
RUST_LOG=info,tower_http=warn
EOF

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
