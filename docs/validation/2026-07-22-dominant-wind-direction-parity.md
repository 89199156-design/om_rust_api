# Open-Meteo dominant wind-direction parity repair (2026-07-22)

## Frozen evidence

- Official snapshot: `20260721_gfs2018_cams2012_full_500`
- Snapshot logical SHA-256:
  `f86bae73910ab2f9208c221986dbae452f3451ae0210a31df00b84c9ae1748f3`
- GFS run: `2026072018`
- Audited Open-Meteo revision:
  `b743cbc9a7fab3f8f7dda85968fb770eee48b9ec`
- First mismatch: point 446, `7.979259,133.983049`, daily frame
  `2026-07-23`, `wind_direction_10m_dominant`; official `208`,
  former clone `209`.

The first 445 Singapore points completed with exact parsed JSON type and value
parity before this mismatch. Shanghai used the same daily and wind-direction
implementation, so both production services require the same repair and both
full comparisons must restart at point 1.

## Official source path

The pinned official implementation selects `dominantDirection` with derived
10 m wind speed and direction in
[`VariableDaily.swift`](https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/App/Controllers/VariableDaily.swift).

[`GenericDailyCalculator.swift`](https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/App/Helper/GenericDailyCalculator.swift)
then reconstructs hourly U/V components from those derived series, sums the
components by day, and calls `Meteorology.windirectionFast`.

The single-precision component formulas and degree conversion are defined in
[`Meteorology.swift`](https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/App/Helper/Meteorology.swift)
and
[`NumberExtensions.swift`](https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/App/Helper/NumberExtensions.swift).
The final direction is the FMA polynomial in
[`CHelper/src/shim.c`](https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/CHelper/src/shim.c),
not the standard-library `atan2` result.

## Former clone deviation

The clone directly summed the stored hourly U/V source components and applied
standard `atan2`. Although mathematically close, that is not the official
operation graph. The difference is observable after single-precision
operations and integer API formatting, as the frozen point-446 result proves.

## Repair

The shared Rust implementation now:

1. reads the derived hourly `wind_speed_10m` and
   `wind_direction_10m` series;
2. reconstructs U/V with the official single-precision formulas and operation
   order;
3. sums reconstructed components for the local day; and
4. ports the exact official C FMA polynomial, quadrant handling, and zero
   boundaries.

There is no coordinate, date, expected output, comparison exception, or
test-only production branch. The algorithm is used by normal production API
requests.

## Verification

The pinned official C function was compiled directly for the Linux/x86_64
production target and compared bit-for-bit with the Rust port. Regression
`wind_direction_matches_pinned_official_fast_approximation` locks those
production-target bit patterns and the NaN behavior.

Required release gates are the complete Rust API and WebP suites, direct replay
of frozen point 446, and new strict 500-point comparisons from point 1 for both
servers without changing the frozen GFS/CAMS data identities.

