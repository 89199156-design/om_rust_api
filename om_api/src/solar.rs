use chrono::{DateTime, Timelike, Utc};
use std::f64::consts::PI;

const SECONDS_PER_AVERAGE_YEAR: i64 = 31_557_600;

fn degrees_to_radians(value: f64) -> f64 {
    value * PI / 180.0
}

fn limit_degrees(value: f64) -> f64 {
    value.rem_euclid(360.0)
}

/// Solar declination in degrees and equation of time in hours.
///
/// This is the NOAA Julian-century form of the same NREL SPA quantities used
/// by Open-Meteo's fast lookup.  The backwards integral below is the official
/// Open-Meteo Zensun implementation translated directly to Rust.
fn sun_position(time: DateTime<Utc>) -> (f64, f64) {
    let julian_day = time.timestamp_millis() as f64 / 86_400_000.0 + 2_440_587.5;
    let century = (julian_day - 2_451_545.0) / 36_525.0;
    let geom_mean_long = limit_degrees(280.46646 + century * (36_000.76983 + 0.0003032 * century));
    let geom_mean_anomaly = 357.52911 + century * (35_999.05029 - 0.0001537 * century);
    let eccentricity = 0.016708634 - century * (0.000042037 + 0.0000001267 * century);
    let anomaly = degrees_to_radians(geom_mean_anomaly);
    let equation_center = anomaly.sin() * (1.914602 - century * (0.004817 + 0.000014 * century))
        + (2.0 * anomaly).sin() * (0.019993 - 0.000101 * century)
        + (3.0 * anomaly).sin() * 0.000289;
    let true_long = geom_mean_long + equation_center;
    let omega = 125.04 - 1934.136 * century;
    let apparent_long = true_long - 0.00569 - 0.00478 * degrees_to_radians(omega).sin();
    let mean_obliquity = 23.0
        + (26.0 + (21.448 - century * (46.815 + century * (0.00059 - century * 0.001813))) / 60.0)
            / 60.0;
    let obliquity = mean_obliquity + 0.00256 * degrees_to_radians(omega).cos();
    let obliquity_rad = degrees_to_radians(obliquity);
    let declination = (obliquity_rad.sin() * degrees_to_radians(apparent_long).sin())
        .asin()
        .to_degrees();

    let y = (obliquity_rad / 2.0).tan().powi(2);
    let mean_long = degrees_to_radians(geom_mean_long);
    let equation_minutes = 4.0
        * (y * (2.0 * mean_long).sin() - 2.0 * eccentricity * anomaly.sin()
            + 4.0 * eccentricity * y * anomaly.sin() * (2.0 * mean_long).cos()
            - 0.5 * y * y * (4.0 * mean_long).sin()
            - 1.25 * eccentricity * eccentricity * (2.0 * anomaly).sin())
        .to_degrees();
    (declination, equation_minutes / 60.0)
}

fn sun_radius(time: DateTime<Utc>) -> f64 {
    let second_in_average_year = time.timestamp().rem_euclid(SECONDS_PER_AVERAGE_YEAR);
    let day = second_in_average_year as f64 / 86_400.0 - 4.0 + 1.0;
    1.0 - 0.01672 * degrees_to_radians((360.0 / 365.256363) * day).cos()
}

/// Dimensionless extraterrestrial-radiation factor averaged over the hour
/// ending at `time`. This mirrors `Zensun.calculateRadiationBackwardsAveraged`.
pub fn backwards_averaged_factor(time: DateTime<Utc>, latitude: f64, longitude: f64) -> f32 {
    let (declination, equation_of_time_hours) = sun_position(time);
    let radius = sun_radius(time);
    let universal_time =
        time.hour() as f64 + time.minute() as f64 / 60.0 + time.second() as f64 / 3600.0;
    let sun_colatitude = degrees_to_radians(90.0 - declination);
    let sun_longitude = -15.0 * (universal_time - 12.0 + equation_of_time_hours);
    let p1 = degrees_to_radians(sun_longitude);
    let p10 = degrees_to_radians(-15.0 * (universal_time - 1.0 - 12.0 + equation_of_time_hours));
    let point_colatitude = degrees_to_radians(90.0 - latitude);
    let mut point_longitude = degrees_to_radians(longitude);
    if point_longitude < p1 - PI {
        point_longitude += 2.0 * PI;
    }
    if point_longitude > p1 + PI {
        point_longitude -= 2.0 * PI;
    }

    let denominator = point_colatitude.sin() * sun_colatitude.sin();
    if denominator.abs() < f64::EPSILON {
        return 0.0;
    }
    let argument = -(point_colatitude.cos() * sun_colatitude.cos()) / denominator;
    let hour_angle = if !(-1.0..=1.0).contains(&argument) {
        PI
    } else {
        argument.acos()
    };
    let sunrise = point_longitude + hour_angle;
    let sunset = point_longitude - hour_angle;
    if p10 < sunset || p1 > sunrise {
        return 0.0;
    }
    let upper = sunrise.min(p10);
    let lower = sunset.max(p1);
    let left = point_colatitude.sin() * sun_colatitude.sin() * (upper - point_longitude).sin()
        + upper * point_colatitude.cos() * sun_colatitude.cos();
    let right = point_colatitude.sin() * sun_colatitude.sin() * (lower - point_longitude).sin()
        + lower * point_colatitude.cos() * sun_colatitude.cos();
    (((left - right) / (p10 - p1)) / (radius * radius)).max(0.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn backwards_factor_is_zero_at_night_and_positive_at_local_noon() {
        let night = Utc.with_ymd_and_hms(2026, 7, 14, 16, 0, 0).unwrap();
        let noon = Utc.with_ymd_and_hms(2026, 7, 14, 4, 0, 0).unwrap();
        assert_eq!(backwards_averaged_factor(night, 31.23, 121.47), 0.0);
        assert!(backwards_averaged_factor(noon, 31.23, 121.47) > 0.5);
    }

    #[test]
    fn radius_and_position_are_finite_across_leap_day() {
        let time = Utc.with_ymd_and_hms(2028, 2, 29, 12, 0, 0).unwrap();
        let (declination, equation) = sun_position(time);
        assert!(declination.is_finite());
        assert!(equation.is_finite());
        assert!(sun_radius(time).is_finite());
    }

    #[test]
    fn backwards_factor_matches_open_meteo_zensun_reference() {
        let southern_summer = Utc.with_ymd_and_hms(2020, 12, 26, 12, 0, 0).unwrap();
        let northern_summer = Utc.with_ymd_and_hms(2020, 6, 26, 12, 0, 0).unwrap();
        let southern_watts = backwards_averaged_factor(southern_summer, -23.5, 0.0) * 1367.7;
        let northern_watts = backwards_averaged_factor(northern_summer, 23.5, 0.0) * 1367.7;
        assert!((southern_watts - 1400.073).abs() < 0.1);
        assert!((northern_watts - 1308.9365).abs() < 0.1);
    }
}
