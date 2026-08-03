# Project notice

`om_rust_api` is a separately maintained weather-data service whose
API behavior and OM-format handling are based in part on published Open-Meteo
software and data conventions. Open-Meteo has not endorsed this service.

The repository's Rust and Python source code is offered under
`AGPL-3.0-or-later`; see [`LICENSE`](LICENSE). A deployed API exposes
`/v1/source`, which identifies the exact compiled Git revision and the
corresponding tracked-source archive and SHA-256 file. The source archive does
not contain runtime weather data, credentials, deployment secrets, or build
artifacts.

Weather data, terrain data, and third-party software are not relicensed by the
project's AGPL notice. Their providers, terms, modifications, and attribution
are recorded in [`DATA_SOURCES.md`](DATA_SOURCES.md) and
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md). The deployed service also
publishes the machine-readable summary
`/.well-known/weather-attribution.json`.

The native `om-file-format` shared library is built from an exact upstream
revision licensed `GPL-2.0-only`. Its compatibility with the service's
`AGPL-3.0-or-later` code has not been established. It is restricted by project
policy to server-internal use and must not be placed in a client, container
distributed to a third party, SDK, or other distribution package unless the
upstream licensing position or qualified legal review resolves that issue.

These notices describe the implementation and recorded sources. They are not
a legal opinion or a guarantee that a particular deployment or product meets
all applicable obligations.
