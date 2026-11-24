//! Geodesy helpers. Port of `greatcircle`, `bearing` and the angle helpers from upstream
//! `util.c` / `util.h`. Float (`f32`) and double steps follow upstream so results match.

use std::f64::consts::PI;

const DEGR: f64 = PI / 180.0;

fn to_rad(d: f64) -> f64 {
    d * DEGR
}

/// Distance in metres between two points given in degrees. Small distances use a cheap
/// flat-earth approximation; others a spherical law of cosines. `approx` forces the former.
pub fn greatcircle(lat0: f64, lon0: f64, lat1: f64, lon1: f64, approx: bool) -> f64 {
    if lat0 == lat1 && lon0 == lon1 {
        return 0.0;
    }
    let (lat0, lon0, lat1, lon1) = (to_rad(lat0), to_rad(lon0), to_rad(lat1), to_rad(lon1));
    let dlat = (lat1 - lat0).abs();
    let dlon = (lon1 - lon0).abs();

    // The earth is flat enough over a few degrees; difference to haversine is < 0.04%.
    if approx || (dlat < 3.0 * DEGR && dlon < 3.0 * DEGR && lat1.abs() < 80.0 * DEGR) {
        let er = 6_378_137.0_f32; // equatorial radius, 6378.1370 km
        let mr = 6_356_752.5_f32; // polar radius, 6356.7523 km rounded to the nearest f32
        let avglat = lat0 as f32 + (lat1 as f32 - lat0 as f32) * 0.5;
        let dmer = dlat as f32 * er;
        let dequ = dlon as f32 * mr * avglat.cos();
        return (dmer * dmer + dequ * dequ).sqrt() as f64;
    }

    // spherical law of cosines, in floats if the latitudes differ sufficiently
    if dlat > DEGR && dlon > DEGR {
        let (l0, l1, dl) = (lat0 as f32, lat1 as f32, dlon as f32);
        return (6371e3f32 * (l0.sin() * l1.sin() + l0.cos() * l1.cos() * dl.cos()).acos()) as f64;
    }
    6371e3 * (lat0.sin() * lat1.sin() + lat0.cos() * lat1.cos() * dlon.cos()).acos()
}

/// Initial bearing in degrees (0..360) from point 0 to point 1.
pub fn bearing(lat0: f64, lon0: f64, lat1: f64, lon1: f64) -> f64 {
    let (lat0, lon0, lat1, lon1) = (
        to_rad(lat0) as f32,
        to_rad(lon0) as f32,
        to_rad(lat1) as f32,
        to_rad(lon1) as f32,
    );
    let y = (lon1 - lon0).sin() * lat1.cos();
    let x = lat0.cos() * lat1.sin() - lat0.sin() * lat1.cos() * (lon1 - lon0).cos();
    let mut res = y.atan2(x) * (180.0 / std::f32::consts::PI) + 360.0;
    while res > 360.0 {
        res -= 360.0;
    }
    res as f64
}

/// Normalise a difference into `-pi..=pi` (pass 180 for degrees).
pub fn norm_diff(mut a: f64, pi: f64) -> f64 {
    if a < -pi {
        a += 2.0 * pi;
    }
    if a > pi {
        a -= 2.0 * pi;
    }
    a
}

/// Normalise an angle into `0..2*pi` (pass 180 for degrees).
pub fn norm_angle(mut a: f64, pi: f64) -> f64 {
    if a < 0.0 {
        a += 2.0 * pi;
    }
    if a >= 2.0 * pi {
        a -= 2.0 * pi;
    }
    a
}

/// Impossible or suspicious positions.
pub fn bogus_lat_lon(lat: f64, lon: f64) -> bool {
    if lat.abs() >= 90.0 || lon.abs() >= 180.0 {
        return true;
    }
    if lat == 0.0 && (lon == -90.0 || lon == 90.0 || lon == 0.0) {
        return true;
    }
    lat.abs() < 0.01 && lon.abs() < 0.01
}

pub const KNOTS_TO_METERS_PER_SECOND: f32 = 1852.0 / 3600.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_distances() {
        // London to Paris is about 344 km (law of cosines path)
        let d = greatcircle(51.5074, -0.1278, 48.8566, 2.3522, false);
        assert!((d - 343_500.0).abs() < 3_000.0, "{d}");
        // short hop uses the flat approximation: 0.01 degree of latitude is ~1.11 km
        let d = greatcircle(52.0, 4.0, 52.01, 4.0, false);
        assert!((d - 1113.0).abs() < 10.0, "{d}");
        assert_eq!(greatcircle(1.0, 2.0, 1.0, 2.0, false), 0.0);
    }

    #[test]
    fn bearings() {
        assert!(
            (bearing(0.0, 0.0, 1.0, 0.0) - 0.0).abs() < 1e-3
                || (bearing(0.0, 0.0, 1.0, 0.0) - 360.0).abs() < 1e-3
        );
        assert!((bearing(0.0, 0.0, 0.0, 1.0) - 90.0).abs() < 1e-3);
        assert!((bearing(0.0, 0.0, -1.0, 0.0) - 180.0).abs() < 1e-3);
        assert!((bearing(0.0, 0.0, 0.0, -1.0) - 270.0).abs() < 1e-3);
    }

    #[test]
    fn angle_helpers_and_bogus() {
        assert_eq!(norm_diff(190.0, 180.0), -170.0);
        assert_eq!(norm_diff(-190.0, 180.0), 170.0);
        assert_eq!(norm_angle(-10.0, 180.0), 350.0);
        assert!(bogus_lat_lon(0.0, 0.0));
        assert!(bogus_lat_lon(91.0, 0.0));
        assert!(!bogus_lat_lon(52.0, 4.0));
    }
}
