//! Comm-B (DF20/DF21) BDS register inference and decoding. Port of upstream `comm_b.c`.
//!
//! A Comm-B reply does not say which register (BDS) it carries, so every decoder scores how
//! plausible the 56 bit MB field looks as its register, and the best unique score wins.

use crate::bits::{getbit, getbits};
use crate::mode_s::{AIS_CHARSET, HeadingType, ModesMessage, NavAltitudeSource, nav_mode};

/// Which register a Comm-B message was recognised as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommBFormat {
    #[default]
    Unknown,
    /// Two or more registers scored equally well.
    Ambiguous,
    EmptyResponse,
    /// BDS 1,0
    DatalinkCaps,
    /// BDS 1,7
    GicbCaps,
    /// BDS 2,0
    AircraftIdent,
    /// BDS 3,0
    AcasRa,
    /// BDS 4,0
    VerticalIntent,
    /// BDS 5,0
    TrackTurn,
    /// BDS 6,0
    HeadingSpeed,
    /// BDS 4,4
    MeteorologicalRoutine,
}

/// Wind from a BDS 4,4 report (or estimated by the tracker).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Wind {
    /// Knots.
    pub speed: f32,
    /// Degrees the wind blows from.
    pub direction: f32,
}

type Decoder = fn(&mut ModesMessage, bool) -> i32;

const DECODERS: [Decoder; 9] = [
    decode_empty,
    decode_bds10,
    decode_bds20,
    decode_bds30,
    decode_bds17,
    decode_bds40,
    decode_bds50,
    decode_bds60,
    decode_bds44,
];

/// Identify and decode the Comm-B payload of `mm` (`mm.mb` must be filled in).
pub fn decode_comm_b(mm: &mut ModesMessage) {
    mm.commb_format = CommBFormat::Unknown;
    // Only trust messages with no downlink request and no corrected bits.
    if mm.dr != 0 || mm.corrected_bits > 0 {
        return;
    }
    let mut best_score = 0;
    let mut best: Option<Decoder> = None;
    let mut ambiguous = false;
    for d in DECODERS {
        let score = d(mm, false);
        if score > best_score {
            best_score = score;
            best = Some(d);
            ambiguous = false;
        } else if score == best_score {
            ambiguous = true;
        }
    }
    if let Some(d) = best {
        if ambiguous {
            mm.commb_format = CommBFormat::Ambiguous;
        } else {
            d(mm, true);
        }
    }
}

fn decode_empty(mm: &mut ModesMessage, store: bool) -> i32 {
    if mm.mb.iter().any(|&b| b != 0) {
        return 0;
    }
    if store {
        mm.commb_format = CommBFormat::EmptyResponse;
    }
    56
}

fn decode_bds10(mm: &mut ModesMessage, store: bool) -> i32 {
    let msg = mm.mb;
    if msg[0] != 0x10 || getbits(&msg, 10, 14) != 0 {
        return 0;
    }
    if store {
        mm.commb_format = CommBFormat::DatalinkCaps;
    }
    56
}

fn decode_bds17(mm: &mut ModesMessage, store: bool) -> i32 {
    let msg = mm.mb;
    if getbits(&msg, 25, 56) != 0 {
        return 0;
    }
    let b = |n| getbit(&msg, n);
    let mut score = 0;
    if b(7) {
        score += 1; // 2,0 aircraft identification
    } else {
        score -= 2;
    }
    // registers a GICB capability report is unlikely to claim: 4,1 4,2 4,3 4,4 (x2), 5,4 5,5 5,6
    for bit in [10, 11, 12, 13, 14, 20, 21, 22] {
        if b(bit) {
            score -= 2;
        }
    }
    if b(1) && b(2) && b(3) && b(4) && b(5) {
        score += 5;
        if b(6) {
            score += 1;
        }
    } else if !b(1) && !b(2) && !b(3) && !b(4) && !b(5) && !b(6) {
        score += 1;
    } else {
        score -= 12;
    }
    if b(16) && b(24) {
        score += 2;
        if b(9) {
            score += 1;
        }
    } else if !b(16) && !b(24) && !b(9) {
        score += 1;
    } else {
        score -= 6;
    }
    if store {
        mm.commb_format = CommBFormat::GicbCaps;
    }
    score
}

