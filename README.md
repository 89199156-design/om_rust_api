# OM Weather Pipeline

This repository is the single source of truth for the Shanghai production
weather pipeline. It contains:

- `om_api/`: Rust point forecast, daily aggregation, soil, and air-quality API.
- `downloader/`: Python GFS/CAMS range downloader and mirror synchronization.
- `webp/om_webp/`: Rust WebP grid renderer and verification tools.

The repository root is a Cargo workspace containing the API and WebP crates.
WebP uses the in-repository `om-api` crate through a path dependency, so a
single Git commit identifies the complete production source state. The
standalone `om_downloader_sh` and `om_weather_webp` repositories are retired
and must not be used for production changes.

## Production storage layout

- `/opt/1panel/apps/weather_om_api/static` stores immutable, checksum-verified
  Copernicus DEM90 chunks plus GFS/ECMWF `HSURF.om` model-elevation grids on
  the system disk.
- `/data/om_downloader`, `/data/om_raw`, `/data/om_webp`, and
  `/data/validation` store growing cycle payloads and validation evidence on the
  dedicated data disk.
- Native GFS coverage markers reference DEM90 through `OM_DEM_ROOT`; all model
  groups reference fixed HSURF grids through `OM_MODEL_STATIC_ROOT`. Neither
  fixed dataset is copied into growing `/data` cycle directories.
- Production download and WebP jobs require `/data` to be a distinct mounted
  filesystem and preserve at least 10 GiB free. A failed preflight leaves the
  previously published immutable batch active and never spills into `/`.

## License, deployed source, and data attribution

Repository source is offered under `AGPL-3.0-or-later`; see [`LICENSE`](LICENSE)
and [`NOTICE.md`](NOTICE.md). A production API exposes `/v1/source` with its
compile-time Git revision, the corresponding tracked-source archive, and the
archive SHA-256 file. The installer refuses a dirty checkout before building,
so the revision named by the service and the archived source describe the same
tracked tree.

Weather and terrain data retain their providers' terms. Source URLs,
transformations, attribution text, and known provenance limitations are in
[`DATA_SOURCES.md`](DATA_SOURCES.md); third-party software is recorded in
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md). The public API also exposes
`/.well-known/weather-attribution.json`, and ECMWF WebP catalogs/manifests
carry machine-readable attribution.

The full `om-file-format` decoder is pinned and emits build metadata and an
artifact hash. Its upstream `GPL-2.0-only` license has an unresolved
compatibility question with this project's `AGPL-3.0-or-later` service.
Project policy therefore limits it to server-internal use and forbids placing
it in distributed clients, SDKs, containers, or third-party native packages
until upstream clarification or qualified legal review resolves that issue.

## Singapore native OM API

The Rust API can read the producer's `openmeteo-native-v1` coverages directly.
GFS is loaded as five independent candidates in newest-first query order: the
latest complete run, the previous complete run, and three strict `f000...f005`
runs. A non-finite value in the latest complete run falls back only to the same
valid time in the previous complete run; the forecast tail covered only by the
latest run never falls back. CAMS retains three complete runs but likewise uses
only the immediately previous run as a null fallback.

API requests never scan or rebuild the snapshot. After OM and WebP publication
finish, the pipeline sends one `SIGHUP`; the API builds the replacement snapshot
in a background worker and atomically swaps it only after successful validation.
The GFS cell-selection path decodes only the required regional window from the
checksum-pinned global `HSURF.om` files on the system disk. Native cycle
coverages contain no model-elevation or Copernicus DEM90 copies.

Silicon Valley is a generic download gateway, not an Open-Meteo runtime. It must not run Open-Meteo containers, Swift services, WebP builders, Shanghai package builders, or business parsers.

Local test command:

```powershell
cd D:\Projects\om_weather_pipeline
python -m unittest discover -s tests -p "test_*.py" -v
```

No-git deployment package:

```powershell
cd D:\Projects\om_weather_pipeline
python -m om_downloader.deploy_package --root . --output D:\Projects\weather_om_downloader_deploy.zip
```

Upload `weather_om_downloader_deploy.zip` to the server, extract it to `/opt/1panel/apps/weather_om_downloader`, then run the native decoder build command below. Runtime `data/`, Python cache files, and local build outputs are intentionally excluded from the zip.

On the server, after uploading the zip to `/tmp/weather_om_downloader_deploy.zip`, extract the install script from the zip and run it:

