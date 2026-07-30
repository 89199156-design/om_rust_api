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

The production download job for each model invokes `run_scope.sh` only after
that same job has downloaded, generated, and published its OM point release.
WebP is therefore the final stage of one continuous production pipeline, not a
separate polling job. The legacy 1Panel rows `OM_GFS_WEBP_BUILD`,
`OM_CAMS_WEBP_BUILD`, and `OM_ECMWF_WEBP_BUILD` remain disabled for operational
history and cannot render independently. A download job is successful only
after the WebP marker references the same immutable source `release_id`.

Production binaries are built and installed from a clean Git worktree with:

```bash
/usr/bin/env bash webp/om_webp/scripts/install_om_webp.sh
```

The installer records the exact Git revision and SHA-256 identities under the
production application directory.

### Two-disk nodes

`OM_DATA_ROOT` is a read-only OM source and may reside on the system disk.
`OM_WEBP_DATA_ROOT` contains staging, immutable releases, and current markers;
it must reside on the same separately mounted filesystem as
`OM_STRICT_DATA_ROOT`. It may be addressed directly below that root or through
a verified bind mount. This permits small nodes to keep GFS/CAMS source bundles
on the system disk while writing all large WebP artifacts to a data disk. The
renderer never mutates `OM_DATA_ROOT`.

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
