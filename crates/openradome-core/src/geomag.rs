//! World Magnetic Model (WMM2025) declination, used to turn a magnetic heading into a true
//! heading. Port of upstream `geomag.c`, which is NOAA's public domain `geomag` program
//! (U.S. Government work, not subject to copyright in the United States).

use std::sync::OnceLock;

const MAXORD: usize = 12;
const COEFFICIENTS: &str = include_str!("wmm2025.cof");

/// Model coefficients converted to unnormalised form.
struct Model {
    epoch: f64,
    c: [[f64; 13]; 13],
    cd: [[f64; 13]; 13],
    k: [[f64; 13]; 13],
    fn_: [f64; 13],
    fm: [f64; 13],
}

fn model() -> &'static Model {
    static M: OnceLock<Model> = OnceLock::new();
    M.get_or_init(|| {
        let mut lines = COEFFICIENTS.lines();
        let epoch: f64 = lines
            .next()
            .and_then(|l| l.split_whitespace().next())
            .and_then(|t| t.parse().ok())
            .expect("coefficient file header");
        let mut c = [[0.0; 13]; 13];
        let mut cd = [[0.0; 13]; 13];
        for line in lines {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 6 {
                continue;
            }
            let n: usize = f[0].parse().expect("n");
            let m: usize = f[1].parse().expect("m");
            let v: Vec<f64> = f[2..6]
                .iter()
                .map(|t| t.parse().expect("coefficient"))
                .collect();
            if n > MAXORD {
                continue;
            }
            c[m][n] = v[0];
            cd[m][n] = v[2];
            if m != 0 {
                c[n][m - 1] = v[1];
                cd[n][m - 1] = v[3];
            }
        }

        // Convert Schmidt normalised Gauss coefficients to unnormalised.
        let mut snorm = [0.0f64; 169];
        snorm[0] = 1.0;
        let mut k = [[0.0; 13]; 13];
        let mut fn_ = [0.0; 13];
        let mut fm = [0.0; 13];
        for n in 1..=MAXORD {
            snorm[n] = snorm[n - 1] * (2 * n - 1) as f64 / n as f64;
            let mut j = 2.0;
            for m in 0..=n {
                k[m][n] = (((n - 1) * (n - 1)) as f64 - (m * m) as f64)
                    / ((2 * n - 1) * (2 * n).saturating_sub(3).max(1)) as f64;
                if m > 0 {
                    let flnmj = ((n - m + 1) as f64 * j) / (n + m) as f64;
                    snorm[n + m * 13] = snorm[n + (m - 1) * 13] * flnmj.sqrt();
                    j = 1.0;
                    c[n][m - 1] *= snorm[n + m * 13];
                    cd[n][m - 1] *= snorm[n + m * 13];
                }
                c[m][n] *= snorm[n + m * 13];
                cd[m][n] *= snorm[n + m * 13];
            }
            fn_[n] = (n + 1) as f64;
            fm[n] = n as f64;
        }
        k[1][1] = 0.0;
        Model {
            epoch,
            c,
            cd,
            k,
            fn_,
            fm,
        }
    })
}

/// Result of a field computation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Field {
    /// Magnetic declination in degrees, east positive.
    pub declination: f64,
    /// Inclination (dip) in degrees, down positive.
    pub dip: f64,
    /// Total intensity in nT.
    pub intensity: f64,
}

