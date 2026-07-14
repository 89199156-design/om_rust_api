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
OM_FILE_FORMAT_REF="${OM_FILE_FORMAT_REF:-71f422b2706d8a81f1cecf52ae3073990de1ddbe}"

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

if [ ! -f "$GFS013_STATIC_PATH" ] || [ "$(sha256sum "$GFS013_STATIC_PATH" | awk '{print $1}')" != "$GFS013_STATIC_SHA256" ]; then
  static_tmp="$(mktemp)"
  trap 'rm -f "$static_tmp"' EXIT
  curl --fail --location --silent --show-error --retry 4 \
    --connect-timeout 15 --max-time 180 \
    --output "$static_tmp" "$GFS013_STATIC_URL"
  actual_static_sha256="$(sha256sum "$static_tmp" | awk '{print $1}')"
  if [ "$actual_static_sha256" != "$GFS013_STATIC_SHA256" ]; then
    echo "official GFS013 static elevation checksum mismatch: $actual_static_sha256" >&2
    exit 1
  fi
  $SUDO install -d -m 0755 "$(dirname "$GFS013_STATIC_PATH")"
  $SUDO install -m 0644 "$static_tmp" "$GFS013_STATIC_PATH"
  rm -f "$static_tmp"
  trap - EXIT
fi

if [ "${OM_REBUILD_OMFILE:-0}" = "1" ] || [ ! -f "$NATIVE_DIR/libomfileformat.so" ] || [ ! -f "$NATIVE_DIR/libomfileformat.build.json" ]; then
  OM_FILE_FORMAT_REF="$OM_FILE_FORMAT_REF" \
    bash "$APP_ROOT/scripts/build_omfileformat_decoder.sh" "$NATIVE_DIR"
else
  python3 - "$NATIVE_DIR/libomfileformat.so" "$NATIVE_DIR/libomfileformat.build.json" "$OM_FILE_FORMAT_REF" <<'PY'
import hashlib
import json
import sys

artifact_path, manifest_path, required_revision = sys.argv[1:]
with open(manifest_path, encoding="utf-8") as handle:
    manifest = json.load(handle)
if manifest.get("source_revision") != required_revision:
    raise SystemExit("existing decoder source revision does not match OM_FILE_FORMAT_REF")
digest = hashlib.sha256()
with open(artifact_path, "rb") as handle:
    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
        digest.update(chunk)
if manifest.get("artifact_sha256") != digest.hexdigest():
    raise SystemExit("existing decoder SHA-256 does not match build provenance")
PY
  echo "reusing=$NATIVE_DIR/libomfileformat.so"
fi

cargo build --release --manifest-path "$APP_ROOT/om_api/Cargo.toml"
install -m 0755 "$APP_ROOT/om_api/target/release/om-api" "$BIN_DIR/om-api"

cat > "$ENV_FILE" <<EOF
OM_DATA_ROOT=$DATA_ROOT
OM_API_BIND=$BIND_ADDR
OM_OMFILE_LIB=$NATIVE_DIR/libomfileformat.so
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
ExecReload=/bin/kill -HUP \$MAINPID
Restart=always
RestartSec=3
CPUWeight=1000
IOWeight=1000
OOMScoreAdjust=-500
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
