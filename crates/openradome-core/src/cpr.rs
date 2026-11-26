//! Compact Position Reporting (CPR) decoding. Port of upstream `cpr.c`.
//!
//! Latitude/longitude are 17-bit values. Even (`fflag = false`) and odd
//! (`fflag = true`) frames are combined for a global decode, or a single frame
//! is decoded against a reference position (relative decode).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CprError {
    /// Latitude out of range: bad data (upstream returns -2).
    BadData,
    /// The two frames straddle a latitude zone boundary; try again later (upstream -1).
    ZoneCrossed,
    /// Relative decode result too far from the reference (upstream -1).
    OutOfRange,
}

impl CprError {
    /// The integer return code upstream uses.
    pub fn code(self) -> i32 {
        match self {
            CprError::BadData => -2,
            CprError::ZoneCrossed | CprError::OutOfRange => -1,
        }
    }
}

impl fmt::Display for CprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CprError::BadData => "bad CPR data",
            CprError::ZoneCrossed => "CPR frames cross a latitude zone",
            CprError::OutOfRange => "CPR result out of range of reference",
        })
    }
}

impl std::error::Error for CprError {}

pub type CprResult = Result<(f64, f64), CprError>;

const CPR_MAX: f64 = 131072.0; // 2^17

fn mod_int(a: i32, b: i32) -> i32 {
    a.rem_euclid(b)
}

fn mod_double(a: f64, b: f64) -> f64 {
    let res = a % b;
    if res < 0.0 { res + b } else { res }
}

/// Number of longitude zones at a latitude (table from 1090-WP-9-14).
pub fn nl(lat: f64) -> i32 {
    // Transition latitudes: NL is `n` for lat below TABLE[59 - n] (n = 59 down to 2).
    const T: [f64; 58] = [
        10.47047130,
        14.82817437,
        18.18626357,
        21.02939493,
        23.54504487,
        25.82924707,
        27.93898710,
        29.91135686,
        31.77209708,
        33.53993436,
        35.22899598,
        36.85025108,
        38.41241892,
        39.92256684,
        41.38651832,
        42.80914012,
        44.19454951,
        45.54626723,
        46.86733252,
        48.16039128,
        49.42776439,
        50.67150166,
        51.89342469,
        53.09516153,
        54.27817472,
        55.44378444,
        56.59318756,
        57.72747354,
        58.84763776,
        59.95459277,
        61.04917774,
        62.13216659,
        63.20427479,
        64.26616523,
        65.31845310,
        66.36171008,
        67.39646774,
        68.42322022,
        69.44242631,
        70.45451075,
        71.45986473,
        72.45884545,
        73.45177442,
        74.43893416,
        75.42056257,
        76.39684391,
        77.36789461,
        78.33374083,
        79.29428225,
        80.24923213,
        81.19801349,
        82.13956981,
        83.07199445,
        83.99173563,
        84.89166191,
        85.75541621,
        86.53536998,
        87.00000000,
    ];
    let lat = lat.abs(); // table is symmetric about the equator
    for (i, &limit) in T.iter().enumerate() {
        if lat < limit {
            return 59 - i as i32;
        }
    }
    1
}

fn n_function(lat: f64, fflag: bool) -> i32 {
    (nl(lat) - fflag as i32).max(1)
}

fn dlon(lat: f64, fflag: bool, surface: bool) -> f64 {
    (if surface { 90.0 } else { 360.0 }) / n_function(lat, fflag) as f64
}

/// Global decode of an even/odd airborne pair. `fflag` selects which frame is the
/// most recent (true = odd). Returns `(lat, lon)`.
pub fn decode_airborne(
    even_lat: u32,
    even_lon: u32,
    odd_lat: u32,
    odd_lon: u32,
    fflag: bool,
) -> CprResult {
    decode_global(even_lat, even_lon, odd_lat, odd_lon, fflag, false, None)
}

/// Global decode of a surface pair, resolved against a reference position
/// (receiver or last known position), since surface CPR only spans 90 degrees.
pub fn decode_surface(
    reflat: f64,
    reflon: f64,
    even_lat: u32,
    even_lon: u32,
    odd_lat: u32,
    odd_lon: u32,
    fflag: bool,
) -> CprResult {
    decode_global(
        even_lat,
        even_lon,
        odd_lat,
        odd_lon,
        fflag,
        true,
        Some((reflat, reflon)),
    )
}