```bash
unzip -p /tmp/weather_om_downloader_deploy.zip scripts/install_from_zip.sh > /tmp/install_from_zip.sh && bash /tmp/install_from_zip.sh /tmp/weather_om_downloader_deploy.zip /opt/1panel/apps/weather_om_downloader
```

If the package has already been extracted into a temporary directory, run:

```bash
bash scripts/install_from_zip.sh /tmp/weather_om_downloader_deploy.zip /opt/1panel/apps/weather_om_downloader
```

Silicon Valley validation commands:

```bash
cd /opt/1panel/apps/weather_om_downloader && /usr/bin/python3 -m om_downloader.cli --inspect-product-catalog gfs013_surface --config config/models.json
cd /opt/1panel/apps/weather_om_downloader && /usr/bin/python3 -m om_downloader.cli --inspect-product-catalog gfs025 --config config/models.json
cd /opt/1panel/apps/weather_om_downloader && /usr/bin/python3 -m om_downloader.cli --inspect-product-catalog gfs_pressure_profile --config config/models.json
cd /opt/1panel/apps/weather_om_downloader && /usr/bin/python3 -m om_downloader.cli --inspect-product-catalog cams_global --config config/models.json
```

These are validation commands, not the final scheduled download commands. Do not create 1Panel scheduled jobs until the product command downloads ranged `.om` data and publishes a complete `latest.json`.

Install Silicon Valley 1Panel v2 plan tasks:

```bash
cd /opt/1panel/apps/weather_om_downloader && sudo /usr/bin/python3 scripts/install_1panel_v2_cronjobs.py --role silicon-valley && sudo 1pctl restart agent
```

This creates the panel-visible tasks `OM_GFS_DOWNLOAD` and `OM_CAMS_DOWNLOAD` on staggered 1Panel schedules. It writes 1Panel v2 task rows to `/opt/1panel/db/agent.db`, not system `cron`.

Silicon Valley 1Panel download commands:

```bash
cd /opt/1panel/apps/weather_om_downloader && OM_TURBOPFOR_LIB=/opt/1panel/apps/weather_om_downloader/native/libom_turbopfor.so /usr/bin/python3 -m om_downloader.cli --download-openmeteo-group gfs --config config/models.json --output data --now "$(date -u +%Y-%m-%dT%H:00:00Z)"
cd /opt/1panel/apps/weather_om_downloader && OM_TURBOPFOR_LIB=/opt/1panel/apps/weather_om_downloader/native/libom_turbopfor.so /usr/bin/python3 -m om_downloader.cli --download-openmeteo-group cams --config config/models.json --output data --now "$(date -u +%Y-%m-%dT%H:00:00Z)"
```

If the 1Panel installer script is not used, create these as two separate 1Panel plan tasks. Do not put them in system `cron` or `systemd timer`.
Production 1Panel commands should use `--download-workers 6` but should not set `--range-io-size-max` for the OM main channel. Workers parallelise metadata/LUT planning and Range reads; they do not split output into small files. Each product writes one cropped coverage bundle, and artificial 4 MiB splitting would reintroduce excessive HTTP Range overhead.
The 1Panel scripts query `/opt/1panel/db/agent.db` before downloading. If the peer GFS/CAMS task is already executing, the new invocation exits successfully without entering download logic. Production group commands do not create filesystem task locks.
`gfs` is complete only when `gfs013_surface`, `gfs025`, and `gfs_pressure_profile` all publish the same `latest_complete_run`; `cams` is complete independently from `cams_global`.
Range bundle downloads are restart-tolerant: each `.omranges` write is promoted atomically from a temporary file, and an existing complete bundle with the expected byte count is reused on rerun.

Before enabling the 1Panel tasks, build the native LUT decoder once:

```bash
cd /opt/1panel/apps/weather_om_downloader && bash scripts/build_turbopfor_decoder.sh /opt/1panel/apps/weather_om_downloader/native
```

Compliance note: the native decoder source includes GPL v2 headers. Confirm the commercial licensing posture before production use or redistribution.

Shanghai sync uses an SSH/rsync pull from Silicon Valley, then local manifest sync. This avoids exposing the Silicon Valley `published/` directory over public HTTP.

Prerequisites on Shanghai:

- The Shanghai server has SSH key access to `ubuntu@43.162.112.201`.
- `rsync` is installed.
- `/data/om_sv_published` and `/data/om_raw` are available or can be created.

Install Shanghai 1Panel v2 sync task:

