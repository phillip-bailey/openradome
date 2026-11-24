//! `aircraft.binCraft`: the compact binary aircraft list that tar1090 and the API read.
//!
//! A port of upstream's `struct binCraft` and `toBinCraft` (aircraft.c, binCraftVersion 20250403):
//! little-endian, packed, 112 bytes per aircraft, and a header of the same size. All values are
//! fixed-point (altitude / 25, rates / 8, speeds x 10, and so on) rounded to nearest-even like C's
//! `nearbyint`. Field order and bit positions follow upstream's comments, byte for byte.

use crate::mode_s::{AirGround, NON_ICAO_ADDRESS};
use crate::track::{Aircraft, HOURS, TRACK_EXPIRE, TRACK_EXPIRE_JAERO, Tracker};

/// Upstream's `Modes.binCraftVersion`.
pub const VERSION: u32 = 20250403;
/// Size of one aircraft record, and of the header (without `TRACKS_UUID`).
pub const RECORD_SIZE: usize = 112;

const ALT_FACTOR: f32 = 1.0 / 25.0;

/// Round to nearest, ties to even, as C's `nearbyint` does in the default rounding mode.
fn ni(x: f64) -> f64 {
    x.round_ties_even()
}

fn i16c(x: f64) -> i16 {
    ni(x) as i16
}

fn u16c(x: f64) -> u16 {
    // C converts a double to uint16_t through int; negative values wrap.
    (ni(x) as i32) as u16
}

/// A little-endian packed bit writer for the record's bitfields: fields are laid out from the
/// least significant bit of each byte, in declaration order, as GCC does for a packed struct.
struct Bits<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Bits<'_> {
    fn put(&mut self, value: u32, width: usize) {
        for i in 0..width {
            if (value >> i) & 1 != 0 {
                self.buf[self.pos / 8] |= 1 << (self.pos % 8);
            }
            self.pos += 1;
        }
    }
}