/// Magnetic field at `alt_km` above the WGS84 ellipsoid, geodetic `lat`/`lon` in degrees,
/// at decimal year `time`.
pub fn field(alt_km: f64, lat: f64, lon: f64, time: f64) -> Field {
    let m = model();
    let a = 6378.137f64;
    let b = 6356.7523142f64;
    let re = 6371.2f64;
    let (a2, b2) = (a * a, b * b);
    let c2 = a2 - b2;
    let (a4, b4) = (a2 * a2, b2 * b2);
    let c4 = a4 - b4;

    let dt = time - m.epoch;
    // upstream's truncated value of pi, kept so results match its output digit for digit
    #[allow(clippy::approx_constant)]
    let dtr = 3.14159265359f64 / 180.0;
    let rlon = lon * dtr;
    let rlat = lat * dtr;
    let (srlon, srlat) = (rlon.sin(), rlat.sin());
    let (crlon, crlat) = (rlon.cos(), rlat.cos());
    let srlat2 = srlat * srlat;
    let crlat2 = crlat * crlat;
    let mut sp = [0.0f64; 13];
    let mut cp = [0.0f64; 13];
    cp[0] = 1.0;
    sp[1] = srlon;
    cp[1] = crlon;

    // geodetic to spherical coordinates
    let q = (a2 - c2 * srlat2).sqrt();
    let q1 = alt_km * q;
    let q2 = ((q1 + a2) / (q1 + b2)) * ((q1 + a2) / (q1 + b2));
    let ct = srlat / (q2 * crlat2 + srlat2).sqrt();
    let st = (1.0 - ct * ct).sqrt();
    let r2 = alt_km * alt_km + 2.0 * q1 + (a4 - c4 * srlat2) / (q * q);
    let r = r2.sqrt();
    let d = (a2 * crlat2 + b2 * srlat2).sqrt();
    let ca = (alt_km + d) / r;
    let sa = c2 * crlat * srlat / (r * d);

    for i in 2..=MAXORD {
        sp[i] = sp[1] * cp[i - 1] + cp[1] * sp[i - 1];
        cp[i] = cp[1] * cp[i - 1] - sp[1] * sp[i - 1];
    }

    let aor = re / r;
    let mut ar = aor * aor;
    let (mut br, mut bt, mut bp, mut bpp) = (0.0, 0.0, 0.0, 0.0);
    // Unnormalised associated Legendre polynomials (index n + m * 13) and derivatives [m][n].
    let mut p = [0.0f64; 169];
    p[0] = 1.0;
    let mut dp = [[0.0f64; 13]; 13];
    let mut pp = [0.0f64; 13];
    pp[0] = 1.0;
    let mut tc = [[0.0f64; 13]; 13];

    for n in 1..=MAXORD {
        ar *= aor;
        for mm in 0..=n {
            // recursion relations
            if n == mm {
                p[n + mm * 13] = st * p[n - 1 + (mm - 1) * 13];
                dp[mm][n] = st * dp[mm - 1][n - 1] + ct * p[n - 1 + (mm - 1) * 13];
            } else if n == 1 && mm == 0 {
                p[n + mm * 13] = ct * p[n - 1 + mm * 13];
                dp[mm][n] = ct * dp[mm][n - 1] - st * p[n - 1 + mm * 13];
            } else if n > 1 {
                if mm + 2 > n {
                    p[n - 2 + mm * 13] = 0.0;
                    dp[mm][n - 2] = 0.0;
                }
                p[n + mm * 13] = ct * p[n - 1 + mm * 13] - m.k[mm][n] * p[n - 2 + mm * 13];
                dp[mm][n] =
                    ct * dp[mm][n - 1] - st * p[n - 1 + mm * 13] - m.k[mm][n] * dp[mm][n - 2];
            }

            // time adjust the Gauss coefficients
            tc[mm][n] = m.c[mm][n] + dt * m.cd[mm][n];
            if mm != 0 {
                tc[n][mm - 1] = m.c[n][mm - 1] + dt * m.cd[n][mm - 1];
            }

            // accumulate the spherical harmonic expansions
            let par = ar * p[n + mm * 13];
            let (temp1, temp2) = if mm == 0 {
                (tc[mm][n] * cp[mm], tc[mm][n] * sp[mm])
            } else {
                (
                    tc[mm][n] * cp[mm] + tc[n][mm - 1] * sp[mm],
                    tc[mm][n] * sp[mm] - tc[n][mm - 1] * cp[mm],
                )
            };
            bt -= ar * temp1 * dp[mm][n];
            bp += m.fm[mm] * temp2 * par;
            br += m.fn_[n] * temp1 * par;

            // special case: geographic poles
            if st == 0.0 && mm == 1 {
                pp[n] = if n == 1 {
                    pp[n - 1]
                } else {
                    ct * pp[n - 1] - m.k[mm][n] * pp[n - 2]
                };
                let parp = ar * pp[n];
                bpp += m.fm[mm] * temp2 * parp;
            }
        }
    }
    bp = if st == 0.0 { bpp } else { bp / st };

    // rotate to geodetic coordinates
    let bx = -bt * ca - br * sa;
    let by = bp;
    let bz = bt * sa - br * ca;
    let bh = (bx * bx + by * by).sqrt();
    Field {
        intensity: (bh * bh + bz * bz).sqrt(),
        declination: by.atan2(bx) / dtr,
        dip: bz.atan2(bh) / dtr,
    }
}