fn decode_global(
    even_lat: u32,
    even_lon: u32,
    odd_lat: u32,
    odd_lon: u32,
    fflag: bool,
    surface: bool,
    reference: Option<(f64, f64)>,
) -> CprResult {
    let span = if surface { 90.0 } else { 360.0 };
    let dlat0 = span / 60.0;
    let dlat1 = span / 59.0;
    let (lat0, lat1) = (even_lat as f64, odd_lat as f64);
    let (lon0, lon1) = (even_lon as f64, odd_lon as f64);

    // Latitude index "j"
    let j = (((59.0 * lat0 - 60.0 * lat1) / CPR_MAX) + 0.5).floor() as i32;
    let mut rlat0 = dlat0 * (mod_int(j, 60) as f64 + lat0 / CPR_MAX);
    let mut rlat1 = dlat1 * (mod_int(j, 59) as f64 + lat1 / CPR_MAX);

    if let Some((reflat, _)) = reference {
        // Surface: only -90..0 and 0..90 are valid. Pick the quadrant closest to the
        // reference; 0 is ambiguous with -90 and +90.
        for r in [&mut rlat0, &mut rlat1] {
            if *r == 0.0 {
                if reflat < -45.0 {
                    *r = -90.0;
                } else if reflat > 45.0 {
                    *r = 90.0;
                }
            } else if *r - reflat > 45.0 {
                *r -= 90.0;
            }
        }
    } else {
        if rlat0 >= 270.0 {
            rlat0 -= 360.0;
        }
        if rlat1 >= 270.0 {
            rlat1 -= 360.0;
        }
    }

    if !(-90.0..=90.0).contains(&rlat0) || !(-90.0..=90.0).contains(&rlat1) {
        return Err(CprError::BadData);
    }
    if nl(rlat0) != nl(rlat1) {
        return Err(CprError::ZoneCrossed);
    }

    // Longitude index "m"
    let (rlat, mut rlon) = if fflag {
        let ni = n_function(rlat1, true);
        let m = ((((lon0 * (nl(rlat1) - 1) as f64) - (lon1 * nl(rlat1) as f64)) / CPR_MAX) + 0.5)
            .floor() as i32;
        (
            rlat1,
            dlon(rlat1, true, surface) * (mod_int(m, ni) as f64 + lon1 / CPR_MAX),
        )
    } else {
        let ni = n_function(rlat0, false);
        let m = ((((lon0 * (nl(rlat0) - 1) as f64) - (lon1 * nl(rlat0) as f64)) / CPR_MAX) + 0.5)
            .floor() as i32;
        (
            rlat0,
            dlon(rlat0, false, surface) * (mod_int(m, ni) as f64 + lon0 / CPR_MAX),
        )
    };

    if let Some((_, reflon)) = reference {
        // Surface longitude spans 90 degrees: move to the quadrant nearest the reference.
        rlon += ((reflon - rlon + 45.0) / 90.0).floor() * 90.0;
    }
    // Renormalize to -180 .. +180
    rlon -= ((rlon + 180.0) / 360.0).floor() * 360.0;

    Ok((rlat, rlon))
}

/// Decode a single frame relative to a reference position that must be within
/// half a cell (about 180 NM airborne, 45 NM surface).
pub fn decode_relative(
    reflat: f64,
    reflon: f64,
    cprlat: u32,
    cprlon: u32,
    fflag: bool,
    surface: bool,
) -> CprResult {
    let frac_lat = cprlat as f64 / CPR_MAX;
    let frac_lon = cprlon as f64 / CPR_MAX;
    let air_dlat = (if surface { 90.0 } else { 360.0 }) / if fflag { 59.0 } else { 60.0 };

    let j = ((reflat / air_dlat).floor()
        + (0.5 + mod_double(reflat, air_dlat) / air_dlat - frac_lat).floor()) as i32;
    let mut rlat = air_dlat * (j as f64 + frac_lat);
    if rlat >= 270.0 {
        rlat -= 360.0;
    }
    if !(-90.0..=90.0).contains(&rlat) {
        return Err(CprError::OutOfRange);
    }
    if (rlat - reflat).abs() > air_dlat / 2.0 {
        return Err(CprError::OutOfRange);
    }

    let air_dlon = dlon(rlat, fflag, surface);
    let m = ((reflon / air_dlon).floor()
        + (0.5 + mod_double(reflon, air_dlon) / air_dlon - frac_lon).floor()) as i32;
    let mut rlon = air_dlon * (m as f64 + frac_lon);
    if rlon > 180.0 {
        rlon -= 360.0;
    }
    if (rlon - reflon).abs() > air_dlon / 2.0 {
        return Err(CprError::OutOfRange);
    }
    Ok((rlat, rlon))
}

