//! Mode S message scoring and decoding. Port of upstream `mode_s.c` / `mode_s.h`.
//!
//! Not yet ported: Comm-B BDS inference (`comm_b.c`), TIS-B/ADS-R refinements beyond the
//! address handling, MLAT/network-source flags. Those arrive with the network inputs.

use crate::bits::{getbit, getbits};
use crate::comm_b::{CommBFormat, Wind, decode_comm_b};
use crate::crc::{self, ErrorInfo, Fixer};
use crate::icao::IcaoFilter;
use crate::mode_ac::{mode_a_to_mode_c, squawk_hex_to_dec};

pub const LONG_MSG_BYTES: usize = 14;
pub const SHORT_MSG_BYTES: usize = 7;
pub const LONG_MSG_BITS: usize = 112;
pub const SHORT_MSG_BITS: usize = 56;
/// Set on addresses to indicate they are not ICAO addresses.
pub const NON_ICAO_ADDRESS: u32 = 1 << 24;
/// Placeholder for "no address".
pub const HEX_UNKNOWN: u32 = 0xDEAD_BEEF;
/// `msgtype` used for Mode A/C replies (valid Mode S DFs are 0..=31).
pub const DFTYPE_MODEAC: u8 = 77;

/// AIS 6-bit character set used for callsigns.
pub const AIS_CHARSET: &[u8; 64] =
    b"@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_ !\"#$%&'()*+,-./0123456789:;<=>?";

/// Number of bits in a message of downlink format `df`.
pub fn msg_len_by_type(df: u8) -> usize {
    if df & 0x10 != 0 {
        LONG_MSG_BITS
    } else {
        SHORT_MSG_BITS
    }
}

/// Where an address came from. Matches upstream `addrtype_t` ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum AddrType {
    AdsbIcao = 0,
    AdsbIcaoNt = 1,
    AdsrIcao = 2,
    TisbIcao = 3,
    Jaero = 4,
    Mlat = 5,
    Other = 6,
    ModeS = 7,
    AdsbOther = 8,
    AdsrOther = 9,
    TisbTrackfile = 10,
    TisbOther = 11,
    ModeA = 12,
    #[default]
    Unknown = 13,
}

