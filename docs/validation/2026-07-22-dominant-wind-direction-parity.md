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

A later full run exposed a second independent rounding boundary at point 486,
`28.117183,124.051053`, daily frame `2026-08-04`: official `355`, while the
first repair returned `356`. All 24 hourly wind-speed and wind-direction JSON
values for that day were already identical, isolating the defect to the hidden
single-precision daily operation graph.

## Official source path

The pinned generic official implementation declares `dominantDirection` with
derived 10 m wind speed and direction in
[`VariableDaily.swift`](https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/App/Controllers/VariableDaily.swift).

[`GenericDailyCalculator.swift`](https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/App/Helper/GenericDailyCalculator.swift)
then reconstructs hourly U/V components from those derived series, sums the
components by day, and calls `Meteorology.windirectionFast`. The frozen API
responses show that reproducing this generic source graph literally is not
bit-stable for the deployed GFS path at every integer-output boundary.

The single-precision component formulas and degree conversion are defined in
[`Meteorology.swift`](https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/App/Helper/Meteorology.swift)
and
[`NumberExtensions.swift`](https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/App/Helper/NumberExtensions.swift).
The final direction is the FMA polynomial in
[`CHelper/src/shim.c`](https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/CHelper/src/shim.c),
not the standard-library `atan2` result.

## Boundary evidence

The former clone directly summed the stored/interpolated hourly U/V components
but applied standard `atan2`; point 446 therefore returned `209` instead of
`208`. The first repair changed both operations at once: it introduced
speed/direction reconstruction and the official fast-angle function. That fixed
point 446 but point 486 proved that the reconstruction step was incorrect for
the deployed API output, returning `356` instead of `355`.

An isolated production-binary matrix against the same frozen data established
the required combination without changing production or data: direct U/V plus
standard `atan2` reproduced point 486 only; reconstructed U/V plus the fast
angle reproduced point 446 only; direct U/V plus the official fast angle
reproduced both official values (`208` and `355`).

## Repair

The shared Rust implementation now:

1. reads the hourly `wind_u_component_10m` and
   `wind_v_component_10m` series after the normal request-time interpolation;
2. sums those components for the local day; and
3. applies the exact official C FMA polynomial, quadrant handling, and zero
   boundaries.

There is no coordinate, date, expected output, comparison exception, or
test-only production branch. The algorithm is used by normal production API
requests.

## Verification

The pinned official C function was compiled directly for the Linux/x86_64
production target and compared bit-for-bit with the Rust port. Regression
`wind_direction_matches_pinned_official_fast_approximation` locks those
production-target bit patterns and the NaN behavior. The isolated release build
also returned official point-446 value `208` and point-486 value `355` before
production deployment.

Required release gates are the complete Rust API and WebP suites, direct replay
of frozen point 446, and new strict 500-point comparisons from point 1 for both
servers without changing the frozen GFS/CAMS data identities.
