# ECMWF IFS025 data provenance and attribution

This downloader reads the Open-Meteo spatial object store model
`ecmwf_ifs025` from:

- `https://openmeteo.s3.amazonaws.com/data_spatial/ecmwf_ifs025/`
- static model elevation:
  `https://openmeteo.s3.amazonaws.com/data/ecmwf_ifs025/static/HSURF.om`

The underlying forecast data are ECMWF IFS open data. ECMWF states that its
open real-time products may be redistributed and used commercially under
CC BY 4.0, subject to attribution and the ECMWF Terms of Use:

- https://www.ecmwf.int/en/forecasts/datasets/open-data-0
- https://apps.ecmwf.int/datasets/licences/general/
- https://creativecommons.org/licenses/by/4.0/

Open-Meteo also publishes its API data under CC BY 4.0 and requires visible
attribution where the data are displayed:

- https://open-meteo.com/en/license
- https://open-meteo.com/en/terms

The commercial product must display both the Open-Meteo attribution and the
ECMWF attribution required by the current terms. A suitable starting point is:

> Weather data by Open-Meteo.com. This service is based on data and products
> of the European Centre for Medium-Range Weather Forecasts (ECMWF). ECMWF
> data are licensed under CC BY 4.0. Forecast data have been spatially cropped,
> compressed, interpolated and used to derive additional weather fields.

The application owner remains responsible for reviewing the current terms and
for placing the final attribution and disclaimer in the shipped product.

## Auditable transformations

The downloader does not add observations or third-party datasets. It:

1. freezes discovery to a completed model reference time when
   `--reference-time` is supplied;
2. selects the configured China/nearby-region grid chunks;
3. preserves the original OM array metadata and compressed bytes in a
   checksummed `.omranges` bundle;
4. stitches the newest 06/18Z short run over the tail of the preceding 00/12Z
   long run, choosing the newest source run for every native valid time.

The immutable ECMWF IFS025 `HSURF.om` observed for this integration has:

- byte size: `433648`
- SHA-256: `935d56ba000b438b61504fbc271bfaa8f70db2acb541d58d5b466a24d294a9fb`
- OM dimensions: `721 x 1440`
- OM chunks: `20 x 20`

Do not replace that file without verifying and recording the new size,
checksum and OM metadata.

## Intentionally unavailable free-model fields

The ECMWF IFS025 object inventory does not provide visibility, UV index,
showers or precipitation probability. They are deliberately not declared as
downloadable capabilities. `wind_gusts_10m` is a required field in the current
free deterministic feed. At forecast hour zero Open-Meteo carries forward the
previous 18Z run's forecast-hour-six value for exactly these six fields:
`wind_gusts_10m`, `temperature_2m_max`, `temperature_2m_min`,
`shortwave_radiation`, `precipitation`, and `runoff`.
