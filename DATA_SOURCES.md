# Runtime data sources, transformations, and attribution

`om_rust_api` does not acquire forecast data or contain provider credentials. It consumes immutable releases supplied by the deployment environment:

- Singapore receives OM from the separate `om_data_raw` Swift raw-model pipeline.
- Shanghai receives OM from the separate `om_data_om` official Open-Meteo bucket downloader.
- Shanghai ECMWF IFS 9 km receives cropped `weather-region-pack-v1` batches from the separate private `raw_data` downloader. The public materializer converts the complete retained five-run batch to an immutable `openmeteo-native-v1` coverage. API and WebP consume only that native coverage; successful downstream validation releases the private transport batch for automatic cleanup.

The API and WebP code must treat both as read-only inputs and expose their exact coverage/run identity. Source manifests and deployment records remain authoritative for acquisition URLs, reference times, checksums and transformations before OM publication.

## NOAA GFS and GEFS

Runtime products include GFS surface/pressure fields and GEFS precipitation probability. NOAA/NCEP is the underlying provider. The API interpolates point values, joins configured valid-time windows, performs requested unit conversions and derives supported hourly/daily variables. It never replaces a missing probability with a fabricated zero.

Provider information: <https://www.ncei.noaa.gov/products/weather-climate-models/global-forecast>

## ECMWF IFS

Runtime products include deterministic IFS 0.25°, its paired ensemble precipitation probability fields, and spatially cropped deterministic IFS 9 km native OM coverages. ECMWF attribution and applicable open-data terms must accompany derived output. The local service performs range extraction and spatial subsetting (in the private downloader), RegionPack-to-OM materialization, nearest-grid sampling, retained-run/NaN fallback, temporal interpolation, derived-variable calculations and WebP encoding; ECMWF is not responsible for those modifications.

Dataset information: <https://www.ecmwf.int/en/forecasts/datasets/open-data>

Recorded license text: [`LICENSES/CC-BY-4.0.txt`](LICENSES/CC-BY-4.0.txt)

## Copernicus Atmosphere Monitoring Service

CAMS runtime products provide global air-quality and greenhouse-gas fields. Releases must retain the attribution required by the applicable CAMS licence and information year.

Service and licence: <https://atmosphere.copernicus.eu/>

## Terrain and fixed model assets

The deployment supplies Copernicus DEM90 under `OM_DEM_ROOT` and checksum-pinned model elevation grids under `OM_MODEL_STATIC_ROOT`. These assets are not stored in the repository. Deployment records must retain exact source, version, checksum, acquisition date and applicable terms.

Dataset information: <https://dataspace.copernicus.eu/explore-data/data-collections/copernicus-contributing-missions/collections-description/COP-DEM>

## Open-Meteo behavior and distribution

The service is a separately maintained implementation compatible with selected Open-Meteo API and OM-format behavior. Shanghai OM inputs are distributed by Open-Meteo; Singapore OM inputs are locally produced from provider raw data. Public output must present the applicable Open-Meteo and underlying-provider attribution for the actual source path.

Open-Meteo terms: <https://open-meteo.com/en/terms>

## Validation captures

Official API comparison responses are saved outside this repository so the free endpoint is not queried repeatedly. Each capture must record endpoint, request parameters, coordinates, model/reference window, retrieval time and file hashes. Captures are test evidence, not production input, and require their own provenance and retention review.

The deployed API publishes `/.well-known/weather-attribution.json` and `/v1/source`; model and WebP manifests repeat the provider, modified-status, data identity and source revision needed for an auditable release.
