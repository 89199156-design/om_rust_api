# OM WebP Renderer

Rust renderer for the Shanghai production node. It reads `/data/om_raw`
directly through the same `om-api` snapshot, interpolation, product-mixing and
weather-code code paths used by the point API. It never calls the HTTP API.

The production inventory matches Singapore: 18 GFS layers and 4 CAMS layers,
121 hourly frames, and a 597x495 north-to-south regional grid over 70E-140E,
0N-58N. Images use lossless RGBA WebP with the published scalar/vector encoding
contract.

Each source `release_id` is built under `data/staging`. A complete immutable
release is moved to `data/releases`, then the public product symlink is switched
atomically. Existing public data remains available while a new release builds.
If the source release changes during a build, staging is discarded and nothing
is published.

## Runtime

`read_variable_grid` decodes each required regional OM rectangle in one native
call. It then applies the same interpolation, derived-variable formulas and JSON
output precision as the point API. Frames are processed with a bounded Rayon
pool; production defaults to two workers to remain suitable for lightweight
servers.

```bash
/usr/bin/env bash /opt/1panel/apps/weather_om_webp/scripts/run_scope.sh gfs
/usr/bin/env bash /opt/1panel/apps/weather_om_webp/scripts/run_scope.sh cams
```

The native 1Panel jobs `OM_GFS_WEBP_BUILD` and `OM_CAMS_WEBP_BUILD` run every
five minutes. They compare the source `release_id` with the local completion
marker, so unchanged releases exit immediately without loading the OM snapshot.

## Verification

```bash
python3 /opt/1panel/apps/weather_om_webp/scripts/verify_deployment.py
/opt/1panel/apps/weather_om_webp/bin/om-grid-verify \
  --scope gfs \
  --data-root /data/om_raw \
  --decoder-lib /opt/1panel/apps/weather_om_api/native/libomfileformat.so \
  --time 2026-07-12T06:00:00Z \
  --samples 64
```

The source tree is deployed under `source/{om_api,om_webp}`. Both crates use
`AGPL-3.0-or-later`; the renderer directly links the API query implementation,
so corresponding source must be published together when the service is
distributed or offered over a network.