/// One aircraft as a binCraft record (`toBinCraft`).
pub fn record(t: &Tracker, a: &Aircraft, now: i64, message_rate_mode: bool) -> [u8; RECORD_SIZE] {
    let mut r = [0u8; RECORD_SIZE];
    let cfg = &t.cfg;

    let callsign_valid = a.callsign_valid.valid();
    let position_valid = a.pos_reliable_valid.valid();

    // --- scalar part (offsets as in upstream's struct comments) ---
    let hex = a.addr;
    let seen = ni((now - a.seen) as f64 / 100.0) as i32;

    let (mut lat, mut lon, mut seen_pos, mut pos_nic, mut pos_rc) = (0i32, 0i32, 0i32, 0u8, 0u16);
    if position_valid || now < a.seen_pos_reliable + 14 * 24 * HOURS {
        seen_pos = ni((now - a.seen_pos_reliable) as f64 / 100.0) as i32;
        lat = ni(a.lat_reliable * 1e6) as i32;
        lon = ni(a.lon_reliable * 1e6) as i32;
        pos_nic = a.pos_nic_reliable as u8;
        pos_rc = a.pos_rc_reliable as u16;
    }

    let baro_alt_valid = a.alt_baro_reliable(cfg);
    let baro_alt = i16c((a.baro_alt as f32 * ALT_FACTOR) as f64);
    let mut geom_alt = i16c((a.geom_alt as f32 * ALT_FACTOR) as f64);
    let mut baro_rate = i16c(a.baro_rate as f64 / 8.0);
    let mut geom_rate = i16c(a.geom_rate as f64 / 8.0);
    let mut ias = a.ias as u16;
    let mut tas = a.tas as u16;
    let mut squawk = a.squawk as u16;
    let category = if now < a.category_updated + TRACK_EXPIRE_JAERO {
        a.category
    } else {
        0
    };
    let mut nav_altitude_mcp = u16c(a.nav_altitude_mcp as f64 / 4.0);
    let mut nav_altitude_fms = u16c(a.nav_altitude_fms as f64 / 4.0);
    let mut nav_qnh = i16c(a.nav_qnh as f64 * 10.0);
    let mut gs = i16c(a.gs as f64 * 10.0);
    let mut mach = i16c(a.mach * 1000.0);
    let mut track_rate = i16c(a.track_rate as f64 * 100.0);
    let mut roll = i16c(a.roll as f64 * 100.0);
    let mut track = if a.track_valid.valid() {
        i16c(a.track as f64 * 90.0)
    } else {
        i16c(a.calc_track * 90.0)
    };
    let mut mag_heading = i16c(a.mag_heading as f64 * 90.0);
    let mut true_heading = i16c(a.true_heading as f64 * 90.0);
    let mut nav_heading = i16c(a.nav_heading as f64 * 90.0);

    let mut emergency = a.emergency as u32;
    let airground = if a.airground_valid.valid() {
        a.airground as u32
    } else {
        AirGround::Invalid as u32
    };
    let addrtype = a.addrtype as u32;
    let mut nav_modes = a.nav_modes as u32;
    let mut nav_altitude_src = a.nav_altitude_src as u32;
    let sil_type = a.sil_type as u32;

    let wind_valid =
        now < a.wind_updated + TRACK_EXPIRE && (a.wind_altitude - a.baro_alt).abs() < 500;
    let wind_direction = if wind_valid {
        ni(a.wind_direction as f64) as i16
    } else {
        0
    };
    let wind_speed = if wind_valid {
        ni(a.wind_speed as f64) as i16
    } else {
        0
    };
    let temp_valid = now < a.oat_updated + TRACK_EXPIRE;
    let oat = if temp_valid { ni(a.oat) as i16 } else { 0 };
    let tat = if temp_valid { ni(a.tat) as i16 } else { 0 };

    let version = |v: i32| if v < 0 { 15 } else { v as u32 & 15 };

    let mut nic_a = a.nic_a;
    let mut nic_c = a.nic_c;
    let mut nic_baro = a.nic_baro;
    let mut nac_p = a.nac_p as u32;
    let mut nac_v = a.nac_v as u32;
    let mut sil = a.sil as u32;
    let mut gva = a.gva as u32;
    let mut sda = a.sda as u32;
    let mut alert = a.alert;
    let mut spi = a.spi;

    // upstream's F(): the _valid flag from the validity, and the value zeroed when invalid
    let geom_alt_valid = a.geom_alt_valid.valid();
    let gs_valid = a.gs_valid.valid();
    let ias_valid = a.ias_valid.valid();
    let tas_valid = a.tas_valid.valid();
    let mach_valid = a.mach_valid.valid();
    let track_valid = a.track_valid.valid();
    let track_rate_valid = a.track_rate_valid.valid();
    let roll_valid = a.roll_valid.valid();
    let mag_heading_valid = a.mag_heading_valid.valid();
    let true_heading_valid = a.true_heading_valid.valid();
    let baro_rate_valid = a.baro_rate_valid.valid();
    let geom_rate_valid = a.geom_rate_valid.valid();
    let nic_a_valid = a.nic_a_valid.valid();
    let nic_c_valid = a.nic_c_valid.valid();
    let nic_baro_valid = a.nic_baro_valid.valid();
    let nac_p_valid = a.nac_p_valid.valid();
    let nac_v_valid = a.nac_v_valid.valid();
    let sil_valid = a.sil_valid.valid();
    let gva_valid = a.gva_valid.valid();
    let sda_valid = a.sda_valid.valid();
    let squawk_valid = a.squawk_valid.valid();
    let emergency_valid = a.emergency_valid.valid();
    let nav_qnh_valid = a.nav_qnh_valid.valid();
    let nav_altitude_mcp_valid = a.nav_altitude_mcp_valid.valid();
    let nav_altitude_fms_valid = a.nav_altitude_fms_valid.valid();
    let nav_altitude_src_valid = a.nav_altitude_src_valid.valid();
    let nav_heading_valid = a.nav_heading_valid.valid();
    let nav_modes_valid = a.nav_modes_valid.valid();
    let alert_valid = a.alert_valid.valid();
    let spi_valid = a.spi_valid.valid();

    macro_rules! zero_unless {
        ($valid:expr => $($v:ident = $z:expr),+) => {
            if !$valid { $( $v = $z; )+ }
        };
    }
    zero_unless!(geom_alt_valid => geom_alt = 0);
    zero_unless!(gs_valid => gs = 0);
    zero_unless!(ias_valid => ias = 0);
    zero_unless!(tas_valid => tas = 0);
    zero_unless!(mach_valid => mach = 0);
    zero_unless!(track_valid => track = 0);
    zero_unless!(track_rate_valid => track_rate = 0);
    zero_unless!(roll_valid => roll = 0);
    zero_unless!(mag_heading_valid => mag_heading = 0);
    zero_unless!(true_heading_valid => true_heading = 0);
    zero_unless!(baro_rate_valid => baro_rate = 0);
    zero_unless!(geom_rate_valid => geom_rate = 0);
    zero_unless!(nic_a_valid => nic_a = false);
    zero_unless!(nic_c_valid => nic_c = false);
    zero_unless!(nic_baro_valid => nic_baro = false);
    zero_unless!(nac_p_valid => nac_p = 0);
    zero_unless!(nac_v_valid => nac_v = 0);
    zero_unless!(sil_valid => sil = 0);
    zero_unless!(gva_valid => gva = 0);
    zero_unless!(sda_valid => sda = 0);
    zero_unless!(squawk_valid => squawk = 0);
    zero_unless!(emergency_valid => emergency = 0);
    zero_unless!(nav_qnh_valid => nav_qnh = 0);
    zero_unless!(nav_altitude_mcp_valid => nav_altitude_mcp = 0);
    zero_unless!(nav_altitude_fms_valid => nav_altitude_fms = 0);
    zero_unless!(nav_altitude_src_valid => nav_altitude_src = 0);
    zero_unless!(nav_heading_valid => nav_heading = 0);
    zero_unless!(nav_modes_valid => nav_modes = 0);
    zero_unless!(alert_valid => alert = false);
    zero_unless!(spi_valid => spi = false);

    let messages: u16 = if message_rate_mode {
        // with the globe index or an API shutdown delay upstream sends the message rate x 10 here
        ni(10.0 * a.message_rate as f64) as u16
    } else {
        a.messages as u16
    };
    let signal = ni(((a.signal_db() + 50.0) * (255.0 / 50.0)) as f64).clamp(0.0, 255.0) as u8;

    // --- write ---
    r[0..4].copy_from_slice(&hex.to_le_bytes());
    r[4..8].copy_from_slice(&seen.to_le_bytes());
    r[8..12].copy_from_slice(&lon.to_le_bytes());
    r[12..16].copy_from_slice(&lat.to_le_bytes());
    r[16..18].copy_from_slice(&baro_rate.to_le_bytes());
    r[18..20].copy_from_slice(&geom_rate.to_le_bytes());
    r[20..22].copy_from_slice(&baro_alt.to_le_bytes());
    r[22..24].copy_from_slice(&geom_alt.to_le_bytes());
    r[24..26].copy_from_slice(&nav_altitude_mcp.to_le_bytes());
    r[26..28].copy_from_slice(&nav_altitude_fms.to_le_bytes());
    r[28..30].copy_from_slice(&nav_qnh.to_le_bytes());
    r[30..32].copy_from_slice(&nav_heading.to_le_bytes());
    r[32..34].copy_from_slice(&squawk.to_le_bytes());
    r[34..36].copy_from_slice(&gs.to_le_bytes());
    r[36..38].copy_from_slice(&mach.to_le_bytes());
    r[38..40].copy_from_slice(&roll.to_le_bytes());
    r[40..42].copy_from_slice(&track.to_le_bytes());
    r[42..44].copy_from_slice(&track_rate.to_le_bytes());
    r[44..46].copy_from_slice(&mag_heading.to_le_bytes());
    r[46..48].copy_from_slice(&true_heading.to_le_bytes());
    r[48..50].copy_from_slice(&wind_direction.to_le_bytes());
    r[50..52].copy_from_slice(&wind_speed.to_le_bytes());
    r[52..54].copy_from_slice(&oat.to_le_bytes());
    r[54..56].copy_from_slice(&tat.to_le_bytes());
    r[56..58].copy_from_slice(&tas.to_le_bytes());
    r[58..60].copy_from_slice(&ias.to_le_bytes());
    r[60..62].copy_from_slice(&pos_rc.to_le_bytes());
    r[62..64].copy_from_slice(&messages.to_le_bytes());

    {
        let mut b = Bits {
            buf: &mut r[64..78],
            pos: 0,
        };
        b.put(category as u32, 8);
        b.put(pos_nic as u32, 8);
        b.put(nav_modes, 8);
        b.put(emergency, 4);
        b.put(addrtype, 4);
        b.put(airground, 4);
        b.put(nav_altitude_src, 4);
        b.put(sil_type, 4);
        b.put(version(a.adsb_version), 4);
        b.put(version(a.adsr_version), 4);
        b.put(version(a.tisb_version), 4);
        b.put(nac_p, 4);
        b.put(nac_v, 4);
        b.put(sil, 2);
        b.put(gva, 2);
        b.put(sda, 2);
        b.put(nic_a as u32, 1);
        b.put(nic_c as u32, 1);
        for v in [
            nic_baro,
            alert,
            spi,
            callsign_valid,
            baro_alt_valid,
            geom_alt_valid,
            position_valid,
            gs_valid,
            ias_valid,
            tas_valid,
            mach_valid,
            track_valid,
            track_rate_valid,
            roll_valid,
            mag_heading_valid,
            true_heading_valid,
            baro_rate_valid,
            geom_rate_valid,
            nic_a_valid,
            nic_c_valid,
            nic_baro_valid,
            nac_p_valid,
            nac_v_valid,
            sil_valid,
            gva_valid,
            sda_valid,
            squawk_valid,
            emergency_valid,
            spi_valid,
            nav_qnh_valid,
            nav_altitude_mcp_valid,
            nav_altitude_fms_valid,
            nav_altitude_src_valid,
            nav_heading_valid,
            nav_modes_valid,
            alert_valid,
            wind_valid,
            temp_valid,
            false,
            false,
        ] {
            b.put(v as u32, 1);
        }
        debug_assert_eq!(b.pos, 14 * 8);
    }

    if callsign_valid {
        let cs = a.callsign.as_bytes();
        let n = cs.len().min(8);
        r[78..78 + n].copy_from_slice(&cs[..n]);
    }
    // 86..88 dbFlags, 88..92 typeCode, 92..104 registration: the aircraft database is not loaded
    // (upstream leaves them zero without --db-file).
    // 104 receiverCount (only with a Beast receiver-id feed), 105 signal, 106 extraFlags (nogps).
    r[105] = signal;
    r[108..112].copy_from_slice(&seen_pos.to_le_bytes());
    r
}

