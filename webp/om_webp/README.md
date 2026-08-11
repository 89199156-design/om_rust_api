# OM WebP Renderer

Shared Rust WebP renderer for the Shanghai and Singapore production nodes. It reads the same immutable OM snapshot as `om-api`, uses the shared query/interpolation/derived-variable implementation, and never calls the HTTP API.

The source path is selected by `OM_DATA_ROOT`:

- Shanghai: `/data/om_raw`, published by `om_data_om`.
- Singapore: the native OM producer root published by `om_data_raw`.

The renderer contains no downloader or raw-model conversion logic.

## Publication contract

Each source `release_id` is built under the configured WebP staging directory. A complete immutable release is moved to `releases/`, then the public/current marker is switched atomically. Existing images remain available while a new release builds. If the OM source identity changes during rendering, the staging release is discarded.

Production model tasks invoke WebP only after the matching OM batch is fully validated and published. The disabled legacy 1Panel WebP rows remain non-rendering historical records; there is no polling renderer or shadow release path.

The production inventory renders model-specific GFS, ECMWF and CAMS layers for the configured first 121 forecast frames. Lossless RGBA WebP encoding and scalar/vector contracts are shared by both servers. Current markers and product manifests include the renderer Git revision, so a production code upgrade rebuilds the same OM release instead of incorrectly skipping it.

## Runtime

`read_variable_grid_series` decodes ordinary GFS, ECMWF and CAMS source dependencies once for the complete 121-frame output window, then quantizes and writes them in bounded six-frame encoding blocks. The dependency-heavy GFS/ECMWF `weather_code` source group is also read in six-frame blocks so cloud, precipitation, snow and instability grids are not resident for the complete output window at the same time. This avoids repeatedly rebuilding native ECMWF run stitching for ordinary variables while bounding the one multi-dependency group.

Each production invocation runs in a transient systemd service with a 1536 MiB memory ceiling, no swap allocation, a 150% CPU quota and the validated production open-file limit. `OM_WEBP_MEMORY_MAX` and `OM_WEBP_CPU_QUOTA` are explicit operational overrides. The runner fails closed if the memory guard cannot be established, so a renderer regression is terminated without exhausting the production host. Two workers are used by default for GFS, ECMWF and CAMS; `OM_WEBP_WORKERS` can override the deployment default.

The progress reporter is part of this WebP component at `scripts/task_progress_reporter.py`; the renderer does not depend on an external downloader repository.

## Install

From a clean `om_rust_api` worktree:

```bash
bash webp/om_webp/scripts/install_om_webp.sh
```

The installer records the exact Git revision and SHA-256 of every deployed binary. `OM_WEBP_DATA_ROOT` must reside on the separately mounted filesystem selected by `OM_STRICT_DATA_ROOT`; source OM is read-only and may use another verified filesystem.

## Verify

```bash
python3 /opt/1panel/apps/weather_om_webp/scripts/verify_deployment.py \
  --raw-root "$OM_DATA_ROOT" \
  --webp-root "$OM_WEBP_DATA_ROOT" \
  --public-root /opt/1panel/apps/weather/data \
  --scope gfs
```

The renderer and API are `AGPL-3.0-or-later` and are published together as corresponding source from the `om_rust_api` repository.
