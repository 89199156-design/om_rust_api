# Third-party software notices

This file records the third-party software directly used or followed by this
repository. Exact Rust dependency versions are locked in `Cargo.lock`; source
distributions contain the manifests and lockfile needed to reproduce that
inventory.

## Open-Meteo

- Project: <https://github.com/open-meteo/open-meteo>
- Recorded behavior baseline: `4efb9c49fb4a3718ed385fb22580d2e0fc56bdb2`
- Declared license: `AGPL-3.0-or-later`
- License text: [`LICENSE`](LICENSE)

The local API is a separately maintained implementation. It reproduces
selected Open-Meteo behavior and OM-format conventions and contains local
changes for regional storage, routing, interpolation, map rendering, and
deployment.

## Open-Meteo `om-file-format`

- Project: <https://github.com/open-meteo/om-file-format>
- Pinned source revision: `71f422b2706d8a81f1cecf52ae3073990de1ddbe`
- Rust FFI package declared license at that revision: `GPL-2.0-only`
- License text: [`LICENSES/GPL-2.0-only.txt`](LICENSES/GPL-2.0-only.txt)

`scripts/build_omfileformat_decoder.sh` compiles the upstream C sources into a
dynamically loaded server library. The build writes `BUILDINFO.json`, an
artifact SHA-256 file, and a copy of the upstream license next to the library.
The library is not part of the tracked-source archive.

The `GPL-2.0-only` native library and this project's `AGPL-3.0-or-later`
service have an unresolved license-compatibility question. Documentation and
dynamic loading do not themselves resolve it. Project policy therefore limits
the library to server-internal use and forbids client or third-party native
distribution pending upstream clarification or qualified legal review.

## TurboPFor decoder subset

`downloader/scripts/build_turbopfor_decoder.sh` builds a small decoder from
TurboPFor files included in the same pinned `om-file-format` source revision.
The selected upstream source headers state GPL version 2 or later. The build is
also treated as server-internal-only; its upstream source and notices must
accompany any permitted distribution.

## Direct Rust dependencies

The two Rust crates directly depend on packages including `anyhow`, `axum`,
`chrono`, `chrono-tz`, `clap`, `fs2`, `image`, `libloading`, `rayon`, `serde`,
`serde_json`, `ryu`, `thiserror`, `tokio`, `tower`, `tower-http`, `tracing`,
`tracing-subscriber`, and `ureq`. Their exact versions and transitive graph are
recorded in `Cargo.lock`. Package license metadata and notices remain those of
their respective authors.

Before distributing a binary, container, SDK, or client, generate and review a
complete dependency SBOM and third-party notice bundle from that exact lockfile.
This repository does not claim that the short direct-dependency inventory above
replaces such a release-specific review.

## Data is covered separately

Forecast and terrain datasets are not software dependencies and are documented
in [`DATA_SOURCES.md`](DATA_SOURCES.md). ECMWF data's CC BY 4.0 text is retained
at [`LICENSES/CC-BY-4.0.txt`](LICENSES/CC-BY-4.0.txt).
