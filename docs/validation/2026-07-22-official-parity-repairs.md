# Open-Meteo frozen-run parity repairs (2026-07-22)

## Evidence baseline

- Frozen official snapshot: `20260721_gfs2018_cams2012_full_500`
- Snapshot index SHA-256: `f86bae73910ab2f9208c221986dbae452f3451ae0210a31df00b84c9ae1748f3`
- GFS run: `2026072018`
- CAMS run: `2026072012`
- Open-Meteo source revision audited for the frozen run:
  `b743cbc9a7fab3f8f7dda85968fb770eee48b9ec`
- Official weather-code implementation:
  <https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/App/Helper/WeatherCode.swift>
- Official GFS reader call site:
  <https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/App/Gfs/GfsController.swift>
- Official generic reader:
  <https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/App/Helper/GenericReader.swift>
- Official Hermite interpolation:
  <https://github.com/open-meteo/open-meteo/blob/b743cbc9a7fab3f8f7dda85968fb770eee48b9ec/Sources/App/Helper/Interpolation.swift>

## CAMS greenhouse CO tail

The first CAMS mismatch was point 5 (`16.22519,101.953125`) at
`2026-07-25T01:00Z`: official `218.0`, former clone `218.5`.

Official frozen source values at the selected cells were:

- CAMS Global sparse CO: `333, 340, 202, 168`
- greenhouse-gas CO tail: `145, 136`

The official generic reader supplies unavailable Hermite C and D samples as
NaN; the Hermite implementation substitutes B for both while retaining A.
The resulting hourly greenhouse tail is therefore `136, 135, 136` rather than
the former clone's constant `136, 136, 136`. Applying the official backwards
three-frame mixer gives `187, 218, 218.5, 202`, exactly matching the snapshot.

The former clone clamped both hours after the final stored greenhouse frame to
B. The repair evaluates the normal Hermite tail with `C=D=B`, applies the CO
scale-factor rounding, and returns NaN from the next native cadence onward.
It does not encode any point, time, or expected API value.

Regression:
`cams_carbon_monoxide_hermite_extrapolates_greenhouse_tail_before_mixing`.

## GFS weather-code selected latitude

The next mismatch was point 88 (`13.765053,123.16406`, land selection) at
`2026-07-23T14:00Z`: official `81`, former clone `95`.

The request resolves to GFS013 model latitude `13.647903`. The frozen public
inputs and internal source inputs reproduce the official thunderstorm formula:

- selected model latitude `13.647903` -> approximately `59.993%` -> code `81`
- original request latitude `13.765053` -> approximately `60.058%` -> code `95`

Open-Meteo uses the strict test `probability > 60`. The clone's optimized point
time-slab path sampled all meteorology from the selected land/model cells but
reconstructed the weather-code latitude from the original request coordinate.
The non-optimized path already used the selected GFS013 latitude.

The repair makes both weather-code grid paths use request-scoped GFS013 model
sampling for one-point calls. Multi-cell regional rendering retains its
explicit grid-coordinate behavior. No threshold or weather-code value was
changed.

Regression:
`point_weather_code_uses_land_selected_surface_model_latitude`.

## Verification contract

Both repairs require complete Rust tests, production build tests, direct replay
of the failed point, and a new strict 500-point comparison whose receipts all
come from the same deployed revision and unchanged frozen data identities.