/// Bit offsets (from byte 64 of a record) of the fields the API filters on.
pub mod bit {
    pub const AIRGROUND: (usize, usize) = (32, 4);
    pub const CALLSIGN_VALID: usize = 75;
    pub const BARO_ALT_VALID: usize = 76;
    pub const POSITION_VALID: usize = 78;
    pub const SQUAWK_VALID: usize = 98;
}

/// Read `width` bits starting at `start` (counted from byte 64) from a record.
pub fn bits(rec: &[u8; RECORD_SIZE], start: usize, width: usize) -> u32 {
    (0..width).fold(0, |v, i| {
        let b = start + i;
        v | (((rec[64 + b / 8] >> (b % 8)) as u32 & 1) << i)
    })
}

/// A few fields of a record by name, for filters.
pub struct View<'a>(pub &'a [u8; RECORD_SIZE]);

impl View<'_> {
    pub fn hex(&self) -> u32 {
        u32::from_le_bytes(self.0[0..4].try_into().unwrap())
    }
    pub fn lon(&self) -> i32 {
        i32::from_le_bytes(self.0[8..12].try_into().unwrap())
    }
    pub fn lat(&self) -> i32 {
        i32::from_le_bytes(self.0[12..16].try_into().unwrap())
    }
    pub fn baro_alt(&self) -> i16 {
        i16::from_le_bytes(self.0[20..22].try_into().unwrap())
    }
    pub fn squawk(&self) -> u16 {
        u16::from_le_bytes(self.0[32..34].try_into().unwrap())
    }
    pub fn db_flags(&self) -> u16 {
        u16::from_le_bytes(self.0[86..88].try_into().unwrap())
    }
    pub fn flag(&self, bit: usize) -> bool {
        bits(self.0, bit, 1) != 0
    }
    pub fn airground(&self) -> u32 {
        bits(self.0, bit::AIRGROUND.0, bit::AIRGROUND.1)
    }
    /// The callsign without its padding.
    pub fn callsign(&self) -> &[u8] {
        trim(&self.0[78..86])
    }
    pub fn type_code(&self) -> &[u8] {
        trim(&self.0[88..92])
    }
    pub fn registration(&self) -> &[u8] {
        trim(&self.0[92..104])
    }
}