fn is_callsign_char(c: u8) -> bool {
    c.is_ascii_uppercase() || (b'-'..=b'9').contains(&c) || c == b' ' || c == b'@'
}

fn decode_bds20(mm: &mut ModesMessage, store: bool) -> i32 {
    let msg = mm.mb;
    if msg[0] != 0x20 {
        return 0;
    }
    let mut callsign = [0u8; 8];
    for (i, c) in callsign.iter_mut().enumerate() {
        let first = 9 + 6 * i as u32;
        *c = AIS_CHARSET[getbits(&msg, first, first + 5) as usize];
    }
    let mut score = 8;
    for &c in &callsign {
        if is_callsign_char(c) {
            score += 6;
        } else {
            return 0;
        }
    }
    if store {
        mm.commb_format = CommBFormat::AircraftIdent;
        mm.callsign = Some(callsign.iter().map(|&c| c as char).collect());
        mm.callsign_valid = true;
    }
    score
}

fn decode_bds30(mm: &mut ModesMessage, store: bool) -> i32 {
    if mm.mb[0] != 0x30 {
        return 0;
    }
    if store {
        mm.commb_format = CommBFormat::AcasRa;
        mm.acas_ra = true;
    }
    56
}

fn decode_bds40(mm: &mut ModesMessage, store: bool) -> i32 {
    let msg = mm.mb;
    let mcp_valid = getbit(&msg, 1);
    let mcp_raw = getbits(&msg, 2, 13);
    let fms_valid = getbit(&msg, 14);
    let fms_raw = getbits(&msg, 15, 26);
    let baro_valid = getbit(&msg, 27);
    let baro_raw = getbits(&msg, 28, 39);
    let reserved_1 = getbits(&msg, 40, 47);
    let mode_valid = getbit(&msg, 48);
    let mode_raw = getbits(&msg, 49, 51);
    let reserved_2 = getbits(&msg, 52, 53);
    let source_valid = getbit(&msg, 54);
    let source_raw = getbits(&msg, 55, 56);

    if !mcp_valid && !fms_valid && !baro_valid && !mode_valid && !source_valid {
        return 0;
    }
    let mut score = 0;

    let mut mcp_alt = 0;
    if mcp_valid && mcp_raw != 0 {
        mcp_alt = mcp_raw * 16;
        if (1000..=50000).contains(&mcp_alt) {
            score += 13;
        } else {
            return 0;
        }
    } else if !mcp_valid && mcp_raw == 0 {
        score += 1;
    } else {
        return 0;
    }

    let mut fms_alt = 0;
    if fms_valid && fms_raw != 0 {
        fms_alt = fms_raw * 16;
        if (1000..=50000).contains(&fms_alt) {
            score += 13;
        } else {
            return 0;
        }
    } else if !fms_valid && fms_raw == 0 {
        score += 1;
    } else {
        return 0;
    }

    let mut baro_setting = 0f32;
    if baro_valid && baro_raw != 0 {
        baro_setting = (800.0 + baro_raw as f64 * 0.1) as f32;
        if (900.0..=1100.0).contains(&baro_setting) {
            score += 13;
        } else {
            return 0;
        }
    } else if !baro_valid && baro_raw == 0 {
        score += 1;
    } else {
        return 0;
    }

    if reserved_1 != 0 {
        return 0;
    }
    if mode_valid {
        score += 4;
    } else if mode_raw == 0 {
        score += 1;
    } else {
        return 0;
    }
    if reserved_2 != 0 {
        return 0;
    }
    if source_valid {
        score += 3;
    } else if source_raw == 0 {
        score += 1;
    } else {
        return 0;
    }

    // MCP and FMS altitudes are usually identical
    if mcp_valid && fms_valid && mcp_alt != fms_alt {
        score -= 4;
    }
    // altitudes are usually multiples of 500 ft (within one LSB)
    if mcp_valid {
        let remainder = mcp_alt % 500;
        if !(16..=484).contains(&remainder) {
            score -= 4;
        }
    }
    if fms_valid {
        let remainder = fms_alt % 500;
        if !(16..=484).contains(&remainder) {
            score -= 4;
        }
    }

    if store {
        mm.commb_format = CommBFormat::VerticalIntent;
        if mcp_valid {
            mm.nav.mcp_altitude = Some(mcp_alt);
        }
        if fms_valid {
            mm.nav.fms_altitude = Some(fms_alt);
        }
        if baro_valid {
            mm.nav.qnh = Some(baro_setting);
        }
        if mode_valid {
            mm.nav.modes = Some(
                (if mode_raw & 4 != 0 { nav_mode::VNAV } else { 0 })
                    | (if mode_raw & 2 != 0 {
                        nav_mode::ALT_HOLD
                    } else {
                        0
                    })
                    | (if mode_raw & 1 != 0 {
                        nav_mode::APPROACH
                    } else {
                        0
                    }),
            );
        }
        mm.nav.altitude_source = if source_valid {
            match source_raw {
                0 => NavAltitudeSource::Unknown,
                1 => NavAltitudeSource::Aircraft,
                2 => NavAltitudeSource::Mcp,
                3 => NavAltitudeSource::Fms,
                _ => NavAltitudeSource::Invalid,
            }
        } else {
            NavAltitudeSource::Invalid
        };
    }
    score
}

