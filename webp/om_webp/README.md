# OM WebP Renderer

Rust renderer for the Shanghai production node. It reads `/data/om_raw`
directly through the same `om-api` snapshot, interpolation, product-mixing and
weather-code code paths used by the point API. It never calls the HTTP API.

The production inventory contains 26 GFS layers, 4 CAMS layers, and 18 ECMWF
IFS 0.25 degree layers. Each product renders exactly 121 WebP files per variable
at one-hour intervals, from latest run hour 0 through hour 120. The longer OM
windows are used by the database and API, not by WebP. Images use lossless RGBA
WebP with the published scalar/vector encoding contract.

ECMWF uses the same layer names, units, precision, and pixel encoding as the
equivalent GFS product. The free deterministic IFS025 long-cycle feed publishes
10-metre gusts, 100-metre wind, and surface temperature. It does not publish
visibility, UV index, GFS 80/120-metre fields, or freezing-level height, so
those layers are intentionally absent from `ecmwf_ifs025`; they are never
synthesized. The feed also has no raw showers field. Weather code and
precipitation phase use the ECMWF derivation path in `om-api`.

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
servers. `OM_WEBP_WORKERS` can override the limit; `--workers 0` deliberately
uses every available CPU and is reserved for isolated/offline rendering.

```bash
/usr/bin/env bash /opt/1panel/apps/weather_om_webp/scripts/run_scope.sh gfs
/usr/bin/env bash /opt/1panel/apps/weather_om_webp/scripts/run_scope.sh cams
```

The native 1Panel jobs `OM_GFS_WEBP_BUILD` and `OM_CAMS_WEBP_BUILD` are
scheduled only by 1Panel. Their panel scripts query `agent.db` before starting;
an older active instance or any running OM download/WebP peer exits
successfully without creating a separate lock. They compare the source
`release_id` with the local completion marker, so unchanged releases also exit
immediately without loading the OM snapshot.

Production binaries are built and installed from a clean Git worktree with:

```bash
/usr/bin/env bash webp/om_webp/scripts/install_om_webp.sh
```

The installer records the exact Git revision and SHA-256 identities under the
production application directory.

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