fn trim(b: &[u8]) -> &[u8] {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let b = &b[..end];
    let keep = b.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1);
    &b[..keep]
}

/// What the header carries besides the aircraft list.
pub struct Header {
    pub now: i64,
    pub aircraft_with_position: u32,
    pub message_count: u32,
    /// The receiver's position, degrees x 1e6; zero when unknown or hidden.
    pub receiver_lat: i32,
    pub receiver_lon: i32,
    /// Messages per second x 10.
    pub message_rate: u32,
    /// Bit 0: the file comes from a decoder with the globe index or API.
    pub flags: u32,
    /// Globe tile index (`aircraft.binCraft` writes 314159) and its bounds in degrees.
    pub index: u32,
    pub bounds: [i16; 4],
}

impl Header {
    pub fn to_bytes(&self) -> [u8; RECORD_SIZE] {
        let mut h = [0u8; RECORD_SIZE];
        h[0..8].copy_from_slice(&self.now.to_le_bytes());
        h[8..12].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
        h[12..16].copy_from_slice(&self.aircraft_with_position.to_le_bytes());
        h[16..20].copy_from_slice(&self.index.to_le_bytes());
        for (i, b) in self.bounds.iter().enumerate() {
            h[20 + 2 * i..22 + 2 * i].copy_from_slice(&b.to_le_bytes());
        }
        h[28..32].copy_from_slice(&self.message_count.to_le_bytes());
        h[32..36].copy_from_slice(&self.receiver_lat.to_le_bytes());
        h[36..40].copy_from_slice(&self.receiver_lon.to_le_bytes());
        h[40..44].copy_from_slice(&VERSION.to_le_bytes());
        h[44..48].copy_from_slice(&self.message_rate.to_le_bytes());
        h[48..52].copy_from_slice(&self.flags.to_le_bytes());
        h
    }
}

