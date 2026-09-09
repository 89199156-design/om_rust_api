# On-demand internal grid API

This API is the licensing and data-format boundary between the AGPL service
that decodes Open-Meteo OM files and independently developed internal
applications. It never writes generated grids to disk. Each authenticated GET
decodes one model, one variable and one UTC hour from the currently published
immutable snapshot, returns the response, and releases its bounded working
memory.

This is a transitional exploration interface while downstream products such as
sunset-cloud and astronomy layers are still determining their final variable
sets. Once those contracts are fixed, remove this general-purpose endpoint and
have the open OM-processing jobs generate the final product artifacts directly;
closed applications should then consume only those finished artifacts.

The service implementation and its OM decoder remain covered by this
repository's AGPL terms. A caller receives documented weather data over HTTP
and does not need to link, copy, import, or execute any OM/Open-Meteo code.
Callers must still preserve the provider attribution and comply with the data
terms described by `/.well-known/weather-attribution.json`.

## Endpoints

Both endpoints require `Authorization: Bearer <token>`. The deployment keeps
the token in `OM_INTERNAL_GRID_TOKEN_FILE`; the token is generated once with
mode `0640`, is never committed, and is reread on every request so rotation
does not require an API restart.

- `GET /v1/internal/grid/catalog?model=ec9`
- `GET /v1/internal/grid?model=ec9&variable=temperature_2m&valid_time=2026-09-10T03:00:00Z`

Models are `ec9`, `ec25`, `gfs`, and `cams`. `expected_run=YYYYMMDDHH` is an
optional optimistic guard: the request returns HTTP 409 instead of silently
reading a different batch. The optional `west`, `east`, `south`, and `north`
parameters must be supplied together. They select exact rows and columns from
the model output axes; the response records both the requested bounds and the
actual included coordinates.

The catalog lists source variables and their exact availability windows. The
data endpoint also accepts hourly derived variables already implemented by the
point/WebP calculation layer. Unsupported variable/time combinations fail
explicitly and never return a partially filled success response.

## `weather-grid-v1` wire format

The response media type is `application/vnd.weather-grid-v1` and its layout is:

1. 8 byte magic: hexadecimal `57 47 52 49 44 31 00 00` (`WGRID1\0\0`).
2. Unsigned little-endian 32-bit JSON header length.
3. UTF-8 JSON metadata header.
4. Binary payload sections declared by offsets relative to the payload start:
   latitude `<f8`, longitude `<f8`, then row-major values `<f4`.

Latitude coordinates are north-to-south and longitude coordinates are
west-to-east. A value is located by `row * width + column`. Every response
contains the complete explicit coordinate axes, full-grid shape, integer
window offsets, current model run, coverage identity, units, payload length,
and SHA-256. IEEE-754 quiet NaN is the sole missing-value representation.

Consumers must align or resample by the explicit coordinate arrays. Geographic
bounds are descriptive and must not be used as an equality key. This is what
allows, for example, a 3-58 degree astronomy product to extract the correct
weather cells from a larger weather grid without requiring identical layer
bounds.

EC9 is stored internally on an O1280 reduced Gaussian grid. The API exports it
onto the same explicit regular nominal-9-km axis used by the established map
pipeline, so consumers do not need an OM parser or reduced-Gaussian topology
implementation. The metadata identifies this nearest-cell sampling step.

The response headers repeat `X-Weather-Model-Run`,
`X-Weather-Coverage-Id`, `X-Weather-Grid-Points`, and
`X-Weather-Grid-Payload-Sha256`. Responses use `private, no-store`; the service
does not create a persistent derived-data cache.

## Resource isolation

`OM_INTERNAL_GRID_MAX_CONCURRENT` defaults to one on the 4-core production
host. A second request waits for at most
`OM_INTERNAL_GRID_QUEUE_TIMEOUT_SECONDS` (30 seconds by default) and then gets
HTTP 429. EC9 is decoded in latitude strips so even dependency-heavy derived
variables do not construct an unbounded all-region intermediate.
