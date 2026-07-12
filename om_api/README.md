# Shanghai OM Point API

License: `AGPL-3.0-or-later`.

This Rust server is the Shanghai client-facing point API. It reads mirrored
Open-Meteo `.omranges` bundles from `/data/om_raw` and returns point JSON
directly. Clients do not fetch `latest.json` or any manifest before requesting
point data.

Official logic baseline:

- Open-Meteo upstream: `4efb9c49fb4a3718ed385fb22580d2e0fc56bdb2`
- Singapore production image: `weather-forecast-openmeteo:9849315`

Implementation rule:

- Supported API behavior must be behavior-equivalent to the recorded
  Open-Meteo source baseline. Rust may use faster implementation details for
  indexing, caching, IO, memory layout, and concurrency, but the returned point
  values and API semantics must match the official baseline within agreed
  tolerance.
- If a variable, derived field, grid-selection mode, or elevation/DEM behavior
  has not been ported and validated against that baseline, the service must
  reject the request instead of returning approximate data.
- Do not mix logic from multiple Open-Meteo source versions. Upgrade the
  baseline as a whole and revalidate.

The service uses official Open-Meteo OM decoder functions when `OM_OMFILE_LIB`
points to a shared library exporting the `om-file-format` C symbols.

Build that decoder on Ubuntu:

```bash
cd /opt/1panel/apps/weather_om_downloader
bash scripts/build_omfileformat_decoder.sh /opt/1panel/apps/weather_om_api/native
```

If an exact approved `om-file-format` source checkout is available, pass it with
`OM_FILE_FORMAT_SRC=/path/to/om-file-format`. This keeps the decoder aligned with
the Singapore/Open-Meteo baseline instead of silently mixing source versions.

Example:

```bash
OM_DATA_ROOT=/data/om_raw \
OM_OMFILE_LIB=/opt/1panel/apps/weather_om_api/native/libomfileformat.so \
/opt/1panel/apps/weather_om_api/om-api --bind 127.0.0.1:8088
```

Client examples:

```text
GET /v1/forecast?latitude=31.23&longitude=121.47&hourly=temperature_2m,weather_code&forecast_hours=24
GET /v1/air-quality?latitude=31.23&longitude=121.47&hourly=pm2_5,pm10,aerosol_optical_depth&forecast_hours=24
GET /v1/air-quality?latitude=31.23&longitude=121.47&hourly=chinese_aqi&daily=chinese_aqi,chinese_aqi_o3&start_date=2026-07-12&end_date=2026-07-12
POST /v1/route
```

Chinese AQI:

- `hourly=chinese_aqi,...` implements the HJ 633-2026 hourly forecast method:
  it uses the six one-hour pollutant concentrations, applies the current
  breakpoint tables, and rounds the IAQI/AQI up to an integer.
- `daily=chinese_aqi,...` uses China Standard Time natural days. It uses the
  SO2/NO2/CO/PM10/PM2.5 daily means and the daily maximum O3 trailing
  eight-hour mean. Pollutant concentrations are rounded according to the
  standard before IAQI calculation.
- The O3 window can require the seven hours before a China-local midnight.
  The service reads those hours from retained, complete CAMS group releases;
  it never combines partial download payloads. A day is omitted until its
  full required window is present.

DEM and land-cell selection:

- The current service only supports `cell_selection=nearest`.
- `cell_selection=land` is intentionally rejected until DEM/static grid
  selection data is wired in.
- When DEM is needed, first copy the Singapore production DEM/static grid data
  that belongs to the recorded Open-Meteo baseline.
- If that DEM package is not suitable for the Rust service format, resolution,
  projection, or baseline version, fetch a new compatible DEM package through
  the Silicon Valley download server, then deploy the verified copy to Shanghai.
