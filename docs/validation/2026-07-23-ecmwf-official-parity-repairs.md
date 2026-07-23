# ECMWF frozen-run parity repairs (2026-07-23)

## Evidence baseline

- Frozen model run: `ecmwf_ifs025 / 2026072300`
- Official capture index:
  `/data/validation/ecmwf/2026072300/official-cache-final/official_index.json`
- Official index file SHA-256:
  `53167359bbcd4c9dbc24d003d0e3f09c740edb768753fc32350f5b6bdd46bcad`
- Official index content SHA-256:
  `e97ed06dfe95f3e83f9b375820b7e3ddf5db070112e093470e393bdd81a263aa`
- Official source tree inspected:
  `acfe608b825da1a8b42a755297eb61121986e9da`
- Captured API behavior source match:
  `b743cbc9a7fab3f8f7dda85968fb770eee48b9ec`

The official responses were captured before the attested source-object
transition, so the comparison remains tied to one immutable API batch. All
diagnostic responses and logs are retained below
`/data/validation/ecmwf/2026072300/diagnostics`.

## Sparse solar transition after forecast hour 144

The first value mismatch after the growing-degree-day unit repair was point 0
(`0,70`) at hourly frame 150:

- official `apparent_temperature=30.9`
- former clone `apparent_temperature=31.2`

Temperature, relative humidity and wind were already identical. The difference
was caused by shortwave radiation during ECMWF's cadence transition from
3-hourly source frames through forecast hour 144 to 6-hourly source frames
afterward.

For the selected cell, the immutable local source contains:

```text
hour 132 135 138 141 144 147 150 153 156 159 162
raw   510  42   0   0   0   NaN 269 NaN 568 NaN 19
```

The captured official API uses the four-point clearness-index Hermite
interpolation and sequential C-frame deaveraging from
`InterpolationInplace.swift` at revision
`b743cbc9a7fab3f8f7dda85968fb770eee48b9ec`. Its regular 3-hour axis is:

```text
hour    132 135 138 141 144 147 150 153 156 159 162
regular 510  42   0   0   0   70  468 677 466 50  0
```

The clone had used the later constant-clearness interpolation introduced by
Open-Meteo commit `b9550ea1df0e36cb8980eb98504fde23e99a9a47`; that produced
`34/504/682` at hours 147/150/153 and did not match the frozen official API.

The repair generalizes the already exact hourly implementation to an explicit
input cadence and uses a 10,800-second step for ECMWF's regular first stage.
The GFS hourly call remains on 3,600 seconds. The isolated full point replay
then reproduced the official shortwave sequence exactly:

```text
hourly 144..156:
0, 0, 27, 182, 341, 479, 590, 665, 693, 670, 599, 477, 315
```

Regression:
`ecmwf_first_stage_matches_captured_official_sparse_solar_transition`.

## Precipitation-type JSON precision

After the sparse-solar repair, strict comparison advanced to
`hourly.precipitation_type[0]`. Both responses held the value one, but the
official dimensionless unit writer emitted the JSON number `1.0`, while the
clone emitted the JSON integer `1`. The repair assigns the official one-decimal
dimensionless output precision to `precipitation_type`; it does not alter the
stored or calculated category.

Regression:
`precipitation_type_uses_official_dimensionless_json_precision`.

## Diffuse-radiation exponent parity

The next strict mismatch was the sunrise frame
`hourly.sunshine_duration[338]`: official `403.71` seconds, clone `403.72`
seconds. The incoming shortwave value was exactly `35.0 W/m²`; the difference
was isolated to the diffuse-radiation separation model.

Open-Meteo evaluates its squared and cubed clearness-index and incidence-angle
terms with Float `powf`. The clone had replaced those calls with integer-power
multiplication. Although both paths produced the same public one-decimal
direct-radiation value, the few internal Float ULPs changed the two-decimal
sunshine result. Restoring `powf` produces diffuse radiation bits `0x41e86c5f`
and sunshine-duration bits `0x43c9db7d`, which serialize to the captured
official `403.71`.

Regression:
`diffuse_and_sunshine_match_captured_ecmwf_sunrise_frame`.

## Rolling wind-gust retention

The next mismatch was `hourly.wind_gusts_10m[88]`: official `6.7 m/s`,
clone `6.8 m/s`; the clone then returned null from frame 93 onward.
ECMWF's open-data spatial objects omit this variable in an intermediate
forecast-hour band, while 6-hour long-range frames reappear after hour 144.
Open-Meteo's rolling database therefore retains gust frames from an older long
run when newer objects do not contain the variable.

The former coverage planner only searched the three source runs selected for
the ordinary stitched window. None of those runs was old enough for the
long-range gust field, so the first missing regular frame also removed the D
lookahead used to interpolate frame 88.

The repair adds an explicitly bounded, product-configured older-cycle search
for variables missing from a selected spatial object. ECMWF searches up to 72
hours behind the oldest ordinary coverage run; GFS and CAMS retain the default
zero extra lookback. HTTP/object inventory remains the authority: nonexistent
short-run or cadence combinations are skipped, and only a source object that
actually contains the missing variable is bundled. Entries preserve their
real older source-run identity so the existing first-stage interpolation runs
within that source run before newest-run overlay, matching the official
rolling database rather than interpolating across runs.

Regressions:
`test_missing_variable_fallback_candidates_reach_retained_long_run` and
`ecmwf_retained_gust_frame_supplies_official_second_stage_lookahead`.

## Verification contract

Every repair requires the complete Rust unit and API suite, deployment from
the GitHub default-branch revision, replay of the failed point, and resumption
of the strict stop-at-first-difference comparison. No downloader or scheduled
task is enabled during the frozen comparison.