/// One globe tile's file, `globe_XXXX.binCraft` (`generateGlobeBin`), uncompressed. `mil` keeps only
/// aircraft flagged military by the database (none without one).
pub fn globe_bin(t: &Tracker, index: i32, mil: bool, now: i64, message_rate: f64) -> Vec<u8> {
    let h = globe_header(
        index,
        now,
        t.counts(now).with_position,
        t.stats.messages_total as u32,
        message_rate,
    );
    let mut out = h.to_vec();
    for a in t.active() {
        if a.globe_index != index || !Tracker::include_in_json(now, a) || mil {
            continue;
        }
        out.extend_from_slice(&record(t, a, now, true));
    }
    out
}

/// A tile file with no aircraft (written once at start so every tile exists).
pub fn globe_bin_empty(index: i32) -> Vec<u8> {
    globe_header(index, 0, 0, 0, 0.0).to_vec()
}

fn globe_header(
    index: i32,
    now: i64,
    with_pos: u32,
    messages: u32,
    message_rate: f64,
) -> [u8; RECORD_SIZE] {
    let (south, west, north, east) = if index >= 0 {
        crate::globe::bounds(index)
    } else {
        (-90, -180, 90, 180)
    };
    let mut h = [0u8; RECORD_SIZE];
    h[0..8].copy_from_slice(&now.to_le_bytes());
    h[8..12].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
    h[12..16].copy_from_slice(&with_pos.to_le_bytes());
    let idx: u32 = if index < 0 { 42777 } else { index as u32 };
    h[16..20].copy_from_slice(&idx.to_le_bytes());
    for (i, b) in [south, west, north, east].iter().enumerate() {
        h[20 + 2 * i..22 + 2 * i].copy_from_slice(&b.to_le_bytes());
    }
    h[28..32].copy_from_slice(&messages.to_le_bytes());
    // 32..40: two dummies, unlike aircraft.binCraft
    h[40..44].copy_from_slice(&VERSION.to_le_bytes());
    h[44..48].copy_from_slice(&(ni(message_rate * 10.0) as u32).to_le_bytes());
    h[48..52].copy_from_slice(&1u32.to_le_bytes());
    h
}