fn decode_bds50(mm: &mut ModesMessage, store: bool) -> i32 {
    let msg = mm.mb;
    let roll_valid = getbit(&msg, 1);
    let roll_sign = getbit(&msg, 2);
    let roll_raw = getbits(&msg, 3, 11);
    let track_valid = getbit(&msg, 12);
    let track_sign = getbit(&msg, 13);
    let track_raw = getbits(&msg, 14, 23);
    let gs_valid = getbit(&msg, 24);
    let gs_raw = getbits(&msg, 25, 34);
    let track_rate_valid = getbit(&msg, 35);
    let track_rate_sign = getbit(&msg, 36);
    let track_rate_raw = getbits(&msg, 37, 45);
    let tas_valid = getbit(&msg, 46);
    let tas_raw = getbits(&msg, 47, 56);

    // everything but the track rate must be present
    if !roll_valid || !track_valid || !gs_valid || !tas_valid {
        return 0;
    }
    let mut score = 0;

    let mut roll = (roll_raw as f64 * 45.0 / 256.0) as f32;
    if roll_sign {
        roll = (roll as f64 - 90.0) as f32;
    }
    if (-40.0..40.0).contains(&roll) {
        score += 11;
    } else {
        return 0;
    }

    score += 12;
    let mut track = (track_raw as f64 * 90.0 / 512.0) as f32;
    if track_sign {
        track = (track as f64 + 180.0) as f32;
    }

    // validity was required above, so a zero value is implausible
    let gs = gs_raw * 2;
    if gs_raw == 0 || !(50..=700).contains(&gs) {
        return 0;
    }
    score += 11;

    let mut track_rate = 0f32;
    if track_rate_valid {
        track_rate = (track_rate_raw as f64 * 8.0 / 256.0) as f32;
        if track_rate_sign {
            track_rate = (track_rate as f64 - 16.0) as f32;
        }
        if (-10.0..=10.0).contains(&track_rate) {
            score += 11;
        } else {
            return 0;
        }
    } else if track_rate_raw == 0 && !track_rate_sign {
        score += 1;
    } else {
        return 0;
    }

    let tas = tas_raw * 2;
    if tas_raw == 0 || !(50..=700).contains(&tas) {
        return 0;
    }
    score += 11;

    // ground speed and true airspeed should be within 150 kt
    if (gs as i32 - tas as i32).abs() > 150 {
        score -= 6;
    }
    // the roll angle should explain the track rate
    if tas > 0 && track_rate_valid {
        let turn_rate = 68625.0 * (roll as f64 * std::f64::consts::PI / 180.0).tan()
            / (tas as f64 * 20.0 * std::f64::consts::PI);
        if (turn_rate - track_rate as f64).abs() > 2.0 {
            score -= 6;
        }
    }

    if store {
        mm.commb_format = CommBFormat::TrackTurn;
        mm.roll = Some(roll);
        mm.heading = Some(track);
        mm.heading_type = HeadingType::GroundTrack;
        let g = gs as f32;
        mm.gs = Some(crate::mode_s::GroundSpeed {
            v0: g,
            v2: g,
            selected: g,
        });
        if track_rate_valid {
            mm.track_rate = Some(track_rate);
        }
        mm.tas = Some(tas);
    }
    score
}