#[cfg(test)]
mod tests {
    use super::*;

    // From "The 1090 MHz Riddle": 8D40621D58C382D690C8AC2863A7 (even) and
    // 8D40621D58C386435CC412692AD6 (odd), position 52.2572 N, 3.91937 E.
    const EVEN: (u32, u32) = (93000, 51372);
    const ODD: (u32, u32) = (74158, 50194);

    #[test]
    fn nl_table_edges() {
        assert_eq!(nl(0.0), 59);
        assert_eq!(nl(10.4), 59);
        assert_eq!(nl(10.5), 58);
        assert_eq!(nl(-10.5), 58);
        assert_eq!(nl(52.2572), 36);
        assert_eq!(nl(86.9), 2);
        assert_eq!(nl(87.0), 1);
        assert_eq!(nl(90.0), 1);
    }

    #[test]
    fn global_airborne_even_and_odd() {
        // Most recent frame odd.
        let (lat, lon) = decode_airborne(EVEN.0, EVEN.1, ODD.0, ODD.1, true).unwrap();
        assert!((lat - 52.26578).abs() < 1e-4, "{lat}");
        assert!((lon - 3.93891).abs() < 1e-4, "{lon}");
        // Most recent frame even.
        let (lat, lon) = decode_airborne(EVEN.0, EVEN.1, ODD.0, ODD.1, false).unwrap();
        assert!((lat - 52.25720).abs() < 1e-4, "{lat}");
        assert!((lon - 3.91937).abs() < 1e-4, "{lon}");
    }

    #[test]
    fn relative_matches_global() {
        let (glat, glon) = decode_airborne(EVEN.0, EVEN.1, ODD.0, ODD.1, false).unwrap();
        let (rlat, rlon) = decode_relative(52.0, 4.0, EVEN.0, EVEN.1, false, false).unwrap();
        assert!((rlat - glat).abs() < 1e-6 && (rlon - glon).abs() < 1e-6);
    }

    #[test]
    fn relative_picks_the_cell_nearest_the_reference() {
        // A single frame is ambiguous; it resolves to whichever cell is nearest the reference.
        let (lat, lon) = decode_relative(10.0, 100.0, EVEN.0, EVEN.1, false, false).unwrap();
        assert!((lat - 10.0).abs() < 3.0 && (lon - 100.0).abs() < 10.0);
    }

    #[test]
    fn zone_crossing_is_reported() {
        // Frames that straddle an NL boundary must be rejected with the "try again" code.
        let mut found = None;
        'outer: for lat0 in (0..131072).step_by(61) {
            for lat1 in (0..131072).step_by(67) {
                if decode_airborne(lat0, 1000, lat1, 1000, false) == Err(CprError::ZoneCrossed) {
                    found = Some((lat0, lat1));
                    break 'outer;
                }
            }
        }
        assert!(found.is_some(), "no zone-crossing pair found");
        assert_eq!(CprError::ZoneCrossed.code(), -1);
        assert_eq!(CprError::BadData.code(), -2);
    }

    #[test]
    fn surface_resolves_to_reference_quadrant() {
        // Surface frames from the 1090 Riddle-style test (Schiphol area, ~52.32N 4.73E).
        let (lat, lon) = decode_surface(52.3, 4.7, 0, 0, 0, 0, false).unwrap();
        assert!((lat - 52.3).abs() < 45.0);
        assert!((lon - 4.7).abs() < 45.0 + 1e-9);
    }
}