/// The whole `aircraft.binCraft` file (`generateAircraftBin`), uncompressed.
pub fn aircraft_bin(t: &Tracker, now: i64, message_rate: f64, hide_receiver: bool) -> Vec<u8> {
    let (rlat, rlon) = match t.cfg.user_location {
        Some((lat, lon)) if !hide_receiver => (ni(lat * 1e6) as i32, ni(lon * 1e6) as i32),
        _ => (0, 0),
    };
    let header = Header {
        now,
        aircraft_with_position: t.counts(now).with_position,
        message_count: t.stats.messages_total as u32,
        receiver_lat: rlat,
        receiver_lon: rlon,
        message_rate: ni(message_rate * 10.0) as u32,
        flags: t.cfg.message_rate_mode as u32,
        index: 314159,
        bounds: [-90, -180, 90, 180],
    };
    let mut out = Vec::with_capacity(RECORD_SIZE * (t.active().count() + 1));
    out.extend_from_slice(&header.to_bytes());
    for a in t.active() {
        if Tracker::include_in_json(now, a) {
            out.extend_from_slice(&record(t, a, now, t.cfg.message_rate_mode));
        }
    }
    out
}

/// Is `a` a non-ICAO (`~`-prefixed) address? binCraft carries it in the hex field's top bits.
pub fn is_non_icao(addr: u32) -> bool {
    addr & NON_ICAO_ADDRESS != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearbyint_rounds_half_to_even() {
        assert_eq!(ni(0.5), 0.0);
        assert_eq!(ni(1.5), 2.0);
        assert_eq!(ni(2.5), 2.0);
        assert_eq!(ni(-0.5), 0.0);
    }

    #[test]
    fn bits_pack_from_the_low_bit() {
        let mut buf = [0u8; 2];
        let mut b = Bits {
            buf: &mut buf,
            pos: 0,
        };
        b.put(0b101, 3); // bits 0..3
        b.put(0b1, 1); // bit 3
        b.put(0xF, 4); // bits 4..8
        b.put(1, 1); // bit 8 -> byte 1
        assert_eq!(buf, [0b1111_1101, 0b0000_0001]);
    }

    #[test]
    fn header_layout() {
        let h = Header {
            now: 0x0102030405060708,
            aircraft_with_position: 7,
            message_count: 9,
            receiver_lat: -5,
            receiver_lon: 6,
            message_rate: 11,
            flags: 1,
            index: 314159,
            bounds: [-90, -180, 90, 180],
        }
        .to_bytes();
        assert_eq!(h.len(), 112);
        assert_eq!(&h[0..8], &0x0102030405060708i64.to_le_bytes());
        assert_eq!(&h[8..12], &112u32.to_le_bytes());
        assert_eq!(&h[40..44], &20250403u32.to_le_bytes());
        assert_eq!(&h[20..22], &(-90i16).to_le_bytes());
        assert!(h[52..].iter().all(|&b| b == 0));
    }
}