```bash
cd /opt/1panel/apps/weather_om_downloader && sudo /usr/bin/python3 scripts/install_1panel_v2_cronjobs.py --role shanghai --sv-host ubuntu@43.162.112.201 --mirror-root /data/om_sv_published --raw-root /data/om_raw && sudo 1pctl restart agent
```

This creates the panel-visible task `OM_SYNC_FROM_SV`. The task first pulls only the Silicon Valley group manifest for `gfs` and `cams`. If the group batch already matches Shanghai's `/data/om_raw/groups/<group>/current/ready_for_processing.json`, it skips file mirroring for that group. If the batch differs, it mirrors the required product manifests and `.omranges` files into `/data/om_sv_published/`, then promotes the whole group into `/data/om_raw`:

```bash
cd /opt/1panel/apps/weather_om_downloader && /usr/bin/python3 -m om_downloader.cli --sync-openmeteo-group-from-mirror gfs --mirror-root /data/om_sv_published --output /data/om_raw
cd /opt/1panel/apps/weather_om_downloader && /usr/bin/python3 -m om_downloader.cli --sync-openmeteo-group-from-mirror cams --mirror-root /data/om_sv_published --output /data/om_raw
```

Shanghai stages every product in the group under `.incoming` first. It verifies size and sha256 for all referenced `.omranges` files, promotes product coverages only after staging succeeds, and writes the group `ready_for_processing.json` last. Each product manifest is mirrored as a whole, including all `entries` in the product bundle, so supplemental stale-run slots used to fill Shanghai/Xinjiang local-day coverage are synced together with the latest batch. Downstream Shanghai processing should use these group-level ready files so it never builds or serves GFS from only one refreshed GFS resolution. Product-level `ready_for_processing.json` files include `status`, `latest_complete_run`, `files`, and `bytes` for the individual product coverage.

The single-product sync commands still exist for manual compatibility, but the production Shanghai 1Panel task uses group-level sync to avoid client-facing empty windows while Silicon Valley clears and refreshes its smaller disk. HTTP sync is still supported through `--sync-from-manifest-url`, but it is not the preferred production path because it requires a public static-file surface.

Shanghai OM client HTTP surface:

```text
GET /data/om/<product>/coverages/<coverage_id>/<path>.json
GET /data/om/<product>/coverages/<coverage_id>/<path>.omranges
GET /data/webp/<product>/<path>.json
GET /data/webp/<product>/<path>.webp
GET /data/webp/<package>/<path>.bin
```

Point clients should not use `/api/om/status` or product `current/latest.json` discovery. Those client-facing routes are intentionally not exposed. The next client protocol should use a fixed or server-assigned point/tile package URL. The current low-level OM HTTP surface only exposes explicit coverage files under `/data/om/<product>/coverages/...`; those files are `.omranges` range bundles produced from the original `.om` object, not global `.om` files. Map layers still use rendered products under `/data/webp/`.

Internal Shanghai sync still writes `/data/om_raw/<product>/current/ready_for_processing.json` and keeps internal product manifests for server-side builders. These internal files are not the public client discovery API.

Include `nginx/om_client_api.conf` under the existing 1Panel site `server {}` to expose `/data/om/` explicit coverage files and `/data/webp/` rendered files only.

Explicit HTTP Range download example:

```bash
cd /opt/1panel/apps/weather_om_downloader && /usr/bin/python3 -m om_downloader.cli --config config/models.json --model gfs025 --metadata metadata/gfs025.json --output data --now 2026-07-08T14:00:00Z --source-url https://example.invalid/object.om --byte-range 1024-2047 --byte-range 4096-8191
```

`--source-url` and `--byte-range` are the real download-gateway entrypoint once OM metadata/index has produced byte ranges. The CLI refuses implicit full-object downloads.

Remote OM metadata inspection example:

```bash
cd /opt/1panel/apps/weather_om_downloader && /usr/bin/python3 -m om_downloader.cli --inspect-om-url https://example.invalid/object.om
```

This prints `available_variables`, inferred `pressure_levels_hpa`, array dimensions, chunk sizes, and LUT offsets using HTTP Range only. Use it before finalizing required variables, pressure levels, and forecast-hour assumptions for a real OM object.

Open-Meteo public catalog inspection example:

```bash
cd /opt/1panel/apps/weather_om_downloader && /usr/bin/python3 -m om_downloader.cli --inspect-openmeteo-model ncep_gfs025
cd /opt/1panel/apps/weather_om_downloader && /usr/bin/python3 -m om_downloader.cli --inspect-product-catalog gfs025 --config config/models.json
```

