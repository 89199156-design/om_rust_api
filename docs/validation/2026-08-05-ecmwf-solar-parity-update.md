# ECMWF solar interpolation parity update (2026-08-05)

## Frozen evidence

- Official API capture: `official_200_capture_20260804_aligned_latest`
- Frozen model run: `ecmwf_ifs025 / 2026080400`
- Official capture SHA-256:
  `84748bfce7a41f9b29d8e3d96b136b9dc0e4319a7518c82a90401d679e055cad`
- First sequential comparison point: latitude `20`, longitude `134`

The Google and ECMWF origin GRIB ranges for the failing shortwave frames were
decoded independently and matched. The mismatch therefore did not require a
new data download or a producer rerun.

## Source parity repair

Open-Meteo changed sparse solar interpolation on 2026-07-24 in commit
`fc670930b55c963b10e9578c8628a824da43a3ab`. The production Rust API now ports
that implementation:

- backwards-averaged Haurwitz clear-sky radiation;
- three-point Gauss-Legendre integration over daylight;
- centered quadratic clearness-index interpolation; and
- source-interval mean preservation across the ECMWF 3-hour to 6-hour cadence
  transition.

Regression coverage uses the retained `2026080400` raw frames and the frozen
official hourly shortwave sequence at the first comparison point. The frozen
official snapshot remains immutable and is reused by the 200-point sequential
validator.
