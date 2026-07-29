# Data sources, transformations, and attribution

This file records the data paths implemented by the repository. Provider terms
can change; release owners must review the linked current terms for every
production release. The links and statements below do not relicense provider
data.

## Open-Meteo distribution layer

The downloader reads regional byte ranges and static model files from
Open-Meteo object storage, including:

- `https://openmeteo.s3.amazonaws.com/data_spatial/`
- `https://openmeteo.s3.amazonaws.com/data/`

Open-Meteo license and terms pages:

- <https://open-meteo.com/en/license>
- <https://open-meteo.com/en/terms>

The product attribution used for Open-Meteo-distributed weather data begins
with **“Weather data by Open-Meteo.com.”**

## ECMWF IFS open data

- Local product keys: `ecmwf_ifs025`, `ecmwf_ifs025_ensemble`
- Underlying provider: European Centre for Medium-Range Weather Forecasts
  (ECMWF)
- Distribution used here: Open-Meteo spatial object storage
- Dataset information: <https://www.ecmwf.int/en/forecasts/datasets/open-data>
- ECMWF terms: <https://apps.ecmwf.int/datasets/licences/general/>
- Recorded data license: `CC-BY-4.0`
- License text: [`LICENSES/CC-BY-4.0.txt`](LICENSES/CC-BY-4.0.txt)

Public ECMWF-derived output carries the attribution:

> Weather data by Open-Meteo.com. This service is based on data and products
> of the European Centre for Medium-Range Weather Forecasts (ECMWF). Contains
> modified ECMWF data.

The implemented pipeline selects a regional grid, extracts byte ranges,
retains source run and valid-time metadata, may join a new short run with the
preceding long-run tail, interpolates in time and space for point requests,
performs requested unit conversions and derived-variable calculations, and
encodes map layers as lossless WebP. ECMWF is not responsible for these local
transformations or service output.

The deterministic IFS product does not expose precipitation probability.
`ecmwf_ifs025_ensemble` supplies that field from the 51-member IFS ensemble.
The pipeline omits the unavailable forecast-hour-zero spatial object, retains
its first real frame at forecast hour 3, and may use an older retained run only
when it represents the same valid time. It never manufactures a zero value.

## NOAA GFS

- Local products: `gfs013_surface`, `gfs025`, `gfs_pressure_profile`,
  `ncep_gefs025`, `ncep_gefs05`
- Underlying provider: NOAA National Centers for Environmental Prediction
  (NCEP)
- Model information: <https://www.ncei.noaa.gov/products/weather-climate-models/global-forecast>
- NOAA/NWS disclaimer: <https://www.weather.gov/disclaimer>
- Distribution used here: Open-Meteo spatial object storage

The pipeline performs regional byte-range extraction, run selection,
null-value fallback to the immediately preceding complete run where defined,
point interpolation, requested unit conversions and derived-variable
calculation, and lossless WebP map encoding. `ncep_gefs025` and
`ncep_gefs05` supply precipitation probability from the NOAA Global Ensemble
Forecast System (GEFS). The API prefers the 0.25° product through forecast hour
240 and uses the 0.5° product at the same valid time when the 0.25° value is
unavailable, including the longer tail through forecast hour 384. Their
unavailable forecast-hour-zero spatial objects are omitted; an older retained
run may fill only the same valid time, and a missing value is never replaced
with a fabricated zero.

## Copernicus Atmosphere Monitoring Service

- Local product: `cams_global`
- Provider: Copernicus Atmosphere Monitoring Service (CAMS)
- Service: <https://atmosphere.copernicus.eu/>
- Data licence: <https://atmosphere.copernicus.eu/data-licence>
- Distribution used here: Open-Meteo spatial object storage

The pipeline performs regional byte-range extraction, run selection,
point interpolation, unit conversion and derived air-quality calculations,
and lossless WebP map encoding. A release that displays CAMS output must retain
the attribution required by the then-current CAMS licence, including the
applicable information year.

## Copernicus DEM90

The API expects a deployment-provided Copernicus DEM90/static-grid package
under the configured `OM_DEM_ROOT`. It uses that package for elevation
correction and supported land-cell selection. The installer verifies that the
expected directory exists, but the repository does not independently prove the
acquisition record for a copied deployment package. Deployment records must
retain the exact source, version, checksum, acquisition date, and applicable
Copernicus terms before the DEM is enabled in a release.

Production sets `OM_DEM_ROOT` to
`/opt/1panel/apps/weather_om_api/static`. This immutable, checksum-audited
asset is installation data on the system disk. Generated GFS/CAMS/ECMWF
cycles, WebP releases, and official-comparison snapshots remain on `/data`.
Native cycle markers record the external `OM_DEM_ROOT` contract and do not
duplicate DEM90 payload bytes inside cycle directories.

The pinned GFS 0.13°, GFS 0.25°, GEFS 0.25°, ECMWF IFS 0.25°, and ECMWF IFS
ensemble 0.25° `HSURF.om` model elevation grids are likewise immutable
installation assets. The API installer downloads and checksum-verifies them under
`/opt/1panel/apps/weather_om_api/static/<model>/HSURF.om`, and the service
resolves them through `OM_MODEL_STATIC_ROOT=/opt/1panel/apps/weather_om_api`.
Downloader and mirror manifests retain their source URL, size, checksum, and
external-environment contract but do not copy their payload into `/data`.

Dataset information:
<https://dataspace.copernicus.eu/explore-data/data-collections/copernicus-contributing-missions/collections-description/COP-DEM>

## Official API validation captures

Validation tooling can save responses from the public Open-Meteo API so a
frozen comparison run does not repeatedly query the service. Those captures
are test evidence, not a new data source, and are excluded from the deployed
source archive unless deliberately committed after a separate privacy,
licensing, and size review. Validation reports must record request parameters,
model run/reference time, retrieval time, endpoint, and hashes of saved
responses.

## Public machine-readable notice

The deployed Rust service publishes
`/.well-known/weather-attribution.json`. ECMWF WebP catalogs and product
manifests repeat provider, distributor, license, modified-status, and
transformation fields so a client can display attribution without inferring it
from file paths.