/// Decimal year for a Unix time in ms, as upstream computes it
/// (`1900 + tm_year + tm_yday / 365.0`, UTC).
pub fn decimal_year(ms: i64) -> f64 {
    let days = ms.div_euclid(1000).div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    if month <= 2 {
        year += 1;
    }
    // day of the year, counted from January 1st = 0
    let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let cum = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let day = doy - (153 * mp + 2) / 5 + 1;
    let mut yday = cum[(month - 1) as usize] + day - 1;
    if is_leap && month > 2 {
        yday += 1;
    }
    year as f64 + yday as f64 / 365.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_year_values() {
        assert_eq!(decimal_year(0), 1970.0);
        // 2025-07-02 is day 182 of a non-leap year (counting from 0)
        let ms = 1_751_414_400_000; // 2025-07-02T00:00:00Z
        assert!((decimal_year(ms) - (2025.0 + 182.0 / 365.0)).abs() < 1e-9);
        // 2024-03-01 is day 60 of a leap year
        assert!((decimal_year(1_709_251_200_000) - (2024.0 + 60.0 / 365.0)).abs() < 1e-9);
    }

    /// Reference values produced by compiling upstream's `geomag.c`:
    /// `(alt_km, lat, lon, year, declination, dip, intensity)`.
    const REFERENCE: [(f64, f64, f64, f64, f64, f64, f64); 11] = [
        (
            0.0,
            80.0,
            0.0,
            2025.0,
            1.281482116250,
            83.210579542267,
            55178.454620311,
        ),
        (
            0.0,
            0.0,
            120.0,
            2025.0,
            -0.158274143781,
            -14.930601264937,
            41064.294091622,
        ),
        (
            0.0,
            -80.0,
            240.0,
            2025.0,
            68.775385175267,
            -72.004988257701,
            54698.166840698,
        ),
        (
            3.0,
            40.7,
            -74.0,
            2026.5,
            -12.466610469555,
            65.539296514872,
            50700.175539230,
        ),
        (
            0.0,
            -33.9,
            151.2,
            2025.5,
            12.808748308720,
            -64.420436653579,
            57015.731051673,
        ),
        (
            0.0,
            64.1,
            -21.9,
            2027.0,
            -10.947453690929,
            75.390522516242,
            52579.486392589,
        ),
        (
            5.0,
            35.7,
            139.7,
            2026.0,
            -7.892238274784,
            49.555788785538,
            46755.959297449,
        ),
        (
            0.0,
            90.0,
            0.0,
            2026.0,
            16.108878226777,
            88.196218022824,
            56910.460159443,
        ),
        (
            0.0,
            -90.0,
            0.0,
            2026.0,
            -31.678431646857,
            -71.962781883905,
            54318.271058844,
        ),
        (
            0.0,
            0.0,
            0.0,
            2025.0,
            -4.016243861527,
            -30.188961827617,
            31839.907862169,
        ),
        (
            12.0,
            25.0,
            55.0,
            2026.9,
            2.218244946044,
            39.471266340435,
            44108.317011483,
        ),
    ];

    #[test]
    fn matches_upstream_geomag() {
        for (alt, lat, lon, year, dec, dip, ti) in REFERENCE {
            let f = field(alt, lat, lon, year);
            assert!(
                (f.declination - dec).abs() < 1e-9,
                "dec at {lat},{lon}: {f:?} vs {dec}"
            );
            assert!(
                (f.dip - dip).abs() < 1e-9,
                "dip at {lat},{lon}: {f:?} vs {dip}"
            );
            assert!(
                (f.intensity - ti).abs() < 1e-5,
                "ti at {lat},{lon}: {f:?} vs {ti}"
            );
        }
    }

    #[test]
    fn london_is_a_small_easterly_declination() {
        // London, late 2026
        let f = field(0.0, 51.5, 0.0, 2026.85);
        assert!(f.declination > 0.0 && f.declination < 3.0, "{f:?}");
        assert!(f.dip > 60.0 && f.dip < 70.0, "{f:?}");
        assert!(f.intensity > 47_000.0 && f.intensity < 51_000.0, "{f:?}");
    }

    #[test]
    fn secular_variation_moves_the_field() {
        let a = field(0.0, 50.0, 10.0, 2025.0);
        let b = field(0.0, 50.0, 10.0, 2027.0);
        assert!((a.declination - b.declination).abs() > 0.05);
        // altitude changes intensity but barely changes declination
        let high = field(11.0, 50.0, 10.0, 2025.0);
        assert!(high.intensity < a.intensity);
        assert!((high.declination - a.declination).abs() < 0.2);
    }

    #[test]
    fn poles_do_not_produce_nan() {
        for lat in [90.0, -90.0] {
            let f = field(0.0, lat, 0.0, 2025.5);
            assert!(
                f.declination.is_finite() && f.intensity.is_finite(),
                "{f:?}"
            );
        }
    }
}
