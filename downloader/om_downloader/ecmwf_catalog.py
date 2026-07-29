"""Exact public ECMWF IFS025 inventory exposed by the local API."""

SURFACE_HOURLY_VARIABLES = (
    "temperature_2m", "temperature_2m_min", "temperature_2m_max",
    "relative_humidity_2m", "dew_point_2m", "apparent_temperature",
    "wet_bulb_temperature_2m", "pressure_msl", "surface_pressure",
    "cloud_cover", "cloud_cover_low", "cloud_cover_mid", "cloud_cover_high",
    "precipitation", "precipitation_probability", "rain", "snowfall",
    "snowfall_water_equivalent",
    "precipitation_type", "runoff", "snow_depth", "snow_depth_water_equivalent",
    "cape", "shortwave_radiation", "shortwave_radiation_instant",
    "direct_radiation", "direct_radiation_instant", "diffuse_radiation",
    "diffuse_radiation_instant", "direct_normal_irradiance",
    "direct_normal_irradiance_instant", "global_tilted_irradiance",
    "global_tilted_irradiance_instant", "sunshine_duration",
    "et0_fao_evapotranspiration", "vapor_pressure_deficit", "wind_speed_10m",
    "wind_direction_10m", "wind_gusts_10m", "wind_speed_100m",
    "wind_direction_100m", "surface_temperature", "skin_temperature",
    "soil_temperature_0cm", "soil_temperature_0_to_7cm",
    "soil_temperature_7_to_28cm", "soil_temperature_28_to_100cm",
    "soil_temperature_100_to_255cm", "soil_temperature_0_to_100cm",
    "soil_moisture_0_to_7cm", "soil_moisture_7_to_28cm",
    "soil_moisture_28_to_100cm", "soil_moisture_100_to_255cm",
    "soil_moisture_0_to_100cm", "total_column_integrated_water_vapour",
    "growing_degree_days_base_0_limit_50", "weather_code", "is_day",
)

PRESSURE_LEVELS_HPA = (1000, 925, 850, 700, 600, 500, 400, 300, 250, 200, 150, 100, 50, 10)
PRESSURE_VARIABLE_TYPES = (
    "temperature", "relative_humidity", "geopotential_height",
    "wind_u_component", "wind_v_component", "wind_speed", "wind_direction",
    "vertical_velocity", "dew_point", "cloud_cover",
)
PRESSURE_HOURLY_VARIABLES = tuple(
    f"{variable}_{level}hPa"
    for level in PRESSURE_LEVELS_HPA
    for variable in PRESSURE_VARIABLE_TYPES
)
HOURLY_VARIABLES = (*SURFACE_HOURLY_VARIABLES, *PRESSURE_HOURLY_VARIABLES)

DAILY_VARIABLES = (
    "apparent_temperature_max", "apparent_temperature_mean",
    "apparent_temperature_min", "cape_max", "cape_mean", "cape_min",
    "cloud_cover_max", "cloud_cover_mean", "cloud_cover_min",
    "dew_point_2m_max", "dew_point_2m_mean", "dew_point_2m_min",
    "et0_fao_evapotranspiration_sum", "growing_degree_days_base_0_limit_50",
    "precipitation_hours", "precipitation_probability_max",
    "precipitation_probability_mean", "precipitation_probability_min",
    "precipitation_sum", "pressure_msl_max",
    "pressure_msl_mean", "pressure_msl_min", "rain_sum",
    "relative_humidity_2m_max", "relative_humidity_2m_mean",
    "relative_humidity_2m_min", "shortwave_radiation_sum", "snowfall_sum",
    "snowfall_water_equivalent_sum", "snow_depth_max", "snow_depth_mean",
    "snow_depth_min", "soil_moisture_0_to_7cm_mean",
    "soil_moisture_7_to_28cm_mean", "soil_moisture_28_to_100cm_mean",
    "soil_moisture_100_to_255cm_mean", "soil_moisture_0_to_100cm_mean",
    "soil_temperature_0_to_7cm_mean", "soil_temperature_7_to_28cm_mean",
    "soil_temperature_28_to_100cm_mean", "soil_temperature_100_to_255cm_mean",
    "soil_temperature_0_to_100cm_mean", "sunrise", "sunset",
    "daylight_duration", "sunshine_duration", "surface_pressure_max",
    "surface_pressure_mean", "surface_pressure_min", "temperature_2m_max",
    "temperature_2m_mean", "temperature_2m_min", "vapor_pressure_deficit_max",
    "weather_code", "wind_direction_10m_dominant", "wind_gusts_10m_max",
    "wind_gusts_10m_mean", "wind_gusts_10m_min", "wind_speed_10m_max",
    "wind_speed_10m_mean", "wind_speed_10m_min",
    "wet_bulb_temperature_2m_max", "wet_bulb_temperature_2m_mean",
    "wet_bulb_temperature_2m_min", "wind_direction_100m_dominant",
    "wind_speed_100m_max", "wind_speed_100m_mean", "wind_speed_100m_min",
)

assert len(SURFACE_HOURLY_VARIABLES) == 58
assert len(PRESSURE_HOURLY_VARIABLES) == 140
assert len(HOURLY_VARIABLES) == 198
assert len(DAILY_VARIABLES) == 68
