//! SBS / BaseStation output. Port of upstream `modesSendSBSOutput`.

use std::fmt::Write;

use crate::mode_s::{AirGround, HeadingType, ModesMessage, NON_ICAO_ADDRESS};

/// Civil date and time for a Unix time in milliseconds (UTC).
fn utc(ms: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let secs = ms.div_euclid(1000);
    let milli = ms.rem_euclid(1000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    // days since 1970-01-01 -> civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, rem / 3600, rem / 60 % 60, rem % 60, milli)
}

/// BaseStation transmission type for a message, `None` if it has none.
fn msg_type(mm: &ModesMessage) -> Option<u8> {
    Some(match mm.msgtype {
        4 | 20 => 5,
        5 | 21 => 6,
        0 | 16 => 7,
        11 => 8,
        17 | 18 => match mm.metype {
            1..=4 => 1,
            5..=8 => 2,
            9..=18 => 3,
            19 => 4,
            _ => return None,
        },
        _ => return None,
    })
}

/// Options that change what is printed.
#[derive(Debug, Clone, Copy, Default)]
pub struct SbsOptions {
    /// `--gnss`: prefer geometric altitude and rate.
    pub use_gnss: bool,
    /// Add signal level and emitter category fields.
    pub extra_fields: bool,
}

/// One `MSG,...` line (with CRLF) for a message from aircraft `addr`.
///
/// `geom_delta` is the aircraft's current geometric-barometric altitude difference, if
/// known. `now_ms` is the current time for the "generated" timestamp.
pub fn sbs_line(
    mm: &ModesMessage,
    addr: u32,
    geom_delta: Option<i32>,
    now_ms: i64,
    opts: SbsOptions,
) -> Option<String> {
    let kind = msg_type(mm)?;
    let mut s = String::with_capacity(200);
    let _ = write!(
        s,
        "MSG,{kind},1,1,{}{:06X},1,",
        if addr & NON_ICAO_ADDRESS != 0 {
            "~"
        } else {
            ""
        },
        addr & 0xFF_FFFF
    );
    let (y, mo, d, h, mi, se, ms) = utc(mm.sys_timestamp);
    let _ = write!(s, "{y:04}/{mo:02}/{d:02},{h:02}:{mi:02}:{se:02}.{ms:03},");
    let (y, mo, d, h, mi, se, ms) = utc(now_ms);
    let _ = write!(s, "{y:04}/{mo:02}/{d:02},{h:02}:{mi:02}:{se:02}.{ms:03}");

    if mm.callsign_valid {
        let _ = write!(s, ",{}", mm.callsign.as_deref().unwrap_or(""));
    } else {
        s.push(',');
    }

    // altitude
    if opts.use_gnss {
        match (mm.geom_alt, mm.baro_alt, geom_delta) {
            (Some(g), _, _) => {
                let _ = write!(s, ",{g}H");
            }
            (None, Some(b), Some(d)) => {
                let _ = write!(s, ",{}H", b + d);
            }
            (None, Some(b), None) => {
                let _ = write!(s, ",{b}");
            }
            _ => s.push(','),
        }
    } else {
        match (mm.baro_alt, mm.geom_alt, geom_delta) {
            (Some(b), _, _) => {
                let _ = write!(s, ",{b}");
            }
            (None, Some(g), Some(d)) => {
                let _ = write!(s, ",{}", g - d);
            }
            _ => s.push(','),
        }
    }

    match mm.gs {
        Some(g) => {
            let _ = write!(s, ",{:.0}", g.selected);
        }
        None => s.push(','),
    }
    match mm.heading {
        Some(h) if mm.heading_type == HeadingType::GroundTrack => {
            let _ = write!(s, ",{h:.0}");
        }
        _ => s.push(','),
    }
    match (mm.cpr_decoded, mm.decoded_pos) {
        (true, Some((lat, lon))) => {
            let _ = write!(s, ",{lat:1.6},{lon:1.6}");
        }
        _ => s.push_str(",,"),
    }

    // vertical rate
    if opts.use_gnss {
        match (mm.geom_rate, mm.baro_rate) {
            (Some(r), _) => {
                let _ = write!(s, ",{r}H");
            }
            (None, Some(r)) => {
                let _ = write!(s, ",{r}");
            }
            _ => s.push(','),
        }
    } else {
        match (mm.baro_rate, mm.geom_rate) {
            (Some(r), _) | (None, Some(r)) => {
                let _ = write!(s, ",{r}");
            }
            _ => s.push(','),
        }
    }

    match mm.squawk {
        Some(sq) => {
            let _ = write!(s, ",{:04}", sq.dec);
        }
        None => s.push(','),
    }
    // alert, emergency, SPI, on-ground flags: -1 true, 0 false, empty unknown
    match mm.alert {
        Some(true) => s.push_str(",-1"),
        Some(false) => s.push_str(",0"),
        None => s.push(','),
    }
    match mm.squawk {
        Some(sq) if matches!(sq.hex, 0x7500 | 0x7600 | 0x7700) => s.push_str(",-1"),
        Some(_) => s.push_str(",0"),
        None => s.push(','),
    }
    match mm.spi {
        Some(true) => s.push_str(",-1"),
        Some(false) => s.push_str(",0"),
        None => s.push(','),
    }
    match mm.airground {
        AirGround::Ground => s.push_str(",-1"),
        AirGround::Airborne => s.push_str(",0"),
        _ => s.push(','),
    }
    if opts.extra_fields {
        if mm.signal_level > 0.0 {
            let _ = write!(s, ",{:.1}", 10.0 * mm.signal_level.max(1e-10).log10());
        } else {
            s.push(',');
        }
        match mm.category {
            Some(c) => {
                let _ = write!(s, ",{c:02X}");
            }
            None => s.push(','),
        }
    }
    s.push_str("\r\n");
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode_s::Decoder;

    fn decoded(hex: &str, at: i64) -> ModesMessage {
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        let mut mm = ModesMessage::from_bytes(&bytes);
        mm.sys_timestamp = at;
        Decoder::new(1).decode(&mut mm);
        mm
    }

    #[test]
    fn dates() {
        assert_eq!(utc(0), (1970, 1, 1, 0, 0, 0, 0));
        assert_eq!(utc(1_700_000_000_123), (2023, 11, 14, 22, 13, 20, 123));
        assert_eq!(utc(951_782_400_000), (2000, 2, 29, 0, 0, 0, 0)); // leap day
        assert_eq!(utc(-1), (1969, 12, 31, 23, 59, 59, 999));
    }

    #[test]
    fn identification_message() {
        let mm = decoded("8D4840D6202CC371C32CE0576098", 1_700_000_000_000);
        let l = sbs_line(
            &mm,
            0x4840D6,
            None,
            1_700_000_000_500,
            SbsOptions::default(),
        )
        .unwrap();
        assert_eq!(
            l,
            "MSG,1,1,1,4840D6,1,2023/11/14,22:13:20.000,2023/11/14,22:13:20.500,KLM1023 ,,,,,,,,,,,0\r\n"
        );
    }

    #[test]
    fn position_and_velocity_fields() {
        let mut mm = decoded("8D40621D58C382D690C8AC2863A7", 1_700_000_000_000);
        mm.cpr_decoded = true;
        mm.decoded_pos = Some((52.2572, 3.91937));
        let l = sbs_line(
            &mm,
            0x40621D,
            None,
            1_700_000_000_000,
            SbsOptions::default(),
        )
        .unwrap();
        assert!(l.starts_with("MSG,3,1,1,40621D,1,"), "{l}");
        assert!(l.contains(",38000,,,52.257200,3.919370,"), "{l}");
        assert!(l.ends_with("\r\n"));

        let mm = decoded("8D485020994409940838175B284F", 1_700_000_000_000);
        let l = sbs_line(
            &mm,
            0x485020,
            None,
            1_700_000_000_000,
            SbsOptions::default(),
        )
        .unwrap();
        assert!(l.starts_with("MSG,4,1,1,485020,1,"));
        assert!(l.contains(",,,159,183,,,-832,"), "{l}");
    }

    #[test]
    fn skips_messages_without_a_type() {
        let mm = decoded("8D4840D6202CC371C32CE0576098", 1);
        let mut other = mm.clone();
        other.msgtype = 24;
        assert!(sbs_line(&other, 1, None, 1, SbsOptions::default()).is_none());
        let mut tc = mm;
        tc.metype = 28;
        assert!(sbs_line(&tc, 1, None, 1, SbsOptions::default()).is_none());
    }

    #[test]
    fn gnss_option_and_extra_fields() {
        let mut mm = decoded("8D40621D58C382D690C8AC2863A7", 1_000);
        mm.geom_alt = Some(38300);
        let on = SbsOptions {
            use_gnss: true,
            extra_fields: true,
        };
        let l = sbs_line(&mm, 0x40621D, None, 1_000, on).unwrap();
        assert!(l.contains(",38300H,"), "{l}");
        mm.signal_level = 0.1;
        let l = sbs_line(&mm, 0x40621D, Some(300), 1_000, on).unwrap();
        assert!(l.trim_end().ends_with(",-10.0,"), "{l}");
        mm.geom_alt = None;
        let l = sbs_line(&mm, 0x40621D, Some(300), 1_000, on).unwrap();
        assert!(l.contains(",38300H,"), "baro + delta: {l}");
    }

    #[test]
    fn non_icao_addresses_get_a_tilde() {
        let mm = decoded("8D4840D6202CC371C32CE0576098", 1);
        let l = sbs_line(
            &mm,
            0x4840D6 | NON_ICAO_ADDRESS,
            None,
            1,
            SbsOptions::default(),
        )
        .unwrap();
        assert!(l.contains(",~4840D6,"));
    }
}