fn decode_bds60(mm: &mut ModesMessage, store: bool) -> i32 {
    let msg = mm.mb;
    let heading_valid = getbit(&msg, 1);
    let heading_sign = getbit(&msg, 2);
    let heading_raw = getbits(&msg, 3, 12);
    let ias_valid = getbit(&msg, 13);
    let ias_raw = getbits(&msg, 14, 23);
    let mach_valid = getbit(&msg, 24);
    let mach_raw = getbits(&msg, 25, 34);
    let baro_rate_valid = getbit(&msg, 35);
    let baro_rate_sign = getbit(&msg, 36);
    let baro_rate_raw = getbits(&msg, 37, 45);
    let inertial_rate_valid = getbit(&msg, 46);
    let inertial_rate_sign = getbit(&msg, 47);
    let inertial_rate_raw = getbits(&msg, 48, 56);

    // heading, IAS and Mach must be present, plus at least one vertical rate
    if !heading_valid || !ias_valid || !mach_valid || (!baro_rate_valid && !inertial_rate_valid) {
        return 0;
    }
    let mut score = 12;
    let mut heading = (heading_raw as f64 * 90.0 / 512.0) as f32;
    if heading_sign {
        heading = (heading as f64 + 180.0) as f32;
    }

    let ias = ias_raw;
    if ias_raw != 0 && (50..=700).contains(&ias) {
        score += 11;
    } else {
        return 0;
    }

    let mach = (mach_raw as f64 * 2.048 / 512.0) as f32;
    if mach_raw != 0 && (0.1..=0.9).contains(&mach) {
        score += 11;
    } else {
        return 0;
    }

    let mut baro_rate = 0i32;
    if baro_rate_valid {
        baro_rate = baro_rate_raw as i32 * 32;
        if baro_rate_sign {
            baro_rate -= 16384;
        }
        if (-6000..=6000).contains(&baro_rate) {
            score += 11;
        } else {
            return 0;
        }
    } else if baro_rate_raw == 0 {
        score += 1;
    } else {
        return 0;
    }

    let mut inertial_rate = 0i32;
    if inertial_rate_valid {
        inertial_rate = inertial_rate_raw as i32 * 32;
        if inertial_rate_sign {
            inertial_rate -= 16384;
        }
        if (-6000..=6000).contains(&inertial_rate) {
            score += 11;
        } else {
            return 0;
        }
    } else if inertial_rate_raw == 0 {
        score += 1;
    } else {
        return 0;
    }

    // IAS vs Mach consistency at the reported pressure altitude (standard atmosphere)
    if let Some(alt) = mm.baro_alt
        && alt > -1500
        && alt < 60000
    {
        let h_m = alt as f64 * 0.3048;
        let p_ratio = if h_m <= 11000.0 {
            (1.0 - 2.25577e-5 * h_m).powf(5.25588)
        } else {
            0.22336 * (-(h_m - 11000.0) / 6341.62).exp()
        };
        let m = mach as f64;
        let qc_p0 = p_ratio * ((1.0 + 0.2 * m * m).powf(3.5) - 1.0);
        let cas = 661.47 * (5.0 * ((qc_p0 + 1.0).powf(2.0 / 7.0) - 1.0)).sqrt();
        if (cas - ias as f64).abs() > 25.0 {
            score -= 12;
        }
    }
    // barometric and inertial rates should roughly agree
    if baro_rate_valid && inertial_rate_valid && (baro_rate - inertial_rate).abs() > 2000 {
        score -= 12;
    }

    if store {
        mm.commb_format = CommBFormat::HeadingSpeed;
        mm.heading = Some(heading);
        mm.heading_type = HeadingType::Magnetic;
        mm.ias = Some(ias);
        mm.mach = Some(mach as f64);
        if baro_rate_valid {
            mm.baro_rate = Some(baro_rate);
        }
        if inertial_rate_valid {
            mm.geom_rate = Some(inertial_rate);
        }
    }
    score
}

