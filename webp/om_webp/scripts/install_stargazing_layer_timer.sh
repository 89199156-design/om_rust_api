#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
UNIT_DIR="$(cd "$SCRIPT_DIR/../systemd" && pwd -P)"
LIGHT_CACHE="/opt/1panel/apps/weather_om_webp/static/stargazing-light/manifest.json"

if [[ ! -f "$LIGHT_CACHE" ]]; then
  echo "stargazing light cache is missing: $LIGHT_CACHE" >&2
  exit 1
fi
python3 -m py_compile "$SCRIPT_DIR/build_stargazing_layers.py"
sudo install -m 0644 "$UNIT_DIR/weather-stargazing-layer.service" /etc/systemd/system/weather-stargazing-layer.service
sudo install -m 0644 "$UNIT_DIR/weather-stargazing-layer.timer" /etc/systemd/system/weather-stargazing-layer.timer
sudo systemctl daemon-reload
sudo systemctl enable --now weather-stargazing-layer.timer
sudo systemctl start weather-stargazing-layer.service
sudo systemctl --no-pager --full status weather-stargazing-layer.timer