`--inspect-openmeteo-model` reads `data_spatial/<model>/latest.json`. `--inspect-product-catalog` compares a configured product against actual Open-Meteo variables and reports missing required or optional variables. When `--now` is supplied, it also walks previous `meta.json` files as needed and prints the coverage source runs that fill UTC+8/UTC+6 local-day midnight gaps.

Remote OM range planning example:

```bash
cd /opt/1panel/apps/weather_om_downloader && /usr/bin/python3 -m om_downloader.cli --plan-om-ranges-url https://example.invalid/object.om --variable temperature_2m --selection 352:354 --selection 272:569 --selection 0:385
```

This reads remote metadata plus the needed LUT chunks and prints `lut_byte_ranges` and `data_byte_ranges`. Production `.om` LUT decoding defaults to TurboPFor and requires `OM_TURBOPFOR_LIB`; `--lut-codec plain` is only for fixtures or explicitly uncompressed test objects.

Implemented OM metadata layers:

- `om_catalog.py` reads Open-Meteo public `data_spatial/<model>/latest.json`, parses `reference_time`, `valid_times`, variables, max forecast hour, and builds `.om` object URLs under `data_spatial/<model>/YYYY/MM/DD/HH00Z/YYYY-MM-DDTHHMM.om`.
- `coverage.py` uses actual Open-Meteo `valid_times` when available. It does not fabricate hourly URLs after the model switches to sparse forecast intervals.
- `om_format.py` parses Open-Meteo `.om` v3 header, trailer, root variable, child variables, array dimensions, chunk dimensions, LUT offset, and LUT size. It also exposes single-variable metadata parsing so remote metadata can be read without downloading the full file.
- `om_format.py` also handles scalar metadata children such as string `unit` attributes, where the variable name appears after the scalar payload in the official OM v3 layout.
- `om_inventory.py` turns parsed arrays into available variable inventory and inferred pressure levels from variable names such as `temperature_850hPa`.
- `om_remote.py` reads remote `.om` metadata through byte ranges only: header probe, trailer, root variable metadata, and child variable metadata. `HttpByteRangeSource` uses HTTP `Range` and refuses non-range remote objects.
- `om_remote_ranges.py` reads only the needed remote LUT chunks for a variable/selection, decodes LUT offsets, and returns data byte ranges for download planning.
- `om_product_download.py` selects configured variables from each actual object inventory, computes padded region selections from the real Open-Meteo model grid, and plans `.omranges` bundles containing only required LUT and data byte ranges.
- `locking.py` provides a dependency-free file lock used by product downloads, so 1Panel cannot run the same product task concurrently.
- `shanghai_sync.py` reads Silicon Valley `latest.json` from either HTTP or a local rsync mirror, skips incomplete manifests, copies referenced `.omranges` files to `.incoming`, verifies sha256, promotes the coverage, and writes `current/ready_for_processing.json` for later Shanghai builders.
- `deploy_package.py` builds a no-git zip package for new servers, excluding runtime `data/`, caches, and build artifacts.
- `om_chunks.py` converts spatial/time selections into chunk index ranges.
- `om_lut.py` calculates compressed LUT byte ranges and converts decoded LUT offsets into data byte ranges. OM v3 compressed LUT decoding uses TurboPFor `p4nddec64`; this must be provided by a native decoder before production downloads can fully automate chunk-to-byte-range conversion.
- `om_byte_ranges.py` connects array metadata, selection ranges, LUT planning, and decoded offset tables.
- `om_native.py` loads an external shared library through `OM_TURBOPFOR_LIB` and calls `p4nddec64` through `ctypes`. See `docs/native_turbopfor.md`.

Rules:

- 1Panel plan tasks are the only scheduled entrypoint.
- Variables, pressure levels, and max forecast hours are determined from OM metadata at runtime.
- Forecast object lists are generated from actual `valid_times`; `forecast_hour_end=384` does not mean every hour exists to 384.
- `config/models.json` uses actual Open-Meteo variable names such as `temperature_2m`, `pressure_msl`, `wind_u_component_850hPa`, and `pm2_5`, not GRIB short names such as `TMP` or `UGRD`.
- Never download a global `.om` file to clip locally; calculate padded region ranges first.
- Product downloads publish `.omranges` files plus manifest byte-range metadata. Shanghai must treat these as range bundles, not full `.om` files.
- Product downloads record `missing_object_required_variables`; if a required variable is present in the run catalog but absent from an individual valid-time object, `status` is forced to `incomplete`.
- Publish only complete and checksummed `latest.json` manifests.
