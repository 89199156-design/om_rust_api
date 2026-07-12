# Singapore Reference Output Contract

This document records the Singapore deployment output contract that the new Shanghai OM processing path should remain compatible with. It is a reference only: OM variables, pressure levels, and forecast hours must still come from actual Open-Meteo metadata/index.

## GFS Surface WebP

Reference file: `D:\Projects\weather_server_xjp_work\gfs_core.py`

- Base output: `data/gfs_data`
- Main manifest: `data/gfs_data/gfs_data.json`
- Computed manifest: `data/gfs_data/gfs_data_computed.json`
- Grid: `70.0..140.0E`, `0.0..58.0N`, `0.25 deg`, point-center grid, north-to-south row order.
- Published render products:
  - `t2m`
  - `tcc`
  - `hcc`
  - `mcc`
  - `lcc`
  - `wind`
  - `gust`
  - `prate`
  - `snod`
  - `prmsl`
  - `r2`
  - `vis`
- Render source channels:
  - `t2m`, `tcc`, `hcc`, `mcc`, `lcc`, `u10`, `v10`, `gust`, `prate`, `snod`, `prmsl`, `r2`, `vis`
- Scalar WebP encoding:
  - RG stores 16-bit scaled value.
  - B is zero.
  - A is 0 for invalid, 255 for valid.
- Wind WebP encoding:
  - U and V are packed as two 12-bit values into RGB.
  - A is 0 for invalid, 255 for valid.

## CAMS WebP

Reference file: `D:\Projects\weather_server_xjp_work\cams_core.py`

- Base output: `data/cams_data`
- Main manifest: `data/cams_data/cams_data.json`
- Grid: `70.0..140.0E`, `0.0..58.0N`, `0.4 deg`, point-center grid, north-to-south row order.
- Published channels:
  - `aod550`
  - `pm2p5`
  - `pm10`
- Reference request variables:
  - `total_aerosol_optical_depth_550nm`
  - `particulate_matter_2.5um`
  - `particulate_matter_10um`
- New OM path currently maps these from Open-Meteo variables:
  - `aerosol_optical_depth`
  - `pm2_5`
  - `pm10`

## Point Package

Reference file: `D:\Projects\weather_server_xjp_work\gfs_point_package_core.py`

- Base output: `data/point_package`
- Metadata: `point_weather_meta.json`
- Binary payload: `point_weather.bin`
- Current package version in reference: `24`
- Future hours in reference: `120`
- Fields:
  - `temperature_c`
  - `humidity_pct`
  - `u10_ms`
  - `v10_ms`
  - `gust_ms`
  - `visibility_m`
  - `surface_pressure_pa`
  - `mean_sea_level_pressure_pa`
  - `precip_1h_mm`
  - `precip_rate_mm_h`
  - `convective_precip_1h_mm`
  - `snowfall_water_equivalent_mm`
  - `freezing_rain_signal`
  - `pbl_height_m`
  - `pwat_mm`
  - `refc_dbz`
  - `hlcy_m2s2`
  - `shear_0_6km_ms`
  - `shear_0_3km_ms`
  - `vertical_velocity_pa_s`
  - `cloud_ice_mixing_ratio`
  - `graupel_mixing_ratio`
  - `rain_water_mixing_ratio`
  - `cloud_total_pct`
  - `cloud_low_pct`
  - `cloud_mid_pct`
  - `cloud_high_pct`
  - `precip_phase_code`
  - `precip_phase_confidence`
  - `precip_phase_reason_code`
  - `precip_phase_quality_code`
  - `precip_rain_1h_mm`
  - `precip_snow_water_equivalent_1h_mm`
  - `precip_showers_1h_mm`
  - `precip_freezing_rain_1h_mm`
  - `precip_unknown_remainder_1h_mm`
  - `thunderstorm_risk`
  - `thunderstorm_level`
  - `thunderstorm_confidence`
  - `thunderstorm_reason_code`
  - `cape_jkg`
  - `cin_jkg`
  - `lifted_index_c`
  - `weather_quality_code`
  - `weather_label_code`
  - `weather_code`

## Pressure Profile

Reference files: `D:\Projects\weather_server_xjp_work\gfs_profile_config.py`, `D:\Projects\weather_server_sh\pressure_profile_store.py`

- Base package expected by Shanghai store: `data/pressure_profile_package`
- Metadata: `pressure_profile_meta.json`
- Binary payload: `pressure_profile.bin`
- Reference pressure levels:
  - `1000`, `975`, `950`, `925`, `900`, `850`, `800`, `750`, `700`, `650`, `600`, `550`, `500`, `450`, `400`, `350`, `300`, `250`, `200`, `150`, `100`, `50`
- Reference variables:
  - `TMP` -> `tmp_k`
  - `RH` -> `rh_pct`
  - `UGRD` -> `ugrd_ms`
  - `VGRD` -> `vgrd_ms`
  - `HGT` -> `hgt_m`
- New OM path must not assume all reference pressure levels exist; missing levels must be recorded and must not overwrite the previous usable package.

## Manifest Shape

GFS and CAMS WebP manifests keep these core fields:

- `update_timestamp`
- `file_count`
- `grid`
- `files`
- `source_grid`

Grid metadata keeps:

- `grid_type`
- `bounds_semantics`
- `sample_bounds`
- `grid_width`
- `grid_height`
- `center_dx`
- `center_dy`
- `display_bounds`
- `display_bounds_semantics`
- `row_order`