fn decode_bds44(mm: &mut ModesMessage, store: bool) -> i32 {
    let msg = mm.mb;
    let source = getbits(&msg, 1, 4);
    let wind_valid = getbit(&msg, 5);
    let wind_speed_raw = getbits(&msg, 6, 14);
    let wind_direction_raw = getbits(&msg, 15, 23);
    let temperature_sign = getbit(&msg, 24);
    let temperature_raw = getbits(&msg, 25, 34);
    let pressure_valid = getbit(&msg, 35);
    let pressure_raw = getbits(&msg, 36, 46);
    let turbulence_valid = getbit(&msg, 47);
    let turbulence_raw = getbits(&msg, 48, 49);
    let humidity_valid = getbit(&msg, 50);
    let humidity_raw = getbits(&msg, 51, 56);

    let mut score = 0;
    if source <= 6 {
        score += 4;
    } else {
        return 0;
    }

    let mut wind_speed = 0i32;
    let mut wind_direction = 0f32;
    if wind_valid {
        wind_speed = wind_speed_raw as i32;
        score += 9;
        wind_direction = wind_direction_raw as f32 * (180.0f32 / 256.0);
        if (0.0..=360.0).contains(&wind_direction) {
            score += 9;
        } else {
            return 0;
        }
    } else if wind_speed_raw == 0 && wind_direction_raw == 0 {
        score += 2;
    } else {
        return 0;
    }

    let temperature = if temperature_sign {
        ((temperature_raw as f64 - 1024.0) * 0.25) as f32
    } else {
        (temperature_raw as f64 * 0.25) as f32
    };
    if (-128.0..=128.0).contains(&temperature) {
        score += 10;
    } else {
        return 0;
    }

    let mut static_pressure = 0;
    if pressure_valid {
        static_pressure = pressure_raw as i32;
        if (0..=2048).contains(&static_pressure) {
            score += 11;
        } else {
            return 0;
        }
    } else if pressure_raw == 0 {
        score += 1;
    } else {
        return 0;
    }

    let mut turbulence = 0;
    if turbulence_valid {
        turbulence = turbulence_raw as i32;
        score += 2;
    } else if turbulence_raw == 0 {
        score += 1;
    } else {
        return 0;
    }

    let mut humidity = 0f32;
    if humidity_valid {
        humidity = humidity_raw as f32 * (100.0f32 / 64.0);
        if (0.0..=100.0).contains(&humidity) {
            score += 6;
        } else {
            return 0;
        }
    } else if humidity_raw == 0 {
        score += 1;
    } else {
        return 0;
    }

    if store {
        mm.commb_format = CommBFormat::MeteorologicalRoutine;
        mm.met_source = Some(source as i32);
        if wind_valid && !(wind_speed == 0 && wind_direction == 0.0) {
            mm.wind = Some(Wind {
                speed: wind_speed as f32,
                direction: wind_direction,
            });
        }
        mm.oat = Some(temperature);
        if pressure_valid {
            mm.static_pressure = Some(static_pressure);
        }
        if turbulence_valid {
            mm.turbulence = Some(turbulence);
        }
        if humidity_valid {
            mm.humidity = Some(humidity);
        }
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    /// A DF20 message whose MB field is `mb`.
    fn with_mb(mb: &str) -> ModesMessage {
        let mut mm = ModesMessage::default();
        mm.mb.copy_from_slice(&hex(mb));
        mm
    }

    // Reference messages from the pyModeS test suite (MB field = bytes 5..=11 of the frame).
    // A000139381951536E024D4CCF6B5 -> BDS 5,0
    // A000029CFFBAA11E2004727281F1 -> BDS 6,0
    // A000029C85E42F313000007047D3 -> BDS 4,0
    // A0001838201584F23468207CDFA5 -> BDS 2,0 (callsign JAL...)

    #[test]
    fn bds50_track_and_turn() {
        let mut mm = with_mb("81951536E024D4");
        decode_comm_b(&mut mm);
        assert_eq!(mm.commb_format, CommBFormat::TrackTurn);
        assert!((mm.roll.unwrap() - 2.1).abs() < 0.05, "{:?}", mm.roll);
        assert!(
            (mm.heading.unwrap() - 114.258).abs() < 0.001,
            "{:?}",
            mm.heading
        );
        assert_eq!(mm.heading_type, HeadingType::GroundTrack);
        assert_eq!(mm.gs.unwrap().selected, 438.0);
        assert!((mm.track_rate.unwrap() - 0.125).abs() < 1e-6);
        assert_eq!(mm.tas, Some(424));
    }

    /// Pack MB fields MSB first.
    fn pack(fields: &[(u32, u32)]) -> ModesMessage {
        let mut mm = ModesMessage::default();
        let mut n = 0;
        for &(value, width) in fields {
            for i in (0..width).rev() {
                if (value >> i) & 1 == 1 {
                    mm.mb[n / 8] |= 0x80 >> (n % 8);
                }
                n += 1;
            }
        }
        assert_eq!(n, 56);
        mm
    }

    /// BDS 6,0: heading 120.06, IAS 265, Mach 0.78, baro rate -64, inertial rate -96.
    fn bds60_payload() -> ModesMessage {
        pack(&[
            (1, 1),
            (0, 1),
            (683, 10), // heading
            (1, 1),
            (265, 10), // IAS
            (1, 1),
            (195, 10), // Mach
            (1, 1),
            (1, 1),
            (510, 9), // baro rate
            (1, 1),
            (1, 1),
            (509, 9), // inertial rate
        ])
    }

    #[test]
    fn bds60_heading_and_speed() {
        let mut mm = bds60_payload();
        mm.baro_alt = Some(35000);
        decode_comm_b(&mut mm);
        assert_eq!(mm.commb_format, CommBFormat::HeadingSpeed);
        assert!(
            (mm.heading.unwrap() - 120.0586).abs() < 0.001,
            "{:?}",
            mm.heading
        );
        assert_eq!(mm.heading_type, HeadingType::Magnetic);
        assert_eq!(mm.ias, Some(265));
        assert!((mm.mach.unwrap() - 0.78).abs() < 1e-6, "{:?}", mm.mach);
        assert_eq!(mm.baro_rate, Some(-64));
        assert_eq!(mm.geom_rate, Some(-96));
    }

    #[test]
    fn bds60_ias_mach_consistency_check_uses_the_altitude() {
        // Mach 0.78 at 35000 ft is about 265 kt CAS: consistent. At sea level the same
        // Mach would be far faster than 265 kt, so the score drops by 12.
        let mut high = bds60_payload();
        high.baro_alt = Some(35000);
        let consistent = decode_bds60(&mut high, false);
        let mut none = bds60_payload();
        let no_altitude = decode_bds60(&mut none, false);
        let mut low = bds60_payload();
        low.baro_alt = Some(0);
        let sea_level = decode_bds60(&mut low, false);
        assert_eq!(consistent, no_altitude);
        assert_eq!(sea_level, no_altitude - 12);
    }

    #[test]
    fn ambiguous_payload_follows_the_scores() {
        // pyModeS labels this one BDS 6,0, but under upstream's scoring BDS 5,0 wins.
        let mut mm = with_mb("FFBAA11E200472");
        assert!(decode_bds50(&mut mm, false) > decode_bds60(&mut mm, false));
        decode_comm_b(&mut mm);
        assert_eq!(mm.commb_format, CommBFormat::TrackTurn);
    }

    #[test]
    fn bds40_vertical_intent() {
        let mut mm = with_mb("85E42F31300000");
        decode_comm_b(&mut mm);
        assert_eq!(mm.commb_format, CommBFormat::VerticalIntent);
        assert_eq!(mm.nav.mcp_altitude, Some(3008));
        assert_eq!(mm.nav.fms_altitude, Some(3008));
        assert!(
            (mm.nav.qnh.unwrap() - 1020.0).abs() < 0.01,
            "{:?}",
            mm.nav.qnh
        );
    }

    #[test]
    fn bds20_identification() {
        // MB: 0x20 then eight 6-bit characters "KLM1023 "
        let mut mm = ModesMessage::default();
        let mut bits: Vec<bool> = Vec::new();
        let mut put = |v: u32, w: u32| {
            for i in (0..w).rev() {
                bits.push((v >> i) & 1 == 1);
            }
        };
        put(0x20, 8);
        for c in "KLM1023 ".chars() {
            put(
                AIS_CHARSET.iter().position(|&x| x as char == c).unwrap() as u32,
                6,
            );
        }
        for (i, b) in bits.iter().enumerate() {
            if *b {
                mm.mb[i / 8] |= 0x80 >> (i % 8);
            }
        }
        decode_comm_b(&mut mm);
        assert_eq!(mm.commb_format, CommBFormat::AircraftIdent);
        assert_eq!(mm.callsign.as_deref(), Some("KLM1023 "));
        assert!(mm.callsign_valid);
    }

    #[test]
    fn empty_and_unknown_payloads() {
        let mut mm = with_mb("00000000000000");
        decode_comm_b(&mut mm);
        assert_eq!(mm.commb_format, CommBFormat::EmptyResponse);
        let mut mm = with_mb("FFFFFFFFFFFFFF");
        decode_comm_b(&mut mm);
        assert_eq!(mm.commb_format, CommBFormat::Unknown);
        assert!(mm.roll.is_none() && mm.ias.is_none());
    }

    #[test]
    fn corrected_or_downlink_request_messages_are_not_decoded() {
        let mut mm = with_mb("81951536E024D4");
        mm.corrected_bits = 1;
        decode_comm_b(&mut mm);
        assert_eq!(mm.commb_format, CommBFormat::Unknown);
        let mut mm = with_mb("81951536E024D4");
        mm.dr = 3;
        decode_comm_b(&mut mm);
        assert_eq!(mm.commb_format, CommBFormat::Unknown);
        assert!(mm.roll.is_none());
    }

    #[test]
    fn bds44_meteorological_report() {
        // source 5 (AMDAR-ish), wind 40 kt from 90 deg, -50.0 C, 300 hPa-ish pressure
        let mut bits: Vec<bool> = Vec::new();
        let mut put = |v: u32, w: u32| {
            for i in (0..w).rev() {
                bits.push((v >> i) & 1 == 1);
            }
        };
        put(5, 4);
        put(1, 1);
        put(40, 9);
        put(128, 9); // 128 * 180/256 = 90 degrees
        put(1, 1); // negative
        put(1024 - 200, 10); // -50.0 C
        put(1, 1);
        put(300, 11);
        put(0, 1);
        put(0, 2);
        put(0, 1);
        put(0, 6);
        let mut mm = ModesMessage::default();
        for (i, b) in bits.iter().enumerate() {
            if *b {
                mm.mb[i / 8] |= 0x80 >> (i % 8);
            }
        }
        decode_comm_b(&mut mm);
        assert_eq!(mm.commb_format, CommBFormat::MeteorologicalRoutine);
        assert_eq!(
            mm.wind,
            Some(Wind {
                speed: 40.0,
                direction: 90.0
            })
        );
        assert_eq!(mm.oat, Some(-50.0));
        assert_eq!(mm.static_pressure, Some(300));
        assert_eq!(mm.met_source, Some(5));
    }
}
