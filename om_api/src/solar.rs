use crate::solar_lookup::{DECLINATION_BITS, EQUATION_OF_TIME_MINUTES_BITS};
use chrono::{DateTime, Utc};
use std::sync::OnceLock;

pub const SOLAR_CONSTANT: f32 = 1367.7;
// Swift's Float.pi is one ULP below Rust's f32::consts::PI. Open-Meteo's
// solar helpers use the Swift value for every degree/radian conversion.
const PI: f32 = f32::from_bits(0x4049_0fda);
const SECONDS_PER_DAY: i64 = 86_400;
const SECONDS_PER_AVERAGE_YEAR: i64 = 31_557_600;
const LOOKUP_START: i64 = -631_152_000; // 1950-01-01T00:00:00Z
const LOOKUP_END: i64 = 2_524_608_000; // 2050-01-01T00:00:00Z
const LOOKUP_STEP: i64 = 20 * SECONDS_PER_DAY;

#[derive(Debug, Clone, Copy)]
struct SunPosition {
    declination_degrees: f32,
    equation_of_time_hours: f32,
}

#[derive(Debug, Clone, Copy)]
struct BackwardsGeometry {
    t0: f32,
    t1: f32,
    p0: f32,
    p1: f32,
    p10: f32,
    p1_limited: f32,
    p10_limited: f32,
}

fn radians(degrees: f32) -> f32 {
    degrees * PI / 180.0
}

struct SolarPositionLookup {
    declination: Vec<f32>,
    equation_of_time_minutes: Vec<f32>,
}

impl SolarPositionLookup {
    fn new() -> Self {
        debug_assert_eq!(DECLINATION_BITS.len(), EQUATION_OF_TIME_MINUTES_BITS.len());
        debug_assert_eq!(
            DECLINATION_BITS.len() as i64,
            (LOOKUP_END - LOOKUP_START + LOOKUP_STEP - 1) / LOOKUP_STEP
        );
        Self {
            declination: DECLINATION_BITS
                .iter()
                .copied()
                .map(f32::from_bits)
                .collect(),
            equation_of_time_minutes: EQUATION_OF_TIME_MINUTES_BITS
                .iter()
                .copied()
                .map(f32::from_bits)
                .collect(),
        }
    }

    fn position(&self, unix_time: i64) -> SunPosition {
        let cycle_seconds = LOOKUP_END - LOOKUP_START;
        let relative = (unix_time - LOOKUP_START).rem_euclid(cycle_seconds);
        let index = (relative / LOOKUP_STEP) as usize;
        let fraction = relative.rem_euclid(LOOKUP_STEP) as f32 / LOOKUP_STEP as f32;
        SunPosition {
            declination_degrees: interpolate_hermite_ring(&self.declination, index, fraction),
            equation_of_time_hours: interpolate_hermite_ring(
                &self.equation_of_time_minutes,
                index,
                fraction,
            ) / 60.0,
        }
    }
}

fn interpolate_hermite_ring(values: &[f32], index: usize, fraction: f32) -> f32 {
    let count = values.len();
    let a_value = values[(index + count - 1) % count];
    let b_value = values[index % count];
    let c_value = values[(index + 1) % count];
    let d_value = values[(index + 2) % count];
    let a = -a_value / 2.0 + (3.0 * b_value) / 2.0 - (3.0 * c_value) / 2.0 + d_value / 2.0;
    let b = a_value - (5.0 * b_value) / 2.0 + 2.0 * c_value - d_value / 2.0;
    let c = -a_value / 2.0 + c_value / 2.0;
    let d = b_value;
    a * fraction * fraction * fraction + b * fraction * fraction + c * fraction + d
}