/// Quality of a data source; higher is better. Matches upstream `datasource_t` ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum DataSource {
    #[default]
    Invalid,
    Indirect,
    ModeAc,
    Sbs,
    Mlat,
    ModeS,
    Jaero,
    ModeSChecked,
    Tisb,
    Adsr,
    Nt,
    Adsb,
    Prio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AirGround {
    #[default]
    Invalid,
    Ground,
    Airborne,
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeadingType {
    #[default]
    Invalid,
    GroundTrack,
    True,
    Magnetic,
    MagneticOrTrue,
    TrackOrHeading,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CprType {
    #[default]
    Invalid,
    Surface,
    Airborne,
    Coarse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SilType {
    #[default]
    Invalid,
    Unknown,
    PerSample,
    PerHour,
}

/// ES type 28/1 emergency/priority status subfield.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emergency {
    None = 0,
    General = 1,
    Lifeguard = 2,
    MinFuel = 3,
    NoRadio = 4,
    Unlawful = 5,
    Downed = 6,
    Reserved = 7,
}

impl Emergency {
    fn from_bits(v: u32) -> Self {
        match v & 7 {
            0 => Self::None,
            1 => Self::General,
            2 => Self::Lifeguard,
            3 => Self::MinFuel,
            4 => Self::NoRadio,
            5 => Self::Unlawful,
            6 => Self::Downed,
            _ => Self::Reserved,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NavAltitudeSource {
    #[default]
    Invalid,
    Unknown,
    Aircraft,
    Mcp,
    Fms,
}

/// Autopilot mode flags (bit set).
pub mod nav_mode {
    pub const AUTOPILOT: u8 = 1;
    pub const VNAV: u8 = 2;
    pub const ALT_HOLD: u8 = 4;
    pub const APPROACH: u8 = 8;
    pub const LNAV: u8 = 16;
    pub const TCAS: u8 = 32;
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Squawk {
    /// 13 identity bits as four hex digits (each an octal digit of the squawk).
    pub hex: u32,
    /// Squawk as a decimal number, e.g. 7700.
    pub dec: u32,
}

impl Squawk {
    pub fn from_hex(hex: u32) -> Self {
        Self {
            hex,
            dec: squawk_hex_to_dec(hex),
        }
    }
}

/// Ground speed in knots. Surface movement has different interpretations for
/// ADS-B v0 and v2; both are filled in and `selected` starts as the v0 value.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GroundSpeed {
    pub v0: f32,
    pub v2: f32,
    pub selected: f32,
}

/// Raw CPR position fields of a position message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CprFields {
    pub kind: CprType,
    pub odd: bool,
    pub lat: u32,
    pub lon: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Accuracy {
    pub nic_a: Option<bool>,
    pub nic_b: Option<bool>,
    pub nic_c: Option<bool>,
    pub nic_baro: Option<bool>,
    pub nac_p: Option<u8>,
    pub nac_v: Option<u8>,
    pub gva: Option<u8>,
    pub sda: Option<u8>,
    pub sil: u8,
    pub sil_type: SilType,
}

/// Aircraft operational status (ES type 31).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpStatus {
    pub valid: bool,
    pub version: u8,
    pub sil_type: SilType,
    pub tah: HeadingType,
    pub hrd: HeadingType,
    pub cc_lw: u8,
    pub cc_antenna_offset: u8,
    pub cc_lw_valid: bool,
    pub om_acas_ra: bool,
    pub om_ident: bool,
    pub om_atc: bool,
    pub om_saf: bool,
    pub cc_acas: bool,
    pub cc_cdti: bool,
    pub cc_1090_in: bool,
    pub cc_arv: bool,
    pub cc_ts: bool,
    pub cc_tc: u8,
    pub cc_uat_in: bool,
    pub cc_poa: bool,
    pub cc_b2_low: bool,
}

/// Selected/target state: ES type 29 (target state and status).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Nav {
    pub fms_altitude: Option<u32>,
    pub mcp_altitude: Option<u32>,
    pub qnh: Option<f32>,
    pub heading: Option<f32>,
    pub heading_type: HeadingType,
    pub modes: Option<u8>,
    pub altitude_source: NavAltitudeSource,
}

/// A decoded Mode S (or Mode A/C) message.
#[derive(Debug, Clone, PartialEq)]
pub struct ModesMessage {
    /// Binary message (long messages use all 14 bytes, short ones the first 7).
    pub msg: [u8; LONG_MSG_BYTES],
    /// The message as received, before any error correction.
    pub verbatim: [u8; LONG_MSG_BYTES],
    /// RSSI as a fraction of full-scale power, 0..1.
    pub signal_level: f64,
    /// 12 MHz receiver clock timestamp.
    pub timestamp: i64,
    /// Estimated system time of reception, milliseconds since the Unix epoch.
    pub sys_timestamp: i64,
    /// Receiver id for network sources, 0 if not transmitted.
    pub receiver_id: u64,
    /// Downlink format (0-31), or `DFTYPE_MODEAC`.
    pub msgtype: u8,
    pub msgbits: usize,
    /// Score from `Decoder::score`, if used.
    pub score: i32,
    pub corrected_bits: u8,
    /// 0 = ok, -1 = maybe valid but CRC/ICAO not verified, -2 = bad or unrepairable.
    pub decode_result: i32,
    pub crc: u32,
    pub addr: u32,
    pub maybe_addr: u32,
    pub addrtype: AddrType,
    pub source: DataSource,
    pub remote: bool,

    // Raw fields.
    pub iid: u32,
    pub aa: u32,
    pub ac: u32,
    pub ca: u32,
    pub cc: u32,
    pub cf: u32,
    pub dr: u32,
    pub fs: u32,
    pub id: u32,
    pub ke: u32,
    pub nd: u32,
    pub ri: u32,
    pub sl: u32,
    pub um: u32,
    pub vs: u32,
    pub metype: u32,
    pub mesub: u32,
    pub mb: [u8; 7],
    pub md: [u8; 10],
    pub me: [u8; 7],
    pub mv: [u8; 7],

    // Decoded data.
    /// Barometric altitude in feet.
    pub baro_alt: Option<i32>,
    pub alt_q_bit: bool,
    /// Geometric altitude in feet.
    pub geom_alt: Option<i32>,
    /// Geometric minus barometric altitude, feet.
    pub geom_delta: Option<i32>,
    pub heading: Option<f32>,
    pub heading_type: HeadingType,
    pub gs: Option<GroundSpeed>,
    pub ias: Option<u32>,
    pub tas: Option<u32>,
    /// Mach number (from BDS 6,0).
    pub mach: Option<f64>,
    /// Roll angle in degrees, right positive (BDS 5,0).
    pub roll: Option<f32>,
    /// Rate of change of track in degrees per second (BDS 5,0).
    pub track_rate: Option<f32>,
    /// Barometric rate of climb, ft/min.
    pub baro_rate: Option<i32>,
    /// Geometric rate of climb, ft/min.
    pub geom_rate: Option<i32>,
    pub callsign: Option<String>,
    /// All callsign characters were legal.
    pub callsign_valid: bool,
    pub squawk: Option<Squawk>,
    /// Emitter category: A0..D7 as a single byte (0xA0 | n etc.).
    pub category: Option<u8>,
    pub emergency: Option<Emergency>,
    pub cpr: Option<CprFields>,
    pub airground: AirGround,
    pub alert: Option<bool>,
    pub spi: Option<bool>,
    pub acas_ra: bool,
    /// Register a Comm-B message was recognised as.
    pub commb_format: CommBFormat,
    /// Meteorological data from BDS 4,4, or a wind estimate made by the tracker.
    pub wind: Option<Wind>,
    pub oat: Option<f32>,
    pub static_pressure: Option<i32>,
    pub turbulence: Option<i32>,
    pub humidity: Option<f32>,
    pub met_source: Option<i32>,
    pub accuracy: Accuracy,
    pub opstatus: OpStatus,
    pub nav: Nav,
    /// A position message the decoder filtered out as a known transponder failure.
    pub cpr_filtered: bool,

    // Filled in by the tracker.
    pub address_reliable: bool,
    pub duplicate: bool,
    pub pos_bad: bool,
    pub pos_ignore: bool,
    pub pos_old: bool,
    pub in_disc_cache: bool,
    pub reduce_forward: bool,
    /// Decoded position (lat, lon) if a CPR decode succeeded.
    pub decoded_pos: Option<(f64, f64)>,
    pub decoded_nic: u32,
    pub decoded_rc: u32,
    pub cpr_relative: bool,
    /// The tracker decoded a position for this message (`decoded_pos` is valid and good).
    pub cpr_decoded: bool,
    /// Set by the tracker when this message gave a position accurate enough for the range
    /// outline (`outline.json`).
    pub outline_position: bool,
    pub distance_traveled: f64,
    pub receiver_distance: f64,
    pub calculated_track: f32,
    pub speed_unreliable: i32,
    pub track_unreliable: i32,
}

impl Default for ModesMessage {
    fn default() -> Self {
        Self {
            msg: [0; LONG_MSG_BYTES],
            verbatim: [0; LONG_MSG_BYTES],
            signal_level: 0.0,
            timestamp: 0,
            sys_timestamp: 0,
            receiver_id: 0,
            msgtype: 0,
            msgbits: 0,
            score: 0,
            corrected_bits: 0,
            decode_result: 0,
            crc: 0,
            addr: HEX_UNKNOWN,
            maybe_addr: HEX_UNKNOWN,
            addrtype: AddrType::Unknown,
            source: DataSource::Invalid,
            remote: false,
            iid: 0,
            aa: 0,
            ac: 0,
            ca: 0,
            cc: 0,
            cf: 0,
            dr: 0,
            fs: 0,
            id: 0,
            ke: 0,
            nd: 0,
            ri: 0,
            sl: 0,
            um: 0,
            vs: 0,
            metype: 0,
            mesub: 0,
            mb: [0; 7],
            md: [0; 10],
            me: [0; 7],
            mv: [0; 7],
            baro_alt: None,
            alt_q_bit: false,
            geom_alt: None,
            geom_delta: None,
            heading: None,
            heading_type: HeadingType::Invalid,
            gs: None,
            ias: None,
            tas: None,
            mach: None,
            roll: None,
            track_rate: None,
            baro_rate: None,
            geom_rate: None,
            callsign: None,
            callsign_valid: false,
            squawk: None,
            category: None,
            emergency: None,
            cpr: None,
            airground: AirGround::Invalid,
            alert: None,
            spi: None,
            acas_ra: false,
            commb_format: CommBFormat::Unknown,
            wind: None,
            oat: None,
            static_pressure: None,
            turbulence: None,
            humidity: None,
            met_source: None,
            accuracy: Accuracy::default(),
            opstatus: OpStatus::default(),
            nav: Nav::default(),
            cpr_filtered: false,
            address_reliable: false,
            duplicate: false,
            pos_bad: false,
            pos_ignore: false,
            pos_old: false,
            in_disc_cache: false,
            reduce_forward: false,
            decoded_pos: None,
            decoded_nic: 0,
            decoded_rc: 0,
            cpr_relative: false,
            cpr_decoded: false,
            outline_position: false,
            distance_traveled: 0.0,
            receiver_distance: 0.0,
            calculated_track: -1.0,
            speed_unreliable: 0,
            track_unreliable: 0,
        }
    }
}

impl ModesMessage {
    /// Build a message from raw bytes (7 or 14 are used, extra bytes ignored).
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut m = Self::default();
        let n = bytes.len().min(LONG_MSG_BYTES);
        m.msg[..n].copy_from_slice(&bytes[..n]);
        m.verbatim = m.msg;
        m
    }

    /// The message bytes actually used (`msgbits / 8`).
    pub fn bytes(&self) -> &[u8] {
        &self.msg[..self.msgbits / 8]
    }

    /// Hex string of the message as received.
    pub fn hex(&self) -> String {
        self.msg[..self.msgbits / 8]
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect()
    }

    /// Fabricate a message from a Mode A/C reply (`mode_a` is hex-encoded Mode A).
    pub fn from_mode_a(mode_a: u32) -> Self {
        let mut mm = Self {
            source: DataSource::ModeAc,
            addrtype: AddrType::ModeA,
            msgtype: DFTYPE_MODEAC,
            msgbits: 16,
            ..Self::default()
        };
        mm.msg[0] = (mode_a >> 8) as u8;
        mm.msg[1] = mode_a as u8;
        mm.verbatim = mm.msg;
        // Fudge an address based on Mode A (remove the Ident bit).
        mm.addr = (mode_a & 0x0000_FF7F) | NON_ICAO_ADDRESS;
        mm.squawk = Some(Squawk::from_hex(mode_a & 0x7777));
        let spi = mode_a & 0x0080 != 0;
        mm.spi = Some(spi);
        if !spi && let Some(c) = mode_a_to_mode_c(mode_a) {
            mm.baro_alt = Some(c * 100);
        }
        mm
    }
}

// ---------------------------------------------------------------------------
// Field decoders
// ---------------------------------------------------------------------------

/// Gillham-coded 13 bit identity field to a hex-encoded Mode A value.
/// Bits: C1 A1 C2 A2 C4 A4 (X/M) B1 D1 B2 D2 B4 D4, MSB first.
pub fn decode_id13(id13: u32) -> u32 {
    const MAP: [(u32, u32); 12] = [
        (0x1000, 0x0010), // C1
        (0x0800, 0x1000), // A1
        (0x0400, 0x0020), // C2
        (0x0200, 0x2000), // A2
        (0x0100, 0x0040), // C4
        (0x0080, 0x4000), // A4
        (0x0020, 0x0100), // B1
        (0x0010, 0x0001), // D1 or Q
        (0x0008, 0x0200), // B2
        (0x0004, 0x0002), // D2
        (0x0002, 0x0400), // B4
        (0x0001, 0x0004), // D4
    ];
    MAP.iter()
        .filter(|(src, _)| id13 & src != 0)
        .fold(0, |acc, (_, dst)| acc | dst)
}

/// 13 bit AC altitude field (DF0/4/16/20), feet. Meters are not supported (upstream TODO).
fn decode_ac13(ac13: u32) -> Option<(i32, bool)> {
    let m_bit = ac13 & 0x0040 != 0;
    let q_bit = ac13 & 0x0010 != 0;
    if m_bit {
        return None;
    }
    if q_bit {
        // 11 bit integer after removing Q and M; altitude is n * 25 - 1000.
        let n = ((ac13 & 0x1F80) >> 2) | ((ac13 & 0x0020) >> 1) | (ac13 & 0x000F);
        Some((n as i32 * 25 - 1000, true))
    } else {
        let n = mode_a_to_mode_c(decode_id13(ac13))?;
        (n >= -12).then_some((100 * n, false))
    }
}

/// 12 bit AC altitude field (DF17/18), feet.
fn decode_ac12(ac12: u32) -> Option<(i32, bool)> {
    if ac12 & 0x10 != 0 {
        let n = ((ac12 & 0x0FE0) >> 1) | (ac12 & 0x000F);
        Some((n as i32 * 25 - 1000, true))
    } else {
        // Insert M=0 at bit 6 to make a 13 bit Gillham field.
        let n13 = ((ac12 & 0x0FC0) << 1) | (ac12 & 0x003F);
        let n = mode_a_to_mode_c(decode_id13(n13))?;
        (n >= -12).then_some((100 * n, false))
    }
}

fn movement_v2(m: u32) -> f32 {
    let f = m as f32;
    match m {
        125.. => 0.0,
        124 => 180.0,
        109..=123 => 100.0 + (f - 109.0 + 0.5) * 5.0,
        94..=108 => 70.0 + (f - 94.0 + 0.5) * 2.0,
        39..=93 => 15.0 + (f - 39.0 + 0.5),
        13..=38 => 2.0 + (f - 13.0 + 0.5) * 0.50,
        9..=12 => 1.0 + (f - 9.0 + 0.5) * 0.25,
        3..=8 => 0.125 + (f - 3.0 + 0.5) * 0.875 / 6.0,
        2 => 0.125 / 2.0,
        _ => 0.0,
    }
}

fn movement_v0(m: u32) -> f32 {
    let f = m as f32;
    match m {
        125.. => 0.0,
        124 => 180.0,
        109..=123 => 100.0 + (f - 109.0 + 0.5) * 5.0,
        94..=108 => 70.0 + (f - 94.0 + 0.5) * 2.0,
        39..=93 => 15.0 + (f - 39.0 + 0.5),
        13..=38 => 2.0 + (f - 13.0 + 0.5) * 0.50,
        9..=12 => 1.0 + (f - 9.0 + 0.5) * 0.25,
        2..=8 => 0.125 + (f - 2.0 + 0.5) * 0.125,
        _ => 0.0,
    }
}

/// Apply an error syndrome to the AA field (bits 8..=31); returns number of address bits touched.
fn correct_aa_field(addr: &mut u32, ei: &ErrorInfo) -> u32 {
    let mut n = 0;
    for &b in &ei.bit[..ei.errors as usize] {
        if (8..=31).contains(&b) {
            *addr ^= 1 << (31 - b);
            n += 1;
        }
    }
    n
}

/// DF values that could be a DF17 with a single bit error in the DF field.
fn fix_df17_candidate(msgtype: u8) -> bool {
    matches!(msgtype, 1 | 25 | 21 | 19 | 16)
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Decoder configuration and state (CRC tables, recently seen ICAO addresses).
#[derive(Debug, Clone)]
pub struct Decoder {
    pub icao: IcaoFilter,
    fixer: Fixer,
    /// Bit errors to correct: 0, 1 or 2 (`--no-fix`, default, `--fix-2bit`/`--aggressive`).
    nfix_crc: u8,
    /// Fix single-bit errors in the DF field of would-be DF17 messages (default on).
    pub fix_df: bool,
    /// Keep decoding after a CRC/ICAO failure (`--decode-all` style debugging).
    pub decode_all: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Decoder {
    pub fn new(nfix_crc: u8) -> Self {
        Self {
            icao: IcaoFilter::new(),
            fixer: Fixer::new(nfix_crc),
            nfix_crc,
            fix_df: true,
            decode_all: false,
        }
    }

    pub fn nfix_crc(&self) -> u8 {
        self.nfix_crc
    }

    /// If `msg[0]` is a single-bit-flipped DF17, return the repaired first byte.
    /// `crc` is the CRC of the unmodified 112-bit message.
    fn fix_df17_type(&self, first_byte: u8, msgtype: u8, crc: u32) -> Option<u8> {
        if !self.fix_df || self.nfix_crc == 0 || !fix_df17_candidate(msgtype) {
            return None;
        }
        let new_byte = (first_byte & 7) | (17 << 3);
        let diff = first_byte ^ new_byte;
        // The CRC is linear, so the CRC of the message with the DF rewritten to 17
        // is the original CRC XOR the syndromes of the changed DF bits.
        let mut syndrome = 0;
        for bit in 0..5 {
            if diff & (0x80 >> bit) != 0 {
                syndrome ^= crc::bit_syndrome(bit);
            }
        }
        (crc ^ syndrome == 0).then_some(new_byte)
    }

    /// Score how plausible a Mode S message looks (higher is more reliable).
    ///
    /// * 1800: DF17/18, good CRC, known address   * 1400: same, unknown address
    /// * 900/700: 1-bit error, known/unknown      * 450/350: 2-bit error, known/unknown
    /// * 1600/1000/800/750: DF11 variants         * 1000: DF0/4/5/16/20/21 with a known address
    /// * -1: might be valid but CRC not verifiable against a known ICAO
    /// * -2: bad message or unrepairable CRC error
    pub fn score(&self, msg: &[u8], validbits: usize) -> i32 {
        if validbits < SHORT_MSG_BITS {
            return -2;
        }
        let mut msgtype = getbits(msg, 1, 5) as u8;
        let mut crc = 0;
        let mut have_crc = false;

        if validbits >= LONG_MSG_BITS
            && self.fix_df
            && self.nfix_crc != 0
            && fix_df17_candidate(msgtype)
        {
            crc = crc::checksum(&msg[..LONG_MSG_BYTES]);
            have_crc = true;
            if self.fix_df17_type(msg[0], msgtype, crc).is_some() {
                // DF17 with a single bit error in the DF field.
                return if self.icao.test(getbits(msg, 9, 32)) {
                    1800 / 2
                } else {
                    1400 / 2
                };
            }
        }

        let msgbits = msg_len_by_type(msgtype);
        if validbits < msgbits {
            return -2;
        }
        // Discard messages that are all zero in the first short-message bytes.
        if msg[..SHORT_MSG_BYTES].iter().all(|&b| b == 0) {
            return -2;
        }
        if !have_crc || msgbits != LONG_MSG_BITS {
            crc = crc::checksum(&msg[..msgbits / 8]);
        }
        let _ = &mut msgtype;

        match msgtype {
            0 | 4 | 5 | 16 | 20 | 21 => {
                if self.icao.test(crc) {
                    1000
                } else {
                    -1
                }
            }
            11 => {
                let iid = crc & 0x7f;
                let mut addr = getbits(msg, 9, 32);
                if crc & 0xffff80 != 0 {
                    // Diagnose using the full CRC, i.e. assuming IID = 0.
                    let Some(ei) = self.fixer.diagnose(crc, msgbits) else {
                        return -2;
                    };
                    // Two-bit errors are ambiguous in DF11: only single-bit fixes.
                    if ei.errors > 1 {
                        return -2;
                    }
                    correct_aa_field(&mut addr, &ei);
                    return if self.icao.test(addr) { 800 } else { -1 };
                }
                match (iid == 0, self.icao.test(addr)) {
                    (true, true) => 1600,
                    (true, false) => 750,
                    (false, true) => 1000,
                    (false, false) => -1,
                }
            }
            17 | 18 => {
                let Some(ei) = self.fixer.diagnose(crc, msgbits) else {
                    return -2;
                };
                let mut addr = getbits(msg, 9, 32);
                correct_aa_field(&mut addr, &ei);
                let base = if self.icao.test(addr) { 1800 } else { 1400 };
                base / (ei.errors as i32 + 1)
            }
            _ => -2,
        }
    }

    /// Decode `mm.msg` in place, filling in the rest of `mm`. Returns `mm.decode_result`:
    /// 0 = ok, -1 = maybe valid but unverified, -2 = bad or unrepairable.
    pub fn decode(&mut self, mm: &mut ModesMessage) -> i32 {
        mm.verbatim = mm.msg;
        mm.decode_result = 0;

        macro_rules! decode_return {
            ($x:expr) => {{
                mm.decode_result = $x;
                if !self.decode_all {
                    return mm.decode_result;
                }
            }};
        }

        if mm.msg[..SHORT_MSG_BYTES].iter().all(|&b| b == 0) {
            decode_return!(-2);
        }

        mm.msgtype = getbits(&mm.msg, 1, 5) as u8;
        mm.corrected_bits = 0;

        let mut crc = 0;
        let mut have_crc = false;
        if self.fix_df && self.nfix_crc != 0 && fix_df17_candidate(mm.msgtype) {
            crc = crc::checksum(&mm.msg[..LONG_MSG_BYTES]);
            have_crc = true;
            if let Some(nb) = self.fix_df17_type(mm.msg[0], mm.msgtype, crc) {
                mm.msg[0] = nb;
                mm.msgtype = 17;
                mm.corrected_bits = 1;
                crc = 0; // the rewritten DF17 message has a valid CRC
            }
        }

        mm.msgbits = msg_len_by_type(mm.msgtype);
        mm.crc = if have_crc && mm.msgbits == LONG_MSG_BITS {
            crc
        } else {
            crc::checksum(&mm.msg[..mm.msgbits / 8])
        };
        mm.addr = HEX_UNKNOWN;
        mm.maybe_addr = HEX_UNKNOWN;
        mm.addrtype = AddrType::Unknown;

        // CRC work, and fields that depend on it.
        match mm.msgtype {
            0 | 4 | 5 | 16 | 24..=31 => {
                // Address/Parity: the CRC syndrome is the sender's ICAO address. Accept if
                // it matches a previously seen aircraft.
                mm.maybe_addr = mm.crc;
                if !self.icao.test(mm.crc) {
                    decode_return!(-1);
                }
                mm.source = DataSource::ModeS;
                mm.addr = mm.crc;
                mm.addrtype = AddrType::ModeS;
            }
            11 => {
                // Parity/Interrogator: CL + IC occupy the low 7 bits of the CRC, so the
                // upper 17 bits can still be checked.
                mm.iid = mm.crc & 0x7f;
                mm.maybe_addr = getbits(&mm.msg, 9, 32);
                if mm.crc & 0xffff80 != 0 {
                    match self.fixer.diagnose(mm.crc, mm.msgbits) {
                        None => decode_return!(-2),
                        // Two-bit errors are ambiguous in DF11: only single-bit fixes.
                        Some(ei) if ei.errors > 1 => decode_return!(-2),
                        Some(ei) => {
                            mm.corrected_bits = ei.errors;
                            mm.iid = 0;
                            ei.apply(&mut mm.msg);
                            // Be conservative: only accept corrected messages matching a known aircraft.
                            let addr = getbits(&mm.msg, 9, 32);
                            mm.maybe_addr = addr;
                            if !self.icao.test(addr) {
                                decode_return!(-1);
                            }
                        }
                    }
                }
                mm.source = DataSource::ModeSChecked;
                mm.addrtype = AddrType::ModeS;
            }
            17 | 18 => {
                // Parity/Interrogator, but II is specified as 0.
                if mm.crc != 0 {
                    match self.fixer.diagnose(mm.crc, mm.msgbits) {
                        None => decode_return!(-2),
                        Some(ei) => {
                            let addr1 = getbits(&mm.msg, 9, 32);
                            mm.corrected_bits = ei.errors;
                            ei.apply(&mut mm.msg);
                            let addr2 = getbits(&mm.msg, 9, 32);
                            mm.maybe_addr = addr2;
                            if addr1 != addr2 && !self.icao.test(addr2) {
                                decode_return!(-1);
                            }
                        }
                    }
                }
                mm.addrtype = AddrType::AdsbIcao;
                mm.source = DataSource::Adsb; // TIS-B decoding overrides this if needed
            }
            20 | 21 => {
                // Address/Parity or Data Parity (BDS xored into the top byte): we can't
                // know that the ICAO is right, so only accept an exact match.
                mm.maybe_addr = mm.crc;
                if self.icao.test(mm.crc) {
                    mm.addrtype = AddrType::ModeS;
                    mm.source = DataSource::ModeS;
                    mm.addr = mm.crc;
                } else {
                    decode_return!(-1);
                }
            }
            _ => decode_return!(-2),
        }

        self.decode_fields(mm);

        if mm.decode_result == 0
            && mm.corrected_bits == 0
            && (mm.msgtype == 17 || (mm.msgtype == 11 && mm.iid == 0))
        {
            // No CRC errors and either an extended squitter or an acquisition squitter with
            // II = 0: we have the right address. Not for DF18, a DF18 transmitter doesn't
            // necessarily have a Mode S transponder. This is the only place addresses are added.
            self.icao.add(mm.addr);
        }

        mm.decode_result
    }

    /// Convenience: build a message from bytes, decode it, return it.
    pub fn decode_bytes(&mut self, bytes: &[u8]) -> ModesMessage {
        let mut mm = ModesMessage::from_bytes(bytes);
        self.decode(&mut mm);
        mm
    }

    fn decode_fields(&self, mm: &mut ModesMessage) {
        let df = mm.msgtype;
        let msg = mm.msg;

        // AA (Address announced)
        if matches!(df, 11 | 17 | 18) {
            mm.aa = getbits(&msg, 9, 32);
            mm.maybe_addr = mm.aa;
            if mm.decode_result == 0 {
                mm.addr = mm.aa;
            }
        }

        // AC (Altitude code)
        if matches!(df, 0 | 4 | 16 | 20) {
            mm.ac = getbits(&msg, 20, 32);
            if mm.ac != 0
                && let Some((alt, q)) = decode_ac13(mm.ac)
            {
                mm.alt_q_bit = q;
                mm.baro_alt = Some(alt);
            }
        }

        // CA (Capability)
        if matches!(df, 11 | 17) {
            mm.ca = getbits(&msg, 6, 8);
            mm.airground = match mm.ca {
                4 => AirGround::Ground,
                5 => AirGround::Airborne,
                0 | 6 | 7 => AirGround::Uncertain,
                _ => mm.airground,
            };
        }

        if df == 0 {
            mm.cc = getbit(&msg, 7) as u32; // cross-link capability
        }
        if df == 18 {
            mm.cf = getbits(&msg, 6, 8); // control field
        }
        if matches!(df, 4 | 5 | 20 | 21) {
            mm.dr = getbits(&msg, 9, 13);
            mm.um = getbits(&msg, 14, 19);
            // FS (Flight status)
            mm.fs = getbits(&msg, 6, 8);
            let (ag, alert, spi) = match mm.fs {
                0 => (Some(AirGround::Uncertain), false, false),
                1 => (Some(AirGround::Ground), false, false),
                2 => (Some(AirGround::Uncertain), true, false),
                3 => (Some(AirGround::Ground), true, false),
                4 => (Some(AirGround::Uncertain), true, true),
                5 => (Some(AirGround::Uncertain), false, true),
                _ => (None, false, false),
            };
            if let Some(ag) = ag {
                mm.airground = ag;
                mm.alert = Some(alert);
                mm.spi = Some(spi);
            }
        }

        // ID (Identity): Gillham-encoded squawk
        if matches!(df, 5 | 21) {
            mm.id = getbits(&msg, 20, 32);
            if mm.id != 0 {
                mm.squawk = Some(Squawk::from_hex(decode_id13(mm.id)));
            }
        }

        if (24..=31).contains(&df) {
            mm.ke = getbit(&msg, 4) as u32;
            mm.md.copy_from_slice(&msg[1..11]);
            mm.nd = getbits(&msg, 5, 8);
        }

        // MB (Comm-B): infer which register it carries and decode it
        if matches!(df, 20 | 21) {
            mm.mb.copy_from_slice(&msg[4..11]);
            decode_comm_b(mm);
        }

        // ME (extended squitter)
        if matches!(df, 17 | 18) {
            mm.me.copy_from_slice(&msg[4..11]);
            self.decode_extended_squitter(mm);
        }

        // MV (ACAS)
        if df == 16 {
            mm.mv.copy_from_slice(&msg[4..11]);
            if mm.mv[0] == 0x30 {
                mm.acas_ra = true;
            }
        }

        if matches!(df, 0 | 16) {
            mm.ri = getbits(&msg, 14, 17);
            mm.sl = getbits(&msg, 9, 11);
            mm.vs = getbit(&msg, 6) as u32;
            mm.airground = if mm.vs != 0 {
                AirGround::Ground
            } else {
                AirGround::Uncertain
            };
        }
    }

    // -----------------------------------------------------------------------
    // Extended squitter
    // -----------------------------------------------------------------------

    fn decode_extended_squitter(&self, mm: &mut ModesMessage) {
        let me = mm.me;
        let metype = getbits(&me, 1, 5);
        mm.metype = metype;
        let mut check_imf = false;

        // On DF18 the CF field tells us the format and whether to look for an IMF bit.
        if mm.msgtype == 18 {
            match mm.cf {
                0 => {
                    mm.addrtype = AddrType::AdsbIcaoNt;
                    mm.source = DataSource::Nt;
                }
                1 => {
                    mm.addrtype = AddrType::AdsbOther;
                    mm.addr |= NON_ICAO_ADDRESS;
                }
                2 => {
                    mm.source = DataSource::Tisb;
                    mm.addrtype = AddrType::TisbIcao;
                    check_imf = true;
                }
                3 => {
                    // Coarse TIS-B airborne position and velocity: only the IMF bit for now.
                    mm.source = DataSource::Tisb;
                    mm.addrtype = AddrType::TisbIcao;
                    if getbit(&me, 1) {
                        set_imf(mm);
                    }
                    return;
                }
                5 => {
                    mm.addrtype = AddrType::TisbOther;
                    mm.source = DataSource::Tisb;
                    mm.addr |= NON_ICAO_ADDRESS;
                }
                6 => {
                    mm.addrtype = AddrType::AdsrIcao;
                    mm.source = DataSource::Adsr;
                    check_imf = true;
                }
                _ => {
                    mm.addrtype = AddrType::Unknown;
                    mm.addr |= NON_ICAO_ADDRESS;
                    return;
                }
            }
        }

        match metype {
            1..=4 => decode_es_ident_and_category(mm),
            19 => decode_es_airborne_velocity(mm, check_imf),
            5..=8 => decode_es_surface_position(mm, check_imf),
            0 | 9..=18 | 20..=22 => decode_es_airborne_position(mm, check_imf),
            23 => decode_es_test_message(mm),
            28 => decode_es_aircraft_status(mm, check_imf),
            29 => decode_es_target_status(mm, check_imf),
            31 => decode_es_operational_status(mm, check_imf),
            _ => {} // 24 surface system status, 30 operational coordination: nothing to decode
        }
    }
}

/// Handle a non-ICAO address (IMF bit set).
fn set_imf(mm: &mut ModesMessage) {
    mm.addr |= NON_ICAO_ADDRESS;
    mm.addrtype = match mm.addrtype {
        AddrType::AdsbIcao | AddrType::AdsbIcaoNt => AddrType::AdsbOther,
        AddrType::TisbIcao => AddrType::TisbTrackfile,
        AddrType::AdsrIcao => AddrType::AdsrOther,
        other => other,
    };
}

fn decode_es_ident_and_category(mm: &mut ModesMessage) {
    let me = mm.me;
    mm.mesub = getbits(&me, 6, 8);

    let mut callsign = [0u8; 8];
    for (i, c) in callsign.iter_mut().enumerate() {
        let first = 9 + 6 * i as u32;
        *c = AIS_CHARSET[getbits(&me, first, first + 5) as usize];
    }
    mm.callsign_valid = callsign
        .iter()
        .all(|&c| c.is_ascii_uppercase() || (b'-'..=b'9').contains(&c) || c == b' ' || c == b'@');
    mm.callsign = Some(callsign.iter().map(|&c| c as char).collect());
    mm.category = Some((((0x0E - mm.metype) << 4) | mm.mesub) as u8);
}

fn decode_es_airborne_velocity(mm: &mut ModesMessage, check_imf: bool) {
    let me = mm.me;
    mm.mesub = getbits(&me, 6, 8);
    if !(1..=4).contains(&mm.mesub) {
        return;
    }
    if check_imf && getbit(&me, 9) {
        set_imf(mm);
    }

    // 11-13: NACv (NUCr in v0, maps directly to NACv in v2)
    mm.accuracy.nac_v = Some(getbits(&me, 11, 13) as u8);

    match mm.mesub {
        1 | 2 => {
            let ew_raw = getbits(&me, 15, 24);
            let ns_raw = getbits(&me, 26, 35);
            if ew_raw != 0 && ns_raw != 0 {
                let scale = if mm.mesub == 2 { 4 } else { 1 };
                let ew_vel = (ew_raw as i32 - 1) * if getbit(&me, 14) { -1 } else { 1 } * scale;
                let ns_vel = (ns_raw as i32 - 1) * if getbit(&me, 25) { -1 } else { 1 } * scale;

                let gs = (((ns_vel * ns_vel + ew_vel * ew_vel) as f64 + 0.5) as f32).sqrt();
                mm.gs = Some(GroundSpeed {
                    v0: gs,
                    v2: gs,
                    selected: gs,
                });
                if gs > 0.0 {
                    let mut track =
                        (ew_vel as f32).atan2(ns_vel as f32) * 180.0 / std::f32::consts::PI;
                    if track < 0.0 {
                        track += 360.0;
                    }
                    mm.heading = Some(track);
                    mm.heading_type = HeadingType::GroundTrack;
                }
            }
        }
        _ => {
            // 3, 4: airspeed and heading
            if getbit(&me, 14) {
                mm.heading = Some((getbits(&me, 15, 24) as f64 * 360.0 / 1024.0) as f32);
                mm.heading_type = HeadingType::MagneticOrTrue;
            }
            let airspeed = getbits(&me, 26, 35);
            if airspeed != 0 {
                let speed = (airspeed - 1) * if mm.mesub == 4 { 4 } else { 1 };
                if getbit(&me, 25) {
                    mm.tas = Some(speed);
                } else {
                    mm.ias = Some(speed);
                }
            }
        }
    }

    // 36: vertical rate source, 37: sign, 38-46: magnitude
    let vert_rate = getbits(&me, 38, 46);
    if vert_rate != 0 {
        let rate = (vert_rate as i32 - 1) * if getbit(&me, 37) { -64 } else { 64 };
        if getbit(&me, 36) {
            mm.baro_rate = Some(rate);
        } else {
            mm.geom_rate = Some(rate);
        }
    }

    // 49: baro/geom delta sign, 50-56: magnitude
    let raw_delta = getbits(&me, 50, 56);
    if raw_delta != 0 {
        mm.geom_delta = Some((raw_delta as i32 - 1) * if getbit(&me, 49) { -25 } else { 25 });
    }
}

fn decode_es_surface_position(mm: &mut ModesMessage, check_imf: bool) {
    let me = mm.me;
    mm.airground = AirGround::Ground; // definitely

    // 6-12: movement
    let movement = getbits(&me, 6, 12);
    if movement > 0 && movement < 125 {
        let v0 = movement_v0(movement); // assumed v0 until told otherwise
        mm.gs = Some(GroundSpeed {
            v0,
            v2: movement_v2(movement),
            selected: v0,
        });
    }

    // 13: heading/track status, 14-20: heading/track
    if getbit(&me, 13) {
        mm.heading = Some((getbits(&me, 14, 20) as f64 * 360.0 / 128.0) as f32);
        mm.heading_type = HeadingType::TrackOrHeading;
    }

    // 21: IMF or T flag
    if check_imf && getbit(&me, 21) {
        set_imf(mm);
    }

    mm.cpr = Some(CprFields {
        kind: CprType::Surface,
        odd: getbit(&me, 22),
        lat: getbits(&me, 23, 39),
        lon: getbits(&me, 40, 56),
    });
}

fn decode_es_airborne_position(mm: &mut ModesMessage, check_imf: bool) {
    let me = mm.me;

    // 6-7: surveillance status
    match getbits(&me, 6, 7) {
        0 => {
            mm.alert = Some(false);
            mm.spi = Some(false);
        }
        1 | 2 => mm.alert = Some(true), // states 1/2 override 3, SPI unknown
        _ => {
            mm.alert = Some(false);
            mm.spi = Some(true);
        }
    }

    // 8: IMF or NIC supplement-B
    if check_imf {
        if getbit(&me, 8) {
            set_imf(mm);
        }
    } else {
        mm.accuracy.nic_b = Some(getbit(&me, 8));
    }

    // 9-20: altitude
    let ac12 = getbits(&me, 9, 20);

    if mm.metype != 0 {
        // 21: T flag, 22: F flag (odd/even), 23-39 CPR latitude, 40-56 CPR longitude
        let lat = getbits(&me, 23, 39);
        let lon = getbits(&me, 40, 56);
        // Some transponders send altitude 0, longitude 0, type 15 and zero latitude LSBs;
        // don't use those for positioning.
        let bogus = ac12 == 0 && lon == 0 && (lat & 0x0fff) == 0 && mm.metype == 15;
        mm.cpr_filtered = bogus;
        if !bogus {
            mm.cpr = Some(CprFields {
                kind: CprType::Airborne,
                odd: getbit(&me, 22),
                lat,
                lon,
            });
        }
    }

    // Only decode a non-zero altitude, and not while on the ground.
    if ac12 != 0
        && mm.airground != AirGround::Ground
        && let Some((alt, q)) = decode_ac12(ac12)
    {
        mm.alt_q_bit = q;
        if matches!(mm.metype, 20..=22) {
            mm.geom_alt = Some(alt);
        } else {
            mm.baro_alt = Some(alt);
        }
    }
}

fn decode_es_test_message(mm: &mut ModesMessage) {
    let me = mm.me;
    mm.mesub = getbits(&me, 6, 8);
    if mm.mesub == 7 {
        let id13 = getbits(&me, 9, 21);
        if id13 != 0 {
            mm.squawk = Some(Squawk::from_hex(decode_id13(id13)));
        }
    }
}

fn decode_es_aircraft_status(mm: &mut ModesMessage, check_imf: bool) {
    let me = mm.me;
    mm.mesub = getbits(&me, 6, 8);
    if mm.mesub == 1 {
        mm.emergency = Some(Emergency::from_bits(getbits(&me, 9, 11)));
        let id13 = getbits(&me, 12, 24);
        if id13 != 0 {
            mm.squawk = Some(Squawk::from_hex(decode_id13(id13)));
        }
        if check_imf && getbit(&me, 56) {
            set_imf(mm);
        }
    }
    if mm.mesub == 2 {
        mm.acas_ra = true;
    }
}

fn decode_es_target_status(mm: &mut ModesMessage, check_imf: bool) {
    let me = mm.me;
    mm.mesub = getbits(&me, 6, 7); // only 2 bits of subtype

    if check_imf && getbit(&me, 51) {
        set_imf(mm);
    }

    if mm.mesub == 0 && !getbit(&me, 11) {
        // Target state and status, v1
        mm.nav.altitude_source = match getbits(&me, 8, 9) {
            1 => NavAltitudeSource::Mcp,
            2 => NavAltitudeSource::Aircraft,
            3 => NavAltitudeSource::Fms,
            _ => mm.nav.altitude_source,
        };
        let src = mm.nav.altitude_source;
        let mut modes: u8 = 0;
        let mut modes_valid = false;

        // 14-15: vertical mode
        match getbits(&me, 14, 15) {
            1 => {
                modes_valid = true;
                modes |= if src == NavAltitudeSource::Fms {
                    nav_mode::VNAV
                } else {
                    nav_mode::AUTOPILOT
                };
            }
            2 => {
                modes_valid = true;
                modes |= match src {
                    NavAltitudeSource::Fms => nav_mode::VNAV,
                    NavAltitudeSource::Aircraft => nav_mode::ALT_HOLD,
                    _ => nav_mode::AUTOPILOT,
                };
            }
            _ => {}
        }

        // 16-25: target altitude
        let alt = (-1000 + 100 * getbits(&me, 16, 25) as i64) as u32;
        match src {
            NavAltitudeSource::Mcp => mm.nav.mcp_altitude = Some(alt),
            NavAltitudeSource::Fms => mm.nav.fms_altitude = Some(alt),
            _ => {}
        }

        // 26-27: horizontal data source; 28-36: target heading/track; 37: track vs heading
        let h_source = getbits(&me, 26, 27);
        if h_source != 0 {
            mm.nav.heading = Some(getbits(&me, 28, 36) as f32);
            mm.nav.heading_type = if getbit(&me, 37) {
                HeadingType::GroundTrack
            } else {
                HeadingType::MagneticOrTrue
            };
        }
        // 38-39: horizontal mode
        if matches!(getbits(&me, 38, 39), 1 | 2) {
            modes_valid = true;
            modes |= if h_source == 3 {
                nav_mode::LNAV
            } else {
                nav_mode::AUTOPILOT
            };
        }

        // 40-43: NACp, 44: NICbaro, 45-46: SIL
        mm.accuracy.nac_p = Some(getbits(&me, 40, 43) as u8);
        mm.accuracy.nic_baro = Some(getbit(&me, 44));
        mm.accuracy.sil = getbits(&me, 45, 46) as u8;
        mm.accuracy.sil_type = SilType::Unknown;

        // 52-53: TCAS status
        match getbits(&me, 52, 53) {
            1 => modes_valid = true, // no TCAS
            2 | 3 => {
                modes_valid = true;
                modes |= nav_mode::TCAS;
            }
            0 => modes |= nav_mode::TCAS, // assume TCAS if we had any other modes, but don't enable modes just for this
            _ => {}
        }
        if modes_valid {
            mm.nav.modes = Some(modes);
        }

        // 54-56: emergency/priority
        mm.emergency = Some(Emergency::from_bits(getbits(&me, 54, 56)));
    } else if mm.mesub == 1 {
        // Target state and status, v2
        let is_fms = getbit(&me, 9);
        let alt_bits = getbits(&me, 10, 20);
        if alt_bits != 0 {
            let alt = (alt_bits - 1) * 32;
            if is_fms {
                mm.nav.fms_altitude = Some(alt);
            } else {
                mm.nav.mcp_altitude = Some(alt);
            }
        }
        let baro_bits = getbits(&me, 21, 29);
        if baro_bits != 0 {
            mm.nav.qnh = Some((800.0 + (baro_bits as f64 - 1.0) * 0.8) as f32);
        }
        if getbit(&me, 30) {
            // Two's complement -180..+180, conveniently also the same as unsigned 0..360.
            mm.nav.heading = Some((getbits(&me, 31, 39) as f64 * 180.0 / 256.0) as f32);
            mm.nav.heading_type = HeadingType::MagneticOrTrue;
        }
        mm.accuracy.nac_p = Some(getbits(&me, 40, 43) as u8);
        mm.accuracy.nic_baro = Some(getbit(&me, 44));
        mm.accuracy.sil = getbits(&me, 45, 46) as u8;
        mm.accuracy.sil_type = SilType::Unknown;

        // 47: mode bits validity, 48-54: mode bits
        if getbit(&me, 47) {
            let flag = |bit, mode| if getbit(&me, bit) { mode } else { 0 };
            mm.nav.modes = Some(
                flag(48, nav_mode::AUTOPILOT)
                    | flag(49, nav_mode::VNAV)
                    | flag(50, nav_mode::ALT_HOLD)
                    | flag(52, nav_mode::APPROACH)
                    | flag(53, nav_mode::TCAS)
                    | flag(54, nav_mode::LNAV),
            );
        }
    }
}

fn decode_es_operational_status(mm: &mut ModesMessage, check_imf: bool) {
    let me = mm.me;
    mm.mesub = getbits(&me, 6, 8);
    if check_imf && getbit(&me, 56) {
        set_imf(mm);
    }
    if mm.mesub > 1 {
        return;
    }
    let surface = mm.mesub == 1;
    let op = &mut mm.opstatus;
    let acc = &mut mm.accuracy;
    op.valid = true;
    op.version = getbits(&me, 41, 43) as u8;

    match op.version {
        0 => {
            if !surface && getbits(&me, 9, 10) == 0 {
                op.cc_acas = !getbit(&me, 12);
                op.cc_cdti = getbit(&me, 13);
            }
        }
        1 => {
            if getbits(&me, 25, 26) == 0 {
                op.om_acas_ra = getbit(&me, 27);
                op.om_ident = getbit(&me, 28);
                op.om_atc = getbit(&me, 29);
            }
            if getbits(&me, 9, 10) == 0 && getbits(&me, 13, 14) == 0 {
                if !surface {
                    op.cc_acas = !getbit(&me, 11);
                    op.cc_cdti = getbit(&me, 12);
                    op.cc_arv = getbit(&me, 15);
                    op.cc_ts = getbit(&me, 16);
                    op.cc_tc = getbits(&me, 17, 18) as u8;
                } else {
                    op.cc_poa = getbit(&me, 11);
                    op.cc_cdti = getbit(&me, 12);
                    op.cc_b2_low = getbit(&me, 15);
                    op.cc_lw_valid = true;
                    op.cc_lw = getbits(&me, 21, 24) as u8;
                }
            }
            acc.nic_a = Some(getbit(&me, 44));
            acc.nac_p = Some(getbits(&me, 45, 48) as u8);
            acc.sil_type = SilType::Unknown;
            acc.sil = getbits(&me, 51, 52) as u8;
            op.hrd = if getbit(&me, 54) {
                HeadingType::Magnetic
            } else {
                HeadingType::True
            };
            if !surface {
                acc.nic_baro = Some(getbit(&me, 53));
            } else {
                // DO-260B 2.2.3.2.7.2.12: TAH=0 reports ground track, TAH=1 aircraft heading.
                op.tah = if getbit(&me, 53) {
                    op.hrd
                } else {
                    HeadingType::GroundTrack
                };
            }
        }
        2 => {
            if getbits(&me, 25, 26) == 0 {
                op.om_acas_ra = getbit(&me, 27);
                op.om_ident = getbit(&me, 28);
                op.om_atc = getbit(&me, 29);
                op.om_saf = getbit(&me, 30);
                acc.sda = Some(getbits(&me, 31, 32) as u8);
            }
            if getbits(&me, 9, 10) == 0 {
                if !surface {
                    op.cc_acas = getbit(&me, 11); // inverted sense versus v0/v1
                    op.cc_1090_in = getbit(&me, 12);
                    op.cc_arv = getbit(&me, 15);
                    op.cc_ts = getbit(&me, 16);
                    op.cc_tc = getbits(&me, 17, 18) as u8;
                    op.cc_uat_in = getbit(&me, 19);
                } else {
                    op.cc_poa = getbit(&me, 11);
                    op.cc_1090_in = getbit(&me, 12);
                    op.cc_b2_low = getbit(&me, 15);
                    op.cc_uat_in = getbit(&me, 16);
                    acc.nac_v = Some(getbits(&me, 17, 19) as u8);
                    acc.nic_c = Some(getbit(&me, 20));
                    op.cc_lw_valid = true;
                    op.cc_lw = getbits(&me, 21, 24) as u8;
                    op.cc_antenna_offset = getbits(&me, 33, 40) as u8;
                }
            }
            acc.nic_a = Some(getbit(&me, 44));
            acc.nac_p = Some(getbits(&me, 45, 48) as u8);
            acc.sil = getbits(&me, 51, 52) as u8;
            acc.sil_type = if getbit(&me, 55) {
                SilType::PerSample
            } else {
                SilType::PerHour
            };
            op.hrd = if getbit(&me, 54) {
                HeadingType::Magnetic
            } else {
                HeadingType::True
            };
            if !surface {
                acc.gva = Some(getbits(&me, 49, 50) as u8);
                acc.nic_baro = Some(getbit(&me, 53));
            } else {
                op.tah = if getbit(&me, 53) {
                    op.hrd
                } else {
                    HeadingType::GroundTrack
                };
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn decoder() -> Decoder {
        Decoder::new(1)
    }

    #[test]
    fn df17_identification() {
        let mut d = decoder();
        let mm = d.decode_bytes(&hex("8D4840D6202CC371C32CE0576098"));
        assert_eq!(mm.decode_result, 0);
        assert_eq!(mm.msgtype, 17);
        assert_eq!(mm.addr, 0x4840D6);
        assert_eq!(mm.addrtype, AddrType::AdsbIcao);
        assert_eq!(mm.source, DataSource::Adsb);
        assert_eq!(mm.callsign.as_deref(), Some("KLM1023 "));
        assert!(mm.callsign_valid);
        assert_eq!(mm.category, Some(0xA0)); // A0: no category info
        assert!(d.icao.test(0x4840D6), "good DF17 registers its address");
    }

    #[test]
    fn df17_airborne_position() {
        let mut d = decoder();
        let even = d.decode_bytes(&hex("8D40621D58C382D690C8AC2863A7"));
        assert_eq!(even.decode_result, 0);
        assert_eq!(even.metype, 11);
        assert_eq!(even.baro_alt, Some(38000));
        assert!(even.alt_q_bit);
        let c = even.cpr.unwrap();
        assert_eq!(
            (c.kind, c.odd, c.lat, c.lon),
            (CprType::Airborne, false, 93000, 51372)
        );
        let odd = d.decode_bytes(&hex("8D40621D58C386435CC412692AD6"));
        let c = odd.cpr.unwrap();
        assert_eq!((c.odd, c.lat, c.lon), (true, 74158, 50194));
    }

    #[test]
    fn df17_airborne_velocity() {
        let mut d = decoder();
        let mm = d.decode_bytes(&hex("8D485020994409940838175B284F"));
        assert_eq!(mm.decode_result, 0);
        assert_eq!(mm.metype, 19);
        assert_eq!(mm.mesub, 1);
        let gs = mm.gs.unwrap().selected;
        assert!((gs - 159.2).abs() < 0.5, "gs {gs}");
        let trk = mm.heading.unwrap();
        assert!((trk - 182.88).abs() < 0.05, "track {trk}");
        assert_eq!(mm.heading_type, HeadingType::GroundTrack);
        assert_eq!(mm.baro_rate.or(mm.geom_rate), Some(-832));
        assert_eq!(mm.geom_delta, Some(550));
    }

    #[test]
    fn single_bit_error_is_corrected_for_known_aircraft() {
        let mut d = decoder();
        let good = hex("8D4840D6202CC371C32CE0576098");
        d.decode_bytes(&good); // learn the address
        let mut bad = good.clone();
        bad[9] ^= 0x08;
        let mm = d.decode_bytes(&bad);
        assert_eq!(mm.decode_result, 0);
        assert_eq!(mm.corrected_bits, 1);
        assert_eq!(&mm.msg[..14], &good[..]);
        assert_eq!(&mm.verbatim[..14], &bad[..]);
        assert_eq!(mm.callsign.as_deref(), Some("KLM1023 "));
    }

    #[test]
    fn corruption_is_rejected_without_correction() {
        let mut d = Decoder::new(0);
        let mut bad = hex("8D4840D6202CC371C32CE0576098");
        bad[9] ^= 0x08;
        assert_eq!(d.decode_bytes(&bad).decode_result, -2);
    }

    #[test]
    fn df_field_error_on_df17_is_repaired() {
        let mut d = decoder();
        let mut bad = hex("8D4840D6202CC371C32CE0576098");
        bad[0] ^= 0x08; // DF17 -> DF21 (single bit error in the DF field)
        assert!(d.score(&bad, 112) > 0);
        let mm = d.decode_bytes(&bad);
        assert_eq!(mm.decode_result, 0);
        assert_eq!(mm.msgtype, 17);
        assert_eq!(mm.corrected_bits, 1);
        assert_eq!(mm.addr, 0x4840D6);
        let mut off = Decoder::new(1);
        off.fix_df = false;
        assert_eq!(off.decode_bytes(&bad).decode_result, -1);
    }

    #[test]
    fn df17_scores() {
        let mut d = decoder();
        let good = hex("8D4840D6202CC371C32CE0576098");
        assert_eq!(d.score(&good, 112), 1400, "unknown address");
        d.decode_bytes(&good);
        assert_eq!(d.score(&good, 112), 1800, "known address");
        let mut one = good.clone();
        one[9] ^= 1;
        assert_eq!(d.score(&one, 112), 900);
        assert_eq!(d.score(&good, 100), -2, "too short");
        assert_eq!(d.score(&[0u8; 14], 112), -2, "all zero");
    }

    #[test]
    fn address_parity_messages_need_a_known_address() {
        // DF4 altitude reply whose parity field is the ICAO address 4840D6.
        let mut raw = vec![0x20, 0x00, 0x19, 0x38, 0, 0, 0];
        let crc = crc::checksum(&raw);
        let addr = 0x4840D6u32;
        let p = crc ^ addr;
        raw[4] = (p >> 16) as u8;
        raw[5] = (p >> 8) as u8;
        raw[6] = p as u8;
        // checksum over the full frame now equals the address
        assert_eq!(crc::checksum(&raw), addr);

        let mut d = decoder();
        let mm = d.decode_bytes(&raw);
        assert_eq!(mm.decode_result, -1, "unknown aircraft");
        assert_eq!(mm.maybe_addr, addr);

        d.icao.add(addr);
        assert_eq!(d.score(&raw, 56), 1000);
        let mm = d.decode_bytes(&raw);
        assert_eq!(mm.decode_result, 0);
        assert_eq!(mm.msgtype, 4);
        assert_eq!(mm.addr, addr);
        assert_eq!(mm.addrtype, AddrType::ModeS);
        assert_eq!(mm.source, DataSource::ModeS);
        assert!(mm.baro_alt.is_some());
    }

    #[test]
    fn ac13_altitudes() {
        // Q=1, N=0x4C8 -> 1224*25-1000 = 29600? use a hand-built field: n=1000 -> 24000 ft
        let n = 1000u32;
        let ac13 = ((n & 0x7E0) << 2) | ((n & 0x10) << 1) | 0x10 | (n & 0x0F);
        assert_eq!(decode_ac13(ac13), Some((24000, true)));
        // Meters unsupported.
        assert_eq!(decode_ac13(0x0040), None);
        // 12 bit field: n=1000.
        let ac12 = ((n & 0x7F0) << 1) | 0x10 | (n & 0x0F);
        assert_eq!(decode_ac12(ac12), Some((24000, true)));
        // Gillham: C4 only set => -1200 ft.
        let id = 0x0100; // C4 bit in the 13-bit field: lowest valid Gillham code
        assert_eq!(decode_ac13(id), Some((-1200, false)));
    }

    #[test]
    fn squawk_decoding() {
        // 7700: A=7 B=7 C=0? 7700 -> A4A2A1=111, B=111, C=000, D=000
        // 13-bit order C1 A1 C2 A2 C4 A4 X B1 D1 B2 D2 B4 D4
        let id13 = 0b0_1010_1010_1010; // A1 A2 A4 (bits 11,9,7) and B1 B2 B4 (bits 5,3,1)
        let hex = decode_id13(id13);
        assert_eq!(hex, 0x7700);
        assert_eq!(Squawk::from_hex(hex).dec, 7700);
    }

    #[test]
    fn mode_a_message() {
        let mm = ModesMessage::from_mode_a(0x1200);
        assert_eq!(mm.msgtype, DFTYPE_MODEAC);
        assert_eq!(mm.msgbits, 16);
        assert_eq!(mm.source, DataSource::ModeAc);
        assert_eq!(mm.addrtype, AddrType::ModeA);
        assert_eq!(
            mm.squawk,
            Some(Squawk {
                hex: 0x1200,
                dec: 1200
            })
        );
        assert_eq!(mm.spi, Some(false));
        assert_eq!(mm.addr, 0x1200 | NON_ICAO_ADDRESS);
        let ident = ModesMessage::from_mode_a(0x1280);
        assert_eq!(ident.spi, Some(true));
        assert_eq!(ident.baro_alt, None);
    }

    #[test]
    fn surface_movement_scales() {
        assert_eq!(movement_v0(1), 0.0);
        assert_eq!(movement_v2(2), 0.0625);
        assert_eq!(movement_v0(124), 180.0);
        assert!((movement_v0(40) - 16.5).abs() < 1e-6);
        assert_eq!(movement_v0(125), 0.0);
    }

    #[test]
    fn short_and_zero_messages() {
        let mut d = decoder();
        assert_eq!(d.decode_bytes(&[0u8; 14]).decode_result, -2);
        assert_eq!(d.score(&hex("8D4840D6"), 32), -2);
        // Unsupported DF
        let mm = d.decode_bytes(&hex("08000000000000"));
        assert_eq!(mm.decode_result, -2);
    }
}