fn sun_position(timestamp: DateTime<Utc>) -> SunPosition {
    static LOOKUP: OnceLock<SolarPositionLookup> = OnceLock::new();
    LOOKUP
        .get_or_init(SolarPositionLookup::new)
        .position(timestamp.timestamp())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SunTransit {
    PolarNight,
    PolarDay,
    Transit { rise_seconds: i64, set_seconds: i64 },
}

/// Exact Float-port of Open-Meteo's `Zensun.calculateSunTransit` at the pinned
/// source revision. Offsets are seconds from the supplied UTC-midnight axis.
pub fn sun_transit(utc_midnight: DateTime<Utc>, latitude: f32, longitude: f32) -> SunTransit {
    let local_midday =
        utc_midnight + chrono::Duration::seconds(((12.0 - longitude / 15.0) * 3600.0) as i64);
    let position = sun_position(local_midday);
    let declination = radians(position.declination_degrees);
    let alpha = radians(0.83333);
    let latitude = radians(latitude);
    let noon = 12.0 - longitude / 15.0;
    let arg =
        -(alpha.sin() + latitude.sin() * declination.sin()) / (latitude.cos() * declination.cos());
    if arg > 1.0 {
        return SunTransit::PolarNight;
    }
    if arg < -1.0 {
        return SunTransit::PolarDay;
    }
    let hours = arg.acos() / radians(15.0);
    SunTransit::Transit {
        rise_seconds: ((noon - hours - position.equation_of_time_hours) * 3600.0) as i64,
        set_seconds: ((noon + hours - position.equation_of_time_hours) * 3600.0) as i64,
    }
}

/// Exact Float-port of `Zensun.calculateDaylightDuration(localMidnight:)`.
pub fn daylight_duration(local_midnight: DateTime<Utc>, latitude: f32) -> f32 {
    let position = sun_position(local_midnight + chrono::Duration::hours(12));
    let declination = radians(position.declination_degrees);
    let alpha = radians(0.83333);
    let latitude = radians(latitude);
    let arg =
        -(alpha.sin() + latitude.sin() * declination.sin()) / (latitude.cos() * declination.cos());
    if arg > 1.0 {
        return 0.0;
    }
    if arg < -1.0 {
        return 24.0 * 3600.0;
    }
    arg.acos() / radians(15.0) * 2.0 * 3600.0
}

fn hour_with_fraction(timestamp: DateTime<Utc>) -> f32 {
    timestamp.timestamp().rem_euclid(SECONDS_PER_DAY) as f32 / 3600.0
}

fn backwards_geometry(
    timestamp: DateTime<Utc>,
    dt_seconds: i64,
    latitude: f32,
    longitude: f32,
    zenith_cutoff_degrees: f32,
) -> Option<BackwardsGeometry> {
    let position = sun_position(timestamp);
    let ut = hour_with_fraction(timestamp);
    let t1 = radians(90.0 - position.declination_degrees);
    let p1 = radians(-15.0 * (ut - 12.0 + position.equation_of_time_hours));
    let ut0 = ut - dt_seconds as f32 / 3600.0;
    let p10 = radians(-15.0 * (ut0 - 12.0 + position.equation_of_time_hours));
    let t0 = radians(90.0 - latitude);
    let mut p0 = radians(longitude);
    if p0 < p1 - PI {
        p0 += 2.0 * PI;
    }
    if p0 > p1 + PI {
        p0 -= 2.0 * PI;
    }

    let arg = -(t0.cos() * t1.cos()) / (t0.sin() * t1.sin());
    let carg = if !(-1.0..=1.0).contains(&arg) {
        PI
    } else {
        arg.acos() - radians(zenith_cutoff_degrees)
    };
    let sunrise = p0 + carg;
    let sunset = p0 - carg;
    if p10 < sunset || p1 > sunrise {
        return None;
    }
    Some(BackwardsGeometry {
        t0,
        t1,
        p0,
        p1,
        p10,
        p1_limited: sunrise.min(p10),
        p10_limited: sunset.max(p1),
    })
}

fn backwards_sun_elevation(geometry: BackwardsGeometry, daylight_denominator: bool) -> f32 {
    let left = geometry.t0.sin() * geometry.t1.sin() * (geometry.p1_limited - geometry.p0).sin()
        + geometry.p1_limited * geometry.t0.cos() * geometry.t1.cos();
    let right = geometry.t0.sin() * geometry.t1.sin() * (geometry.p10_limited - geometry.p0).sin()
        + geometry.p10_limited * geometry.t0.cos() * geometry.t1.cos();
    let delta = if daylight_denominator {
        let value = geometry.p1_limited - geometry.p10_limited;
        if value < 0.0 {
            value.min(-0.001)
        } else {
            value.max(0.001)
        }
    } else {
        geometry.p10 - geometry.p1
    };
    (left - right) / delta
}

fn instant_sun_elevation(geometry: BackwardsGeometry) -> f32 {
    geometry.t0.cos() * geometry.t1.cos()
        + geometry.t0.sin() * geometry.t1.sin() * (geometry.p1 - geometry.p0).cos()
}

pub fn backwards_to_instant_factor(
    timestamp: DateTime<Utc>,
    dt_seconds: i64,
    latitude: f32,
    longitude: f32,
) -> f32 {
    let Some(geometry) = backwards_geometry(timestamp, dt_seconds, latitude, longitude, 0.0) else {
        return 0.0;
    };
    let backwards = backwards_sun_elevation(geometry, false).abs();
    let instant = instant_sun_elevation(geometry);
    if backwards <= 0.0 || instant <= 0.0 {
        return 0.0;
    }
    instant / backwards
}

/// Approximate backward-averaged diffuse radiation from global horizontal
/// irradiance using Open-Meteo's Razo/Mueller/Witwer separation model.
pub fn backwards_diffuse_radiation(
    shortwave_radiation: f32,
    timestamp: DateTime<Utc>,
    dt_seconds: i64,
    latitude: f32,
    longitude: f32,
) -> f32 {
    if !shortwave_radiation.is_finite() {
        return f32::NAN;
    }
    let sin_alpha = backwards_geometry(timestamp, dt_seconds, latitude, longitude, 0.0)
        .map(|geometry| backwards_sun_elevation(geometry, true))
        .unwrap_or(0.0);
    if sin_alpha <= 5.0 / SOLAR_CONSTANT {
        // At night and at very low solar angles, all GHI is diffuse.
        return shortwave_radiation;
    }
    let extraterrestrial = (sin_alpha * SOLAR_CONSTANT).min(0.95 * SOLAR_CONSTANT);
    let clearness_index = shortwave_radiation / extraterrestrial;
    let angle_of_incidence = sin_alpha.acos();

    let kt2 = clearness_index.powf(2.0);
    let kt3 = clearness_index.powf(3.0);
    let aoi2 = angle_of_incidence.powf(2.0);
    let aoi3 = angle_of_incidence.powf(3.0);
    let diffuse_fraction = 1.58 * clearness_index + 0.991 * angle_of_incidence
        - 5.084 * kt2
        - 2.11 * clearness_index * angle_of_incidence
        - 1.16 * aoi2
        + 2.918 * kt3
        + 1.307 * kt2 * angle_of_incidence
        + 0.762 * clearness_index * aoi2
        + 0.432 * aoi3
        + 0.718;
    (diffuse_fraction * shortwave_radiation).min(shortwave_radiation)
}

pub fn backwards_direct_normal_irradiance(
    direct_radiation: f32,
    timestamp: DateTime<Utc>,
    dt_seconds: i64,
    latitude: f32,
    longitude: f32,
    convert_to_instant: bool,
) -> f32 {
    if !direct_radiation.is_finite() {
        return f32::NAN;
    }
    if direct_radiation <= 0.0 {
        return 0.0;
    }
    let Some(geometry) = backwards_geometry(timestamp, dt_seconds, latitude, longitude, 3.0) else {
        return 0.0;
    };
    let daylight = backwards_sun_elevation(geometry, true);
    // The frozen ECMWF API baseline caps the denominator at sin(3°). This
    // avoids unstable DNI spikes close to sunrise and sunset. A later upstream
    // source revision removed this cap, but it does not describe the captured
    // 2026-07-23 official responses used by this clone.
    let zenith_cutoff_sin = radians(3.0).sin();
    let dni = direct_radiation / daylight.max(zenith_cutoff_sin);
    if !convert_to_instant {
        return dni;
    }
    let Some(no_cutoff) = backwards_geometry(timestamp, dt_seconds, latitude, longitude, 0.0)
    else {
        return 0.0;
    };
    let backwards = backwards_sun_elevation(no_cutoff, false).abs();
    if backwards <= 0.0 {
        return 0.0;
    }
    dni * instant_sun_elevation(no_cutoff).max(0.0) / backwards
}

#[allow(clippy::too_many_arguments)]
pub fn backwards_global_tilted_irradiance(
    direct_radiation: f32,
    diffuse_radiation: f32,
    timestamp: DateTime<Utc>,
    dt_seconds: i64,
    latitude: f32,
    longitude: f32,
    tilt_degrees: f32,
    array_azimuth_degrees: f32,
    convert_to_instant: bool,
) -> f32 {
    if !direct_radiation.is_finite() || !diffuse_radiation.is_finite() {
        return f32::NAN;
    }
    if direct_radiation + diffuse_radiation <= 0.0 {
        return 0.0;
    }
    let Some(geometry) = backwards_geometry(timestamp, dt_seconds, latitude, longitude, 3.0) else {
        return 0.0;
    };
    let daylight = backwards_sun_elevation(geometry, true);
    let daylight_delta = {
        let value = geometry.p1_limited - geometry.p10_limited;
        if value < 0.0 {
            value.min(-0.001)
        } else {
            value.max(0.001)
        }
    };
    let xx_daylight = (geometry.t1.sin() * -(geometry.p1_limited - geometry.p0).cos()
        - geometry.t1.sin() * -(geometry.p10_limited - geometry.p0).cos())
        / daylight_delta;
    let yy_left = geometry.p1_limited * geometry.t0.sin() * geometry.t1.cos()
        - geometry.t0.cos() * geometry.t1.sin() * (geometry.p1_limited - geometry.p0).sin();
    let yy_right = geometry.p10_limited * geometry.t0.sin() * geometry.t1.cos()
        - geometry.t0.cos() * geometry.t1.sin() * (geometry.p10_limited - geometry.p0).sin();
    let yy_daylight = (yy_left - yy_right) / (geometry.p1_limited - geometry.p10_limited);

    let zenith = daylight.acos();
    let azimuth = xx_daylight.atan2(yy_daylight);
    let array_azimuth = if array_azimuth_degrees.is_nan() {
        azimuth + PI
    } else {
        radians(array_azimuth_degrees)
    };
    let tilt = if tilt_degrees.is_nan() {
        zenith
    } else {
        radians(tilt_degrees)
    };
    let sky_view_factor = (1.0 + tilt.cos()) / 2.0;
    let module_diffuse = sky_view_factor * diffuse_radiation;
    let module_albedo = (direct_radiation + diffuse_radiation) * 0.2 * (1.0 - sky_view_factor);
    let dni = if daylight <= 0.0001 {
        direct_radiation
    } else {
        direct_radiation / daylight
    };
    let angle_of_incidence_cosine = (zenith.cos() * tilt.cos()
        + zenith.sin() * tilt.sin() * (azimuth - PI - array_azimuth).cos())
    .max(0.0);
    let gti = dni * angle_of_incidence_cosine + module_diffuse + module_albedo;
    if !convert_to_instant {
        return gti;
    }
    let Some(no_cutoff) = backwards_geometry(timestamp, dt_seconds, latitude, longitude, 0.0)
    else {
        return 0.0;
    };
    let backwards = backwards_sun_elevation(no_cutoff, false).abs();
    if backwards <= 0.0 {
        return 0.0;
    }
    gti * instant_sun_elevation(no_cutoff).max(0.0) / backwards
}

pub fn backwards_sunshine_duration(
    direct_radiation: f32,
    timestamp: DateTime<Utc>,
    dt_seconds: i64,
    latitude: f32,
    longitude: f32,
) -> f32 {
    if !direct_radiation.is_finite() {
        return f32::NAN;
    }
    if direct_radiation <= 0.0 {
        return 0.0;
    }
    let Some(geometry) = backwards_geometry(timestamp, dt_seconds, latitude, longitude, 0.0) else {
        return 0.0;
    };
    let bounded_seconds = dt_seconds as f32
        * ((geometry.p1_limited - geometry.p10_limited) / (geometry.p10 - geometry.p1)).abs();
    let daylight = backwards_sun_elevation(geometry, true);
    let dni = if daylight <= 0.0001 {
        direct_radiation
    } else {
        direct_radiation / daylight
    };
    ((dni - 60.0).max(0.0) / 120.0 * bounded_seconds).min(bounded_seconds)
}

pub fn extra_terrestrial_radiation_backwards(
    timestamp: DateTime<Utc>,
    dt_seconds: i64,
    latitude: f32,
    longitude: f32,
) -> f32 {
    extra_terrestrial_radiation_factor_backwards(timestamp, dt_seconds, latitude, longitude)
        * SOLAR_CONSTANT
}

/// Normalized extraterrestrial radiation factor used internally by Open-Meteo's
/// solar interpolation. Keep this value before multiplying by `SOLAR_CONSTANT`:
/// multiplying and dividing it back can lose one ULP at public quantization
/// boundaries.
pub fn extra_terrestrial_radiation_factor_backwards(
    timestamp: DateTime<Utc>,
    dt_seconds: i64,
    latitude: f32,
    longitude: f32,
) -> f32 {
    let Some(geometry) = backwards_geometry(timestamp, dt_seconds, latitude, longitude, 0.0) else {
        return 0.0;
    };
    let seconds = timestamp.timestamp().rem_euclid(SECONDS_PER_AVERAGE_YEAR) as f32;
    let day = seconds / SECONDS_PER_DAY as f32 - 4.0 + 1.0;
    let sun_radius = 1.0 - 0.01672 * radians((360.0 / 365.256_38) * day).cos();
    backwards_sun_elevation(geometry, false) / (sun_radius * sun_radius)
}

pub fn is_day(timestamp: DateTime<Utc>, latitude: f32, longitude: f32) -> f32 {
    let universal_offset = (longitude / 15.0 * 3600.0) as i64;
    let local_midnight = (timestamp.timestamp() + universal_offset).div_euclid(SECONDS_PER_DAY)
        * SECONDS_PER_DAY
        - universal_offset;
    let local_midday = local_midnight + ((12.0 - longitude / 15.0) * 3600.0) as i64;
    let midday = DateTime::from_timestamp(local_midday, 0).expect("valid solar timestamp");
    let position = sun_position(midday);
    let declination = radians(position.declination_degrees);
    let latitude = radians(latitude);
    let alpha = radians(0.83333);
    let arg =
        -(alpha.sin() + latitude.sin() * declination.sin()) / (latitude.cos() * declination.cos());
    if arg > 1.0 {
        return 0.0;
    }
    if arg < -1.0 {
        return 1.0;
    }
    let noon = 12.0 - longitude / 15.0;
    let hours = arg.acos() / radians(15.0);
    let rise = ((noon - hours - position.equation_of_time_hours) * 3600.0) as i64;
    let set = ((noon + hours - position.equation_of_time_hours) * 3600.0) as i64;
    let seconds_since_midnight =
        (timestamp.timestamp() + universal_offset).rem_euclid(SECONDS_PER_DAY);
    if seconds_since_midnight > rise + universal_offset
        && seconds_since_midnight < set + universal_offset
    {
        1.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn official_lookup_returns_finite_solar_position() {
        let timestamp = Utc.with_ymd_and_hms(2026, 7, 14, 6, 0, 0).unwrap();
        let position = sun_position(timestamp);
        assert!(position.declination_degrees.is_finite());
        assert!(position.equation_of_time_hours.is_finite());
        assert!((20.0..25.0).contains(&position.declination_degrees));
        assert!((-0.2..-0.05).contains(&position.equation_of_time_hours));
    }

    #[test]
    fn instant_factor_and_daylight_are_bounded() {
        let timestamp = Utc.with_ymd_and_hms(2026, 7, 14, 6, 0, 0).unwrap();
        let factor = backwards_to_instant_factor(timestamp, 3600, 29.58, 106.52);
        assert!(factor.is_finite() && factor >= 0.0);
        assert_eq!(is_day(timestamp, 29.58, 106.52), 1.0);
    }

    #[test]
    fn is_day_matches_official_reference() {
        let expected = [
            0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0,
            1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0,
            1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        ];
        let start = Utc.with_ymd_and_hms(2023, 4, 6, 0, 0, 0).unwrap();
        let actual = (0..48)
            .map(|hour| is_day(start + chrono::Duration::hours(hour), 52.52, 13.42))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn is_day_matches_frozen_sunrise_boundary() {
        let before_sunrise = Utc.with_ymd_and_hms(2026, 7, 14, 22, 0, 0).unwrap();
        let after_sunrise = Utc.with_ymd_and_hms(2026, 7, 14, 23, 0, 0).unwrap();
        assert_eq!(is_day(before_sunrise, 29.580215, 106.52344), 0.0);
        assert_eq!(is_day(after_sunrise, 29.580215, 106.52344), 1.0);
    }

    #[test]
    fn pi_matches_official_swift_float_bit_pattern() {
        assert_eq!(PI.to_bits(), 0x4049_0fda);
    }

    #[test]
    fn sunshine_duration_matches_official_swift_float_output() {
        let timestamp = Utc.timestamp_opt(1_784_070_000, 0).unwrap();
        let duration = backwards_sunshine_duration(200.0, timestamp, 3600, 29.580215, 106.52344);
        assert_eq!(duration.to_bits(), 1_162_005_532);
    }

    #[test]
    fn diffuse_and_sunshine_match_captured_ecmwf_sunrise_frame() {
        let timestamp = Utc.with_ymd_and_hms(2026, 8, 6, 2, 0, 0).unwrap();
        let diffuse = backwards_diffuse_radiation(35.0, timestamp, 3600, 0.0, 70.0);
        let duration = backwards_sunshine_duration(35.0 - diffuse, timestamp, 3600, 0.0, 70.0);

        assert_eq!(diffuse.to_bits(), 0x41e8_6c5f);
        assert_eq!(duration.to_bits(), 0x43c9_db7d);
        assert_eq!((duration * 100.0).round() / 100.0, 403.71);
    }

    #[test]
    fn normalized_radiation_preserves_official_interpolation_ulp() {
        let timestamp = Utc.timestamp_opt(1_785_222_000, 0).unwrap();
        let factor = extra_terrestrial_radiation_factor_backwards(
            timestamp,
            3600,
            f32::from_bits(0x4163_bd08),
            f32::from_bits(0x42d2_3c00),
        );
        assert_eq!(factor.to_bits(), 0x3f67_b48d);
        assert_eq!(
            (factor * SOLAR_CONSTANT).to_bits(),
            extra_terrestrial_radiation_backwards(
                timestamp,
                3600,
                f32::from_bits(0x4163_bd08),
                f32::from_bits(0x42d2_3c00),
            )
            .to_bits()
        );
    }

    #[test]
    fn backwards_dni_matches_official_swift_reference() {
        let start = Utc.with_ymd_and_hms(2022, 7, 31, 0, 0, 0).unwrap();
        let direct = [
            0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 7.0, 116.0, 305.0, 485.0, 615.0, 680.0, 681.0, 579.0,
            428.0, 272.0, 87.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        ];
        let expected = [
            0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 88.22421, 525.6257, 730.035, 838.7038, 889.7469,
            908.00757, 911.16675, 843.0275, 749.2078, 665.61414, 414.24954, 40.669086, 0.0, 0.0,
            0.0, 0.0, 0.0, 0.0,
        ];
        for (hour, (direct, expected)) in direct.into_iter().zip(expected).enumerate() {
            let actual = backwards_direct_normal_irradiance(
                direct,
                start + chrono::Duration::hours(hour as i64),
                3600,
                -22.5,
                17.0,
                false,
            );
            assert!(
                (actual - expected).abs() < 0.01,
                "hour={hour} actual={actual:?} expected={expected:?}"
            );
        }
    }

    #[test]
    fn backwards_dni_matches_frozen_ecmwf_sunset_frame() {
        let timestamp = Utc.with_ymd_and_hms(2026, 7, 26, 16, 0, 0).unwrap();
        let direct = 15.0 - backwards_diffuse_radiation(15.0, timestamp, 3600, 58.0, 70.0);
        let actual = backwards_direct_normal_irradiance(direct, timestamp, 3600, 58.0, 70.0, false);
        assert!(
            (actual - 18.7).abs() < 0.05,
            "actual={actual:?}, bits={:08x}",
            actual.to_bits()
        );
    }

    #[test]
    fn horizontal_gti_matches_frozen_ecmwf_sunset_cutoff() {
        let timestamp = Utc.with_ymd_and_hms(2026, 7, 23, 12, 0, 0).unwrap();
        let diffuse = backwards_diffuse_radiation(1.0, timestamp, 3600, 58.0, 140.0);
        let actual = backwards_global_tilted_irradiance(
            1.0 - diffuse,
            diffuse,
            timestamp,
            3600,
            58.0,
            140.0,
            0.0,
            0.0,
            false,
        );
        assert_eq!(actual, 0.0);
    }
}
