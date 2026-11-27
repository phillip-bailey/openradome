//! Aircraft tracking. Port of the core of upstream `track.c` / `aircraft.c`.
//!
//! Every tracked field carries a [`Validity`]: the source it came from, when it arrived
//! and whether it is stale. A new value is only accepted from a source at least as good as
//! the one that supplied the current value, unless the current value is old enough
//! (`will_accept`). Positions are decoded from CPR pairs (or a single frame relative to a
//! known position), checked for plausible speed and range, and only reported once they
//! are "reliable" (enough consistent decodes).
//!
//! Not ported yet: wind and temperature estimation, magnetic declination (so
//! `true_heading` is not derived from magnetic heading), traces and heatmaps, receiver
//! position inference (`receiver.c`; give `--lat/--lon` instead), Mode A/C matching to
//! aircraft, JAERO/SBS sources, per-receiver bookkeeping for network ingest.

use std::collections::{HashMap, HashSet};

use crate::cpr;
use crate::geo::{
    KNOTS_TO_METERS_PER_SECOND, bearing, bogus_lat_lon, greatcircle, norm_angle, norm_diff,
};
use crate::geomag;
use crate::mode_ac::mode_a_to_index;
use crate::mode_s::{
    AddrType, AirGround, CprType, DFTYPE_MODEAC, DataSource, Emergency, HeadingType, ModesMessage,
    NON_ICAO_ADDRESS, NavAltitudeSource, SilType,
};

pub const SECONDS: i64 = 1000;
pub const MINUTES: i64 = 60 * SECONDS;
pub const HOURS: i64 = 60 * MINUTES;

pub const TRACK_STALE: i64 = 15 * SECONDS;
pub const TRACK_EXPIRE: i64 = 60 * SECONDS;
pub const TRACK_EXPIRE_LONG: i64 = 180 * SECONDS;
pub const TRACK_EXPIRE_JAERO: i64 = 33 * MINUTES;
pub const POS_RELIABLE_TIMEOUT: i64 = 60 * MINUTES;
pub const ALTITUDE_BARO_RELIABLE_MAX: i32 = 20;
/// Interval between stale-removal passes.
pub const REMOVE_STALE_INTERVAL: i64 = SECONDS;
/// A trace point is marked stale when the previous reliable position is older than this.
pub const TRACE_STALE: i64 = 15 * SECONDS;
/// How long traces are kept in memory (24 h plus an hour of overlap).
pub const KEEP_TRACES: i64 = 24 * HOURS + 60 * MINUTES;

const CPR_CACHE: usize = 4;
const DISCARD_CACHE: usize = 4;

/// Where a value came from and how fresh it is.
#[derive(Debug, Clone, Copy, Default)]
pub struct Validity {
    /// When it arrived (ms).
    pub updated: i64,
    pub source: DataSource,
    pub last_source: DataSource,
    pub stale: bool,
    /// When this field may next flag a message for `beast_reduce` output (ms).
    pub next_reduce_forward: i64,
}

/// How often a field's updates are forwarded on `beast_reduce` outputs, relative to
/// `--net-beast-reduce-interval` (upstream's REDUCE_RARE / REDUCE_OFTEN / REDUCE_DOUBLE).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reduce {
    /// 4x the interval: fields that rarely change (squawk, callsign, capabilities).
    Rare,
    /// 7/8 of the interval: measurements (altitude, speed, heading).
    Often,
    /// 3/8 of the interval: positions.
    Double,
}

impl Validity {
    pub fn valid(&self) -> bool {
        self.source != DataSource::Invalid
    }

    /// Age in ms.
    pub fn age(&self, now: i64) -> i64 {
        if self.updated >= now {
            0
        } else {
            now - self.updated
        }
    }

    fn update(&mut self, now: i64, expiration: i64) {
        if self.source == DataSource::Invalid {
            return;
        }
        self.stale = now > self.updated + TRACK_STALE;
        if self.source == DataSource::Jaero {
            if now > self.updated + TRACK_EXPIRE_JAERO {
                self.source = DataSource::Invalid;
            }
        } else if now > self.updated + expiration {
            self.source = DataSource::Invalid;
        }
    }
}

/// Should data from `source` replace what `d` holds?
fn will_accept(d: &Validity, source: DataSource, now: i64, seen: i64) -> bool {
    if source == DataSource::Invalid || now < d.updated {
        return false;
    }
    // A worse source only gets in once a reliable-CRC message has been seen TRACK_STALE
    // after the better data arrived.
    !(source < d.source && seen < d.updated + TRACK_STALE)
}

/// Accept `mm`'s value for `d` if its source may replace what is there. On acceptance the
/// message is flagged for `beast_reduce` output when this field's share of the reduce
/// `interval` (ms) has passed, as upstream's `accept_data` does.
fn accept(
    d: &mut Validity,
    source: DataSource,
    mm: &mut ModesMessage,
    seen: i64,
    reduce: Reduce,
    interval: i64,
) -> bool {
    let now = mm.sys_timestamp;
    if !will_accept(d, source, now, seen) {
        return false;
    }
    d.source = if source == DataSource::Prio {
        DataSource::Adsb
    } else {
        source
    };
    d.last_source = d.source;
    d.updated = now;
    d.stale = false;
    if now > d.next_reduce_forward || mm.reduce_forward {
        let mut iv = match reduce {
            Reduce::Double => interval * 3 / 8,
            Reduce::Often => interval * 7 / 8,
            Reduce::Rare => interval * 4,
        };
        // keep global CPR possible even at a long interval
        if mm.cpr.is_some() && iv > 7000 {
            iv = 7000;
        }
        d.next_reduce_forward = now + iv;
        mm.reduce_forward = true;
    }
    true
}

/// Combine two validities for derived data: the worse source, the later time.
fn combine_validity(to: &mut Validity, from1: &Validity, from2: &Validity, now: i64) {
    if from1.source == DataSource::Invalid {
        *to = *from2;
        return;
    }
    if from2.source == DataSource::Invalid {
        *to = *from1;
        return;
    }
    to.source = from1.source.min(from2.source);
    to.last_source = to.source;
    to.updated = from1.updated.max(from2.updated);
    to.stale = now > to.updated + TRACK_STALE;
}

fn compare_validity(lhs: &Validity, rhs: &Validity) -> i32 {
    if !lhs.stale && lhs.source > rhs.source {
        1
    } else if !rhs.stale && lhs.source < rhs.source {
        -1
    } else if lhs.updated >= rhs.updated {
        1
    } else {
        -1
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct CprCache {
    cpr_lat: u32,
    cpr_lon: u32,
    ts: i64,
    receiver_id: u64,
}

/// One tracked aircraft.
#[derive(Debug, Clone)]
pub struct Aircraft {
    pub addr: u32,
    pub addrtype: AddrType,
    pub addrtype_updated: i64,
    /// Last reliable-address message (ms).
    pub seen: i64,
    /// Last position (ms).
    pub seen_pos: i64,
    pub messages: u32,
    pub category: u8,
    pub category_updated: i64,

    pub signal_level: [f32; 8],
    pub signal_next: u32,

    pub callsign: String,
    pub callsign_valid: Validity,
    pub baro_alt: i32,
    pub baro_alt_valid: Validity,
    pub alt_reliable: i32,
    pub geom_alt: i32,
    pub geom_alt_valid: Validity,
    pub geom_delta: i32,
    pub geom_delta_valid: Validity,
    pub gs: f32,
    pub gs_last_pos: f32,
    pub gs_reliable: f32,
    pub gs_valid: Validity,
    pub ias: u32,
    pub ias_valid: Validity,
    pub tas: u32,
    pub tas_valid: Validity,
    pub mach: f64,
    pub mach_valid: Validity,
    pub roll: f32,
    pub roll_valid: Validity,
    pub track_rate: f32,
    pub track_rate_valid: Validity,
    pub track: f32,
    pub track_valid: Validity,
    pub mag_heading: f32,
    pub mag_heading_valid: Validity,
    pub true_heading: f32,
    pub true_heading_valid: Validity,
    pub baro_rate: i32,
    pub baro_rate_valid: Validity,
    pub geom_rate: i32,
    pub geom_rate_valid: Validity,
    pub squawk: u32,
    pub squawk_valid: Validity,
    squawk_tentative: u32,
    squawk_tentative_changed: i64,
    /// `beast_reduce` pacing for status messages and DF11 all-call replies.
    next_reduce_forward_status: i64,
    next_reduce_forward_df11: i64,
    pub emergency: Emergency,
    pub emergency_valid: Validity,
    pub airground: AirGround,
    pub airground_valid: Validity,
    pub nav_altitude_mcp: u32,
    pub nav_altitude_mcp_valid: Validity,
    pub nav_altitude_fms: u32,
    pub nav_altitude_fms_valid: Validity,
    pub nav_altitude_src: NavAltitudeSource,
    pub nav_altitude_src_valid: Validity,
    pub nav_heading: f32,
    pub nav_heading_valid: Validity,
    pub nav_modes: u8,
    pub nav_modes_valid: Validity,
    pub nav_qnh: f32,
    pub nav_qnh_valid: Validity,
    pub alert: bool,
    pub alert_valid: Validity,
    pub spi: bool,
    pub spi_valid: Validity,
    pub nic_a: bool,
    pub nic_a_valid: Validity,
    pub nic_c: bool,
    pub nic_c_valid: Validity,
    pub nic_baro: bool,
    pub nic_baro_valid: Validity,
    pub nac_p: u8,
    pub nac_p_valid: Validity,
    pub nac_v: u8,
    pub nac_v_valid: Validity,
    pub sil: u8,
    pub sil_type: SilType,
    pub sil_valid: Validity,
    pub gva: u8,
    pub gva_valid: Validity,
    pub sda: u8,
    pub sda_valid: Validity,
    /// ADS-B version from operational status; -1 until an ADS-B message is seen.
    pub adsb_version: i32,
    pub adsr_version: i32,
    pub tisb_version: i32,
    adsb_hrd: HeadingType,
    adsb_tah: HeadingType,

    // CPR
    pub cpr_odd_valid: Validity,
    pub cpr_even_valid: Validity,
    cpr_odd_lat: u32,
    cpr_odd_lon: u32,
    cpr_odd_type: CprType,
    cpr_odd_nic: u32,
    cpr_odd_rc: u32,
    cpr_even_lat: u32,
    cpr_even_lon: u32,
    cpr_even_type: CprType,
    cpr_even_nic: u32,
    cpr_even_rc: u32,
    last_cpr_type: CprType,
    cpr_cache: [CprCache; CPR_CACHE],
    cpr_cache_index: usize,
    disc_cache: [CprCache; DISCARD_CACHE],
    disc_cache_index: usize,

    // Position
    pub lat: f64,
    pub lon: f64,
    prev_lat: f64,
    prev_lon: f64,
    prev_pos_time: i64,
    pub pos_nic: u32,
    pub pos_rc: u32,
    pub position_valid: Validity,
    pub pos_reliable_valid: Validity,
    pub pos_reliable_odd: f32,
    pub pos_reliable_even: f32,
    pub seen_pos_reliable: i64,
    pub lat_reliable: f64,
    pub lon_reliable: f64,
    pub pos_nic_reliable: u32,
    pub pos_rc_reliable: u32,
    pos_surface: bool,
    local_cpr_allow_ac_rel: bool,
    surface_cpr_allow_ac_rel: bool,
    speed_unreliable: i32,
    track_unreliable: i32,
    receiver_id: u64,
    pub calc_track: f64,
    pub receiver_distance: f64,
    pub receiver_direction: f64,
    pub acas_ra: bool,
    /// Estimated wind (knots, degrees from) and the altitude it applies to.
    pub wind_speed: f32,
    pub wind_direction: f32,
    pub wind_updated: i64,
    pub wind_altitude: i32,
    /// Outside air and total air temperature in Celsius.
    pub oat: f64,
    pub tat: f64,
    pub oat_updated: i64,
    pub tat_updated: i64,
    magnetic_declination: f64,
    updated_declination: i64,
    /// Globe tile this aircraft is listed in (-5 = none), see [`crate::globe`].
    pub globe_index: i32,
    /// Messages per second, from the last two seconds (upstream's per-aircraft `messageRate`).
    pub message_rate: f32,
    message_rate_acc: [u16; 2],
    next_message_rate_calc: i64,
    /// When the air/ground state last changed.
    pub last_air_ground_change: i64,
    /// The recorded track.
    pub trace: crate::trace::Trace,
}

impl Aircraft {
    fn new(addr: u32) -> Self {
        let v = Validity::default;
        Self {
            addr,
            addrtype: AddrType::Unknown,
            addrtype_updated: 0,
            seen: 0,
            seen_pos: 0,
            messages: 0,
            category: 0,
            category_updated: 0,
            signal_level: [0.0; 8],
            signal_next: 0,
            callsign: String::new(),
            callsign_valid: v(),
            baro_alt: 0,
            baro_alt_valid: v(),
            alt_reliable: 0,
            geom_alt: 0,
            geom_alt_valid: v(),
            geom_delta: 0,
            geom_delta_valid: v(),
            gs: 0.0,
            gs_last_pos: 0.0,
            gs_reliable: 0.0,
            gs_valid: v(),
            ias: 0,
            ias_valid: v(),
            tas: 0,
            tas_valid: v(),
            mach: 0.0,
            mach_valid: v(),
            roll: 0.0,
            roll_valid: v(),
            track_rate: 0.0,
            track_rate_valid: v(),
            track: 0.0,
            track_valid: v(),
            mag_heading: 0.0,
            mag_heading_valid: v(),
            true_heading: 0.0,
            true_heading_valid: v(),
            baro_rate: 0,
            baro_rate_valid: v(),
            geom_rate: 0,
            geom_rate_valid: v(),
            squawk: 0,
            squawk_valid: v(),
            squawk_tentative: 0,
            squawk_tentative_changed: 0,
            next_reduce_forward_status: 0,
            next_reduce_forward_df11: 0,
            emergency: Emergency::None,
            emergency_valid: v(),
            airground: AirGround::Invalid,
            airground_valid: v(),
            nav_altitude_mcp: 0,
            nav_altitude_mcp_valid: v(),
            nav_altitude_fms: 0,
            nav_altitude_fms_valid: v(),
            nav_altitude_src: NavAltitudeSource::Invalid,
            nav_altitude_src_valid: v(),
            nav_heading: 0.0,
            nav_heading_valid: v(),
            nav_modes: 0,
            nav_modes_valid: v(),
            nav_qnh: 0.0,
            nav_qnh_valid: v(),
            alert: false,
            alert_valid: v(),
            spi: false,
            spi_valid: v(),
            nic_a: false,
            nic_a_valid: v(),
            nic_c: false,
            nic_c_valid: v(),
            nic_baro: false,
            nic_baro_valid: v(),
            nac_p: 0,
            nac_p_valid: v(),
            nac_v: 0,
            nac_v_valid: v(),
            sil: 0,
            sil_type: SilType::Invalid,
            sil_valid: v(),
            gva: 0,
            gva_valid: v(),
            sda: 0,
            sda_valid: v(),
            adsb_version: -1,
            adsr_version: 0,
            tisb_version: 0,
            adsb_hrd: HeadingType::Magnetic,
            adsb_tah: HeadingType::GroundTrack,
            cpr_odd_valid: v(),
            cpr_even_valid: v(),
            cpr_odd_lat: 0,
            cpr_odd_lon: 0,
            cpr_odd_type: CprType::Invalid,
            cpr_odd_nic: 0,
            cpr_odd_rc: 0,
            cpr_even_lat: 0,
            cpr_even_lon: 0,
            cpr_even_type: CprType::Invalid,
            cpr_even_nic: 0,
            cpr_even_rc: 0,
            last_cpr_type: CprType::Invalid,
            cpr_cache: [CprCache::default(); CPR_CACHE],
            cpr_cache_index: 0,
            disc_cache: [CprCache::default(); DISCARD_CACHE],
            disc_cache_index: 0,
            lat: 0.0,
            lon: 0.0,
            prev_lat: 0.0,
            prev_lon: 0.0,
            prev_pos_time: 0,
            pos_nic: 0,
            pos_rc: 0,
            position_valid: v(),
            pos_reliable_valid: v(),
            pos_reliable_odd: 0.0,
            pos_reliable_even: 0.0,
            seen_pos_reliable: 0,
            lat_reliable: 0.0,
            lon_reliable: 0.0,
            pos_nic_reliable: 0,
            pos_rc_reliable: 0,
            pos_surface: false,
            local_cpr_allow_ac_rel: false,
            surface_cpr_allow_ac_rel: false,
            speed_unreliable: 0,
            track_unreliable: 0,
            receiver_id: 0,
            calc_track: 0.0,
            receiver_distance: 0.0,
            receiver_direction: 0.0,
            acas_ra: false,
            wind_speed: 0.0,
            wind_direction: 0.0,
            wind_updated: 0,
            wind_altitude: 0,
            oat: 0.0,
            tat: 0.0,
            oat_updated: 0,
            tat_updated: 0,
            magnetic_declination: 0.0,
            updated_declination: 0,
            globe_index: -5,
            message_rate: 0.0,
            message_rate_acc: [0; 2],
            next_message_rate_calc: 0,
            last_air_ground_change: 0,
            trace: crate::trace::Trace::default(),
        }
    }

    /// Is the barometric altitude trustworthy enough to publish?
    pub fn alt_baro_reliable(&self, cfg: &TrackerConfig) -> bool {
        self.baro_alt_valid.valid()
            && (self.position_valid.source == DataSource::Jaero
                || self.alt_reliable >= 2 * cfg.json_reliable)
    }

    /// Has the position been confirmed by enough consistent decodes?
    pub fn pos_reliable(&self, cfg: &TrackerConfig) -> bool {
        if !self.position_valid.valid() {
            return false;
        }
        if matches!(
            self.position_valid.source,
            DataSource::Jaero | DataSource::Mlat | DataSource::Indirect
        ) {
            return true;
        }
        let reliable = cfg.json_reliable as f32;
        self.pos_reliable_odd >= reliable && self.pos_reliable_even >= reliable
    }

    /// Averaged recent signal level (linear power, 0..1), 0 if too few samples.
    pub fn signal_raw(&self) -> f32 {
        if self.signal_next >= 8 {
            self.signal_level.iter().sum::<f32>() / 8.0
        } else if self.signal_next >= 4 {
            let n = self.signal_next as usize;
            self.signal_level[..n].iter().sum::<f32>() / self.signal_next as f32
        } else {
            0.0
        }
    }

    /// Signal in dBFS as shown in `aircraft.json` (`rssi`).
    pub fn signal_db(&self) -> f32 {
        10.0 * (self.signal_raw() + 1.125e-5).log10()
    }

    /// Upstream's `calculateMessageRate`: a decaying average over the last two intervals.
    fn calculate_message_rate(&mut self, now: i64) {
        let mut sum = 0.0f32;
        let mut mult = REMOVE_STALE_INTERVAL as f32 / 1000.0;
        let mut mult_sum = 0.0f32;
        for acc in self.message_rate_acc {
            sum += acc as f32 * mult;
            mult_sum += mult;
            mult *= 0.7;
        }
        self.message_rate = sum / mult_sum;
        self.next_message_rate_calc = now + REMOVE_STALE_INTERVAL;
        self.message_rate_acc = [0, self.message_rate_acc[0]];
    }

    fn update_validities(&mut self, now: i64) {
        let elapsed_pos = now - self.seen_pos;
        // a tile listing ends when the position has been stale for the longest expiry
        if self.globe_index >= 0 && elapsed_pos > TRACK_EXPIRE_JAERO + TRACK_EXPIRE_LONG + MINUTES {
            self.globe_index = -5;
        }
        if self.category != 0
            && now > self.category_updated + TRACK_EXPIRE_JAERO + TRACK_EXPIRE_LONG + MINUTES
        {
            self.category = 0;
        }
        if self.pos_reliable_odd != 0.0
            && self.pos_reliable_even != 0.0
            && elapsed_pos > POS_RELIABLE_TIMEOUT
        {
            self.pos_reliable_odd = 0.0;
            self.pos_reliable_even = 0.0;
        }
        self.baro_alt_valid.update(now, TRACK_EXPIRE);
        if self.alt_reliable != 0 && !self.baro_alt_valid.valid() {
            self.alt_reliable = 0;
        }
        self.callsign_valid.update(now, TRACK_EXPIRE_LONG);
        for v in [
            &mut self.geom_alt_valid,
            &mut self.geom_delta_valid,
            &mut self.gs_valid,
            &mut self.ias_valid,
            &mut self.tas_valid,
            &mut self.mach_valid,
            &mut self.roll_valid,
            &mut self.track_rate_valid,
            &mut self.track_valid,
            &mut self.mag_heading_valid,
            &mut self.true_heading_valid,
            &mut self.baro_rate_valid,
            &mut self.geom_rate_valid,
            &mut self.nic_a_valid,
            &mut self.nic_c_valid,
            &mut self.nic_baro_valid,
            &mut self.nac_p_valid,
            &mut self.nac_v_valid,
            &mut self.sil_valid,
            &mut self.gva_valid,
            &mut self.sda_valid,
            &mut self.squawk_valid,
            &mut self.emergency_valid,
        ] {
            v.update(now, TRACK_EXPIRE);
        }
        self.airground_valid.update(now, TRACK_EXPIRE_LONG);
        for v in [
            &mut self.nav_qnh_valid,
            &mut self.nav_altitude_mcp_valid,
            &mut self.nav_altitude_fms_valid,
            &mut self.nav_altitude_src_valid,
            &mut self.nav_heading_valid,
            &mut self.nav_modes_valid,
            &mut self.cpr_odd_valid,
            &mut self.cpr_even_valid,
            &mut self.position_valid,
            &mut self.alert_valid,
            &mut self.spi_valid,
            &mut self.pos_reliable_valid,
        ] {
            v.update(now, TRACK_EXPIRE);
        }
    }
}

/// Number of aircraft currently listed, for `stats.json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AircraftCounts {
    pub with_position: u32,
    pub without_position: u32,
    /// Indexed by `AddrType as usize`.
    pub by_type: [u32; 14],
}

/// Counters, as printed by `--stats`. Names follow upstream's `struct stats`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackStats {
    pub messages_total: u64,
    pub cpr_surface: u64,
    pub cpr_airborne: u64,
    pub cpr_global_ok: u64,
    pub cpr_global_bad: u64,
    pub cpr_global_range_checks: u64,
    pub cpr_global_speed_checks: u64,
    pub cpr_global_skipped: u64,
    pub cpr_local_ok: u64,
    pub cpr_local_aircraft_relative: u64,
    pub cpr_local_receiver_relative: u64,
    pub cpr_local_skipped: u64,
    pub cpr_local_range_checks: u64,
    pub cpr_local_speed_checks: u64,
    pub cpr_filtered: u64,
    pub suppressed_altitude_messages: u64,
    pub unique_aircraft: u64,
    pub single_message_aircraft: u64,
    /// Positions accepted, in total and by address type (`AddrType as usize`).
    pub pos_all: u64,
    pub pos_by_type: [u64; 14],
    pub pos_duplicate: u64,
    pub pos_garbage: u64,
    /// Furthest distance (metres) of any reliable position from the receiver.
    pub distance_max: u64,
    /// The same, since the stats recorder last closed a 10 s bucket (it resets this), so each
    /// `stats.json` period reports its own furthest position, as upstream's do.
    pub distance_max_bucket: u64,
}

/// Tracker settings (the relevant subset of upstream's `Modes`).
#[derive(Debug, Clone)]
pub struct TrackerConfig {
    /// Receiver position, if configured (`--lat/--lon`).
    pub user_location: Option<(f64, f64)>,
    /// Receiver position is usable as a CPR reference.
    pub user_location_ref: bool,
    /// Absolute maximum range for position decoding, metres (`--max-range`, default 450 nmi).
    pub max_range: f64,
    /// Minimum position reliability to report (upstream `json_reliable`).
    pub json_reliable: i32,
    /// Position persistence against outliers (`--position-persistence`).
    pub position_persistence: i32,
    /// `beast_reduce` update interval in ms (`--net-beast-reduce-interval`, default 250).
    pub beast_reduce_interval: i64,
    /// Keep traces (`--write-json-globe-index` or `--write-globe-history`).
    pub write_traces: bool,
    /// Assign aircraft to globe tiles (`--write-json-globe-index`).
    pub globe_index: bool,
    /// How traces are recorded.
    pub trace: crate::trace::TraceSettings,
    /// binCraft `messages` carries the message rate x 10 and header flag 1 is set (upstream: the
    /// globe index or `--api-shutdown-delay`).
    pub message_rate_mode: bool,
}

impl TrackerConfig {
    /// Derive the same defaults upstream does from `--lat`, `--lon` and `--max-range` (nmi).
    pub fn new(lat: Option<f64>, lon: Option<f64>, max_range_nmi: Option<f64>) -> Self {
        let (lat, lon) = (lat.unwrap_or(0.0), lon.unwrap_or(0.0));
        let (mut lat, mut lon) = (lat, lon);
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=360.0).contains(&lon) {
            lat = 0.0;
            lon = 0.0;
        } else if lon > 180.0 {
            lon -= 360.0;
        }
        let valid = lat != 0.0 || lon != 0.0;
        let max_range = max_range_nmi.map_or(1852.0 * 450.0, |n| n * 1852.0);
        let json_reliable = if valid && max_range != 0.0 { 1 } else { 2 };
        let user_location_ref = valid && max_range != 0.0 && max_range < 1852.0 * 800.0;
        Self {
            user_location: valid.then_some((lat, lon)),
            user_location_ref,
            max_range,
            json_reliable,
            position_persistence: 4.max(json_reliable),
            beast_reduce_interval: 250,
            write_traces: false,
            globe_index: false,
            trace: crate::trace::TraceSettings::default(),
            message_rate_mode: false,
        }
    }
}

impl Default for TrackerConfig {
    fn default() -> Self {
        Self::new(None, None, None)
    }
}

/// Directions (1 degree each) and time slices of the range outline, as upstream.
pub const RANGEDIRS_BUCKETS: usize = 360;
pub const RANGEDIRS_IVALS: usize = 64;

/// The farthest position heard in one direction during one time slice.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DistCoords {
    /// metres
    pub distance: f32,
    pub lat: f32,
    pub lon: f32,
    /// feet
    pub alt: i32,
}

/// The receiver's actual range per direction over the last `--range-outline-hours`
/// (upstream's rangeDirs): 360 directions times 64 time slices; the slice being written is
/// cleared when time moves into it, so the outline covers a rolling window.
#[derive(Debug, Clone)]
pub struct RangeOutline {
    duration: i64,
    dirs: Vec<[DistCoords; RANGEDIRS_BUCKETS]>,
    /// The newest slice so far, counted from the epoch (not wrapped).
    newest: Option<i64>,
}

impl RangeOutline {
    /// `duration_ms` is the window; upstream's minimum is one minute.
    pub fn new(duration_ms: i64) -> Self {
        Self {
            duration: duration_ms.max(MINUTES),
            dirs: vec![[DistCoords::default(); RANGEDIRS_BUCKETS]; RANGEDIRS_IVALS],
            newest: None,
        }
    }

    /// The time slice for `now` (upstream's mapping), clearing slices as time moves into them.
    /// The periodic tick and message times come from slightly different clocks, so a message up
    /// to one slice behind the newest is written to its own slice without clearing anything.
    /// A larger step backwards (the system clock was set back) starts over from that slice,
    /// as upstream's clear-on-change does.
    pub fn rotate(&mut self, now: i64) -> usize {
        let abs = (now * (RANGEDIRS_IVALS as i64 - 1)).div_euclid(self.duration);
        match self.newest {
            Some(newest) if abs > newest => {
                // clear every slice time has moved into, at most one full round
                for k in (newest + 1..=abs).take(RANGEDIRS_IVALS) {
                    self.dirs[k.rem_euclid(RANGEDIRS_IVALS as i64) as usize] =
                        [DistCoords::default(); RANGEDIRS_BUCKETS];
                }
                self.newest = Some(abs);
            }
            Some(newest) if abs >= newest - 1 => {}
            Some(_) => {
                self.dirs[abs.rem_euclid(RANGEDIRS_IVALS as i64) as usize] =
                    [DistCoords::default(); RANGEDIRS_BUCKETS];
                self.newest = Some(abs);
            }
            None => self.newest = Some(abs),
        }
        abs.rem_euclid(RANGEDIRS_IVALS as i64) as usize
    }

    /// Record `a`'s position (receiver distance and direction already set) if it is the
    /// farthest in its direction for the current slice.
    pub fn update(&mut self, a: &Aircraft, now: i64, json_reliable: i32) {
        let ival = self.rotate(now);
        let range = a.receiver_distance;
        let dir = (a.receiver_direction.round_ties_even() as i64)
            .rem_euclid(RANGEDIRS_BUCKETS as i64) as usize;
        let current = self.dirs[ival][dir];
        // a position that is not fully reliable may extend the range in its direction by
        // at most 50 nmi over the maximum of the whole window
        if range > f64::from(current.distance)
            && (a.pos_reliable_odd < 2.0 || a.pos_reliable_even < 2.0)
        {
            let max = self
                .dirs
                .iter()
                .map(|d| d[dir].distance)
                .fold(0.0f32, f32::max)
                + 50.0 * 1852.0;
            if range > f64::from(max) && json_reliable > 0 {
                return;
            }
        }
        if range > f64::from(current.distance) {
            self.dirs[ival][dir] = DistCoords {
                distance: range as f32,
                lat: a.lat as f32,
                lon: a.lon as f32,
                alt: if a.baro_alt_valid.valid() || !a.geom_alt_valid.valid() {
                    a.baro_alt
                } else {
                    a.geom_alt
                },
            };
        }
    }

    /// Forget every recorded point (upstream's `resetRangeOutline` command), e.g. after the
    /// antenna was moved. The current slice stays current.
    pub fn reset(&mut self) {
        for slice in &mut self.dirs {
            *slice = [DistCoords::default(); RANGEDIRS_BUCKETS];
        }
    }

    /// Size of the saved state: upstream's `rangeDirs.gz` contents, an `int` with the current
    /// slice (`lastRangeDirHour`) and then the packed table, little-endian.
    pub const STATE_LEN: usize = 4 + RANGEDIRS_IVALS * RANGEDIRS_BUCKETS * 16;

    /// The outline in upstream's state format, so either decoder can read the other's file.
    pub fn to_state_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::STATE_LEN);
        let current = self
            .newest
            .map_or(0, |n| n.rem_euclid(RANGEDIRS_IVALS as i64)) as i32;
        out.extend_from_slice(&current.to_le_bytes());
        for slice in &self.dirs {
            for c in slice {
                out.extend_from_slice(&c.distance.to_le_bytes());
                out.extend_from_slice(&c.lat.to_le_bytes());
                out.extend_from_slice(&c.lon.to_le_bytes());
                out.extend_from_slice(&c.alt.to_le_bytes());
            }
        }
        out
    }

    /// Restore an outline saved at `saved_at` (ms, e.g. the file's modification time), then
    /// move it on to `now`: slices that time passed through while nothing was running are
    /// cleared, so a stale outline does not come back (upstream keeps them until overwritten).
    /// Rejects data of the wrong size or with an impossible slice number, leaving `self` as is.
    pub fn load_state_bytes(
        &mut self,
        bytes: &[u8],
        saved_at: i64,
        now: i64,
    ) -> Result<(), &'static str> {
        if bytes.len() != Self::STATE_LEN {
            return Err("wrong size");
        }
        let word = |i: usize| <[u8; 4]>::try_from(&bytes[i..i + 4]).expect("4 bytes");
        let current = i64::from(i32::from_le_bytes(word(0)));
        if !(0..RANGEDIRS_IVALS as i64).contains(&current) {
            return Err("bad slice number");
        }
        for (s, slice) in self.dirs.iter_mut().enumerate() {
            for (d, c) in slice.iter_mut().enumerate() {
                let at = 4 + (s * RANGEDIRS_BUCKETS + d) * 16;
                *c = DistCoords {
                    distance: f32::from_le_bytes(word(at)),
                    lat: f32::from_le_bytes(word(at + 4)),
                    lon: f32::from_le_bytes(word(at + 8)),
                    alt: i32::from_le_bytes(word(at + 12)),
                };
            }
        }
        // The file stores the slice wrapped to 0..63; the save time tells which round it
        // belongs to: the latest slice at or before the save time with that number.
        let saved = (saved_at * (RANGEDIRS_IVALS as i64 - 1)).div_euclid(self.duration);
        self.newest = Some(saved - (saved - current).rem_euclid(RANGEDIRS_IVALS as i64));
        self.rotate(now);
        Ok(())
    }

    /// The farthest point per direction over the whole window, in direction order.
    pub fn points(&self) -> Vec<DistCoords> {
        let mut record = [DistCoords::default(); RANGEDIRS_BUCKETS];
        for slice in &self.dirs {
            for (r, c) in record.iter_mut().zip(slice) {
                if c.distance > r.distance {
                    *r = *c;
                }
            }
        }
        record
            .into_iter()
            .filter(|p| p.lat != 0.0 || p.lon != 0.0)
            .collect()
    }
}

/// The most aircraft the table holds: far above any receiver's real traffic (a busy station sees
/// hundreds; each entry is about 2 kB), so only forged addresses can reach it.
pub const MAX_AIRCRAFT: usize = 50_000;

/// The aircraft table.
#[derive(Debug)]
pub struct Tracker {
    pub cfg: TrackerConfig,
    pub stats: TrackStats,
    /// The farthest positions per direction (`outline.json`), when enabled.
    pub outline: Option<RangeOutline>,
    aircraft: HashMap<u32, Aircraft>,
    /// Aircraft worth listing in `aircraft.json`, in the order they became active.
    active: Vec<u32>,
    /// The same addresses, to look one up without a scan on every message.
    active_set: HashSet<u32>,
    next_remove_stale: i64,
    /// New aircraft not added because the table was full ([`MAX_AIRCRAFT`]).
    pub refused_table_full: u64,
    /// Mode A/C reply counters (the ident bit is ignored), indexed by `mode_a_to_index`.
    pub mode_ac_count: Vec<u32>,
}

impl Tracker {
    pub fn new(cfg: TrackerConfig) -> Self {
        Self {
            cfg,
            stats: TrackStats::default(),
            outline: None,
            aircraft: HashMap::new(),
            active: Vec::new(),
            active_set: HashSet::new(),
            next_remove_stale: 0,
            refused_table_full: 0,
            mode_ac_count: vec![0; 4096],
        }
    }

    pub fn get(&self, addr: u32) -> Option<&Aircraft> {
        self.aircraft.get(&addr)
    }

    pub fn len(&self) -> usize {
        self.aircraft.len()
    }

    pub fn is_empty(&self) -> bool {
        self.aircraft.is_empty()
    }

    /// Counts of listed aircraft: with a position, without, and by address type.
    pub fn counts(&self, now: i64) -> AircraftCounts {
        let mut c = AircraftCounts::default();
        for a in self.aircraft.values() {
            if !Self::include_in_json(now, a) {
                continue;
            }
            if a.position_valid.valid() {
                c.with_position += 1;
            } else {
                c.without_position += 1;
            }
            c.by_type[a.addrtype as usize] += 1;
        }
        c
    }

    /// Every aircraft in the table, to update in place (the trace writer's bookkeeping).
    pub fn aircraft_iter_mut(&mut self) -> impl Iterator<Item = &mut Aircraft> {
        self.aircraft.values_mut()
    }

    /// One tracked aircraft by address.
    pub fn aircraft_by_addr(&self, addr: u32) -> Option<&Aircraft> {
        self.aircraft.get(&addr)
    }

    /// Aircraft to publish in `aircraft.json`, in upstream's order.
    pub fn active(&self) -> impl Iterator<Item = &Aircraft> {
        self.active.iter().filter_map(|a| self.aircraft.get(a))
    }

    /// Should this aircraft appear in `aircraft.json`?
    pub fn include_in_json(now: i64, a: &Aircraft) -> bool {
        if a.pos_reliable_valid.valid() {
            return true;
        }
        if a.messages < 2 && a.addrtype != AddrType::Jaero && a.addrtype != AddrType::Other {
            return false;
        }
        now < a.seen + TRACK_EXPIRE
            || (a.addrtype == AddrType::Jaero && now < a.seen + TRACK_EXPIRE_JAERO)
    }

    /// Periodic housekeeping (call with the current time as often as you like; it runs
    /// once per [`REMOVE_STALE_INTERVAL`]).
    pub fn periodic(&mut self, now: i64) {
        // move the outline to the current time slice even when no positions come in
        if let Some(o) = self.outline.as_mut() {
            o.rotate(now);
        }
        if now >= self.next_remove_stale {
            self.remove_stale(now);
            self.next_remove_stale = now + REMOVE_STALE_INTERVAL;
        }
    }

    /// Expire validities of active aircraft and delete aircraft not heard from for long.
    pub fn remove_stale(&mut self, now: i64) {
        // active list: drop aircraft that are no longer interesting
        let mut i = 0;
        while i < self.active.len() {
            let addr = self.active[i];
            let drop = match self.aircraft.get(&addr) {
                Some(a) => {
                    !Self::include_in_json(now, a) && now - a.seen > TRACK_EXPIRE_LONG + MINUTES
                }
                None => true,
            };
            if drop {
                self.active_set.remove(&addr);
                self.active.swap_remove(i);
            } else {
                i += 1;
            }
        }
        let keep_traces = self.cfg.write_traces;
        for addr in &self.active {
            if let Some(a) = self.aircraft.get_mut(addr) {
                a.update_validities(now);
                if keep_traces {
                    a.trace.prune(now, KEEP_TRACES);
                }
            }
        }
        // table: delete old aircraft
        let (pos_timeout, non_icao_pos_timeout) = if self.cfg.write_traces {
            (now - 26 * HOURS, now - 26 * HOURS)
        } else {
            (now - HOURS, now - 30 * MINUTES)
        };
        let nopos_timeout = now - 5 * MINUTES;
        let jaero_timeout = now - TRACK_EXPIRE_JAERO;
        let mut single = 0;
        self.aircraft.retain(|_, a| {
            let expired = ((a.seen_pos_reliable == 0 && a.seen < nopos_timeout)
                || (a.seen_pos_reliable != 0
                    && (a.seen_pos_reliable < pos_timeout
                        || (a.addr & NON_ICAO_ADDRESS != 0
                            && a.seen_pos_reliable < non_icao_pos_timeout))))
                && (a.addrtype != AddrType::Jaero || a.seen < jaero_timeout);
            if expired && a.messages == 1 {
                single += 1;
            }
            !expired
        });
        self.stats.single_message_aircraft += single;
    }

    /// Update the table from a decoded message. Returns the aircraft address if the
    /// message was attributed to one.
    pub fn update_from_message(&mut self, mm: &mut ModesMessage) -> Option<u32> {
        let now = mm.sys_timestamp;
        self.periodic(now);

        if mm.msgtype == DFTYPE_MODEAC {
            // Mode A/C: just count it (the SPI bit is ignored)
            if let Some(sq) = mm.squawk {
                self.mode_ac_count[mode_a_to_index(sq.hex) as usize] += 1;
            }
            return None;
        }
        if mm.decode_result < 0 {
            return None;
        }
        self.stats.messages_total += 1;
        mm.calculated_track = -1.0;

        // DF17/18 and DF11 with IID 0 carry a CRC-protected address.
        mm.address_reliable = matches!(mm.msgtype, 17 | 18) || (mm.msgtype == 11 && mm.iid == 0);

        let addr = mm.addr;
        let cfg = self.cfg.clone();
        let full = self.aircraft.len() >= MAX_AIRCRAFT;
        let a = match self.aircraft.entry(addr) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(_) if mm.address_reliable && full => {
                // full: forged addresses (a CRC is easy to compute) must not exhaust memory
                self.refused_table_full += 1;
                return None;
            }
            std::collections::hash_map::Entry::Vacant(e) if mm.address_reliable => {
                e.insert(Aircraft::new(addr))
            }
            std::collections::hash_map::Entry::Vacant(_) => return None,
        };

        // Messages with a position get processed on a scratch copy; if the position turns
        // out to be bad, duplicate or garbage everything is reverted.
        // The trace stays out of the copy: it can hold 40,000 points, and it only changes once a
        // position is accepted (`set_position`), after every check that could revert it.
        let scratch = mm.cpr.is_some().then(|| {
            let trace = std::mem::take(&mut a.trace);
            let copy = a.clone();
            a.trace = trace;
            copy
        });

        // only count the aircraft as "seen" for reliable messages with CRC
        if mm.address_reliable {
            let elapsed_seen = now - a.seen;
            if elapsed_seen > 5 * MINUTES {
                self.stats.unique_aircraft += 1;
                if (elapsed_seen > 15 * MINUTES && a.addrtype != AddrType::Jaero)
                    || (elapsed_seen > TRACK_EXPIRE_JAERO && a.addrtype == AddrType::Jaero)
                {
                    // if an aircraft hasn't been active in a bit, reset its message count
                    a.messages = 0;
                }
            }
            a.seen = now;
        }

        // don't use messages with unreliable CRC too long after a reliable address
        if now - a.seen > TRACK_STALE {
            return None;
        }

        if mm.signal_level > 0.0 {
            a.signal_level[(a.signal_next % 8) as usize] = mm.signal_level as f32;
            a.signal_next = a.signal_next.wrapping_add(1);
        }

        // reset on overflow ... avoid any low message count checks
        if a.messages == u32::MAX {
            a.messages = u16::MAX as u32;
        }
        a.messages += 1;
        a.message_rate_acc[0] = a.message_rate_acc[0].saturating_add(1);
        if now > a.next_message_rate_calc {
            a.calculate_message_rate(now);
        }

        update_addrtype(a, mm, now);

        // decide where to stash the version
        let mut dummy_version = -1;
        let message_version: &mut i32 = match mm.source {
            DataSource::Adsb => &mut a.adsb_version,
            DataSource::Tisb => &mut a.tisb_version,
            DataSource::Adsr => &mut a.adsr_version,
            _ => &mut dummy_version,
        };
        // assume version 0 until we see something else
        if *message_version < 0 {
            *message_version = 0;
        }
        let version = *message_version;

        if let Some(cat) = mm.category {
            a.category = cat;
            a.category_updated = now;
        }

        // operational status: done early to update version / HRD / TAH
        if mm.opstatus.valid {
            match mm.source {
                DataSource::Adsb => a.adsb_version = mm.opstatus.version as i32,
                DataSource::Tisb => a.tisb_version = mm.opstatus.version as i32,
                DataSource::Adsr => a.adsr_version = mm.opstatus.version as i32,
                _ => {}
            }
            if mm.opstatus.hrd != HeadingType::Invalid {
                a.adsb_hrd = mm.opstatus.hrd;
            }
            if mm.opstatus.tah != HeadingType::Invalid {
                a.adsb_tah = mm.opstatus.tah;
            }
        }
        let version = if mm.opstatus.valid && mm.source == DataSource::Adsb {
            a.adsb_version
        } else {
            version
        };

        // fill in ADS-B v0 NACp and SIL from the position message type
        if version == 0 && mm.accuracy.nac_p.is_none() {
            mm.accuracy.nac_p = compute_v0_nacp(mm);
        }
        if version == 0
            && mm.accuracy.sil_type == SilType::Invalid
            && let Some(sil) = compute_v0_sil(mm)
        {
            mm.accuracy.sil_type = SilType::Unknown;
            mm.accuracy.sil = sil;
        }

        if let Some(alt) = mm.baro_alt
            && (mm.source >= a.baro_alt_valid.source
                || (a.baro_alt_valid.age(now) > 10 * SECONDS
                    && a.baro_alt_valid.source != DataSource::Jaero
                    && a.baro_alt_valid.source != DataSource::Sbs)
                || a.baro_alt_valid.age(now) > 30 * SECONDS)
        {
            update_altitude(&cfg, now, a, mm, alt);
        }

        if let Some(sq) = mm.squawk {
            let ri = cfg.beast_reduce_interval;
            let change_tentative = a.squawk_tentative != sq.hex
                && now - a.seen < 15 * SECONDS
                && will_accept(&a.squawk_valid, mm.source, now, a.seen);
            let change_actual = (mm.source == DataSource::Jaero
                || (a.squawk_tentative == sq.hex && now - a.squawk_tentative_changed > 250))
                && accept(&mut a.squawk_valid, mm.source, mm, a.seen, Reduce::Rare, ri);
            if change_actual {
                a.squawk = sq.hex;
            }
            // forward a squawk change as soon as possible, unless it is flip-flopping
            if (change_tentative || change_actual) && now - a.squawk_tentative_changed > ri {
                a.squawk_valid.next_reduce_forward = now + ri;
                mm.reduce_forward = true;
            }
            if change_tentative {
                a.squawk_tentative = sq.hex;
                a.squawk_tentative_changed = now;
            }
        }

        let seen = a.seen;
        if let Some(e) = mm.emergency
            && accept(
                &mut a.emergency_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.emergency = e;
        }
        if let Some(alt) = mm.geom_alt
            && accept(
                &mut a.geom_alt_valid,
                mm.source,
                mm,
                seen,
                Reduce::Often,
                cfg.beast_reduce_interval,
            )
        {
            a.geom_alt = alt;
        }
        if let Some(d) = mm.geom_delta
            && accept(
                &mut a.geom_delta_valid,
                mm.source,
                mm,
                seen,
                Reduce::Often,
                cfg.beast_reduce_interval,
            )
        {
            a.geom_delta = d;
        }

        if let Some(h) = mm.heading {
            let mut htype = mm.heading_type;
            if htype == HeadingType::MagneticOrTrue {
                htype = a.adsb_hrd;
            } else if htype == HeadingType::TrackOrHeading {
                htype = a.adsb_tah;
            }
            match htype {
                HeadingType::GroundTrack => {
                    if accept(
                        &mut a.track_valid,
                        mm.source,
                        mm,
                        seen,
                        Reduce::Often,
                        cfg.beast_reduce_interval,
                    ) {
                        a.track = h;
                        calc_wind(a, mm, now);
                    }
                }
                HeadingType::Magnetic => {
                    if accept(
                        &mut a.mag_heading_valid,
                        mm.source,
                        mm,
                        seen,
                        Reduce::Often,
                        cfg.beast_reduce_interval,
                    ) {
                        a.mag_heading = h;
                        let err = update_declination(a, now);
                        // derive the true heading from the magnetic one, unless it
                        // disagrees wildly with the ground track
                        let plausible = !a.track_valid.valid()
                            || norm_diff(h as f64 + a.magnetic_declination - a.track as f64, 180.0)
                                .abs()
                                < 45.0;
                        if plausible
                            && err == 0
                            && accept(
                                &mut a.true_heading_valid,
                                DataSource::Indirect,
                                mm,
                                seen,
                                Reduce::Often,
                                cfg.beast_reduce_interval,
                            )
                        {
                            a.true_heading =
                                norm_angle(h as f64 + a.magnetic_declination, 180.0) as f32;
                            calc_wind(a, mm, now);
                        }
                    }
                }
                HeadingType::True
                    if accept(
                        &mut a.true_heading_valid,
                        mm.source,
                        mm,
                        seen,
                        Reduce::Often,
                        cfg.beast_reduce_interval,
                    ) =>
                {
                    a.true_heading = h;
                    calc_wind(a, mm, now);
                }
                _ => {}
            }
        }
        if let Some(v) = mm.track_rate
            && accept(
                &mut a.track_rate_valid,
                mm.source,
                mm,
                seen,
                Reduce::Often,
                cfg.beast_reduce_interval,
            )
        {
            a.track_rate = v;
        }
        if let Some(v) = mm.roll
            && accept(
                &mut a.roll_valid,
                mm.source,
                mm,
                seen,
                Reduce::Often,
                cfg.beast_reduce_interval,
            )
        {
            a.roll = v;
        }

        if let Some(mut gs) = mm.gs {
            gs.selected = if version == 2 { gs.v2 } else { gs.v0 };
            mm.gs = Some(gs);
            if accept(
                &mut a.gs_valid,
                mm.source,
                mm,
                seen,
                Reduce::Often,
                cfg.beast_reduce_interval,
            ) {
                a.gs = gs.selected;
            }
        }
        if let Some(ias) = mm.ias
            && accept(
                &mut a.ias_valid,
                mm.source,
                mm,
                seen,
                Reduce::Often,
                cfg.beast_reduce_interval,
            )
        {
            a.ias = ias;
        }
        if let Some(tas) = mm.tas
            && !(a.ias_valid.valid() && (tas as f32) < 0.7 * a.ias as f32)
            && accept(
                &mut a.tas_valid,
                mm.source,
                mm,
                seen,
                Reduce::Often,
                cfg.beast_reduce_interval,
            )
        {
            a.tas = tas;
            calc_temp(a, now);
            calc_wind(a, mm, now);
        }
        if let Some(m) = mm.mach
            && accept(
                &mut a.mach_valid,
                mm.source,
                mm,
                seen,
                Reduce::Often,
                cfg.beast_reduce_interval,
            )
        {
            a.mach = m;
            calc_temp(a, now);
        }
        if let Some(r) = mm.baro_rate
            && accept(
                &mut a.baro_rate_valid,
                mm.source,
                mm,
                seen,
                Reduce::Often,
                cfg.beast_reduce_interval,
            )
        {
            a.baro_rate = r;
        }
        if let Some(r) = mm.geom_rate
            && accept(
                &mut a.geom_rate_valid,
                mm.source,
                mm,
                seen,
                Reduce::Often,
                cfg.beast_reduce_interval,
            )
        {
            a.geom_rate = r;
        }

        if mm.airground != AirGround::Invalid
            && (mm.source >= a.airground_valid.source
                || mm.source >= DataSource::ModeSChecked
                || a.airground_valid.age(now) > 2 * TRACK_EXPIRE)
            && (a.airground == AirGround::Invalid
                || a.airground == AirGround::Uncertain
                || mm.airground != AirGround::Uncertain
                || (mm.airground == AirGround::Uncertain
                    && a.airground == AirGround::Ground
                    && a.alt_baro_reliable(&cfg)
                    && a.gs_valid.age(now) < 3 * SECONDS
                    && a.gs > 80.0))
        {
            let skip = matches!(a.airground, AirGround::Airborne | AirGround::Ground)
                && matches!(a.last_cpr_type, CprType::Surface | CprType::Airborne)
                && a.cpr_odd_valid.age(now) < 20 * SECONDS
                && a.cpr_even_valid.age(now) < 20 * SECONDS
                && now < a.seen_pos_reliable + 20 * SECONDS
                && a.airground_valid.age(now) < 20 * SECONDS;
            if !skip
                && accept(
                    &mut a.airground_valid,
                    mm.source,
                    mm,
                    seen,
                    Reduce::Rare,
                    cfg.beast_reduce_interval,
                )
            {
                if mm.airground != a.airground {
                    mm.reduce_forward = true;
                    a.last_air_ground_change = now;
                }
                a.airground = mm.airground;
            }
        }

        if mm.callsign_valid
            && accept(
                &mut a.callsign_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
            && let Some(cs) = &mm.callsign
        {
            a.callsign = cs.clone();
        }
        if let Some(v) = mm.nav.mcp_altitude
            && accept(
                &mut a.nav_altitude_mcp_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.nav_altitude_mcp = v;
        }
        if let Some(v) = mm.nav.fms_altitude
            && accept(
                &mut a.nav_altitude_fms_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.nav_altitude_fms = v;
        }
        if mm.nav.altitude_source != NavAltitudeSource::Invalid
            && accept(
                &mut a.nav_altitude_src_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.nav_altitude_src = mm.nav.altitude_source;
        }
        if let Some(v) = mm.nav.heading
            && accept(
                &mut a.nav_heading_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.nav_heading = v;
        }
        if let Some(v) = mm.nav.modes
            && accept(
                &mut a.nav_modes_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.nav_modes = v;
        }
        if let Some(v) = mm.nav.qnh
            && accept(
                &mut a.nav_qnh_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.nav_qnh = v;
        }
        if let Some(v) = mm.alert
            && accept(
                &mut a.alert_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.alert = v;
        }
        if let Some(v) = mm.spi
            && accept(
                &mut a.spi_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.spi = v;
        }
        if let Some(w) = mm.wind {
            a.wind_speed = w.speed;
            a.wind_direction = w.direction;
            a.wind_updated = now;
            a.wind_altitude = a.baro_alt;
        }
        if let Some(o) = mm.oat {
            a.oat = o as f64;
            a.oat_updated = now;
        }

        // CPR
        let mut cpr_new = false;
        if mm.cpr_filtered {
            self.stats.cpr_filtered += 1;
        }
        if let Some(c) = mm.cpr {
            cpr_new = accept_cpr(&cfg, a, mm, c);
            cpr_duplicate_check(now, a, mm, c.lat, c.lon);
        }

        // Status-type messages go out on beast_reduce at a slow, fixed rate.
        if !mm.duplicate {
            let status = (mm.msgtype == 17 && ((1..=4).contains(&mm.metype) || mm.metype >= 23))
                || mm.msgtype == 11
                || (mm.squawk.is_none()
                    && matches!(mm.msgtype, 0 | 4 | 20)
                    && a.airground == AirGround::Ground);
            if status && now > a.next_reduce_forward_status {
                a.next_reduce_forward_status = now + 4 * cfg.beast_reduce_interval;
                mm.reduce_forward = true;
            }
        }

        if mm.acas_ra {
            a.acas_ra = true;
        }

        if let Some(v) = mm.accuracy.nic_a
            && accept(
                &mut a.nic_a_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.nic_a = v;
        }
        if let Some(v) = mm.accuracy.nic_c
            && accept(
                &mut a.nic_c_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.nic_c = v;
        }
        if let Some(v) = mm.accuracy.nic_baro
            && accept(
                &mut a.nic_baro_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.nic_baro = v;
        }
        if let Some(v) = mm.accuracy.nac_p
            && accept(
                &mut a.nac_p_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.nac_p = v;
        }
        if let Some(v) = mm.accuracy.nac_v
            && accept(
                &mut a.nac_v_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.nac_v = v;
        }
        if mm.accuracy.sil_type != SilType::Invalid
            && accept(
                &mut a.sil_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.sil = mm.accuracy.sil;
            if a.sil_type == SilType::Invalid || mm.accuracy.sil_type != SilType::Unknown {
                a.sil_type = mm.accuracy.sil_type;
            }
        }
        if let Some(v) = mm.accuracy.gva
            && accept(
                &mut a.gva_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.gva = v;
        }
        if let Some(v) = mm.accuracy.sda
            && accept(
                &mut a.sda_valid,
                mm.source,
                mm,
                seen,
                Reduce::Rare,
                cfg.beast_reduce_interval,
            )
        {
            a.sda = v;
        }

        // derived geometric altitude / baro-geom delta
        if (a.baro_alt_valid.updated > a.geom_alt_valid.updated
            || a.geom_delta_valid.updated > a.geom_alt_valid.updated)
            && a.alt_baro_reliable(&cfg)
            && compare_validity(&a.baro_alt_valid, &a.geom_alt_valid) > 0
            && a.geom_delta_valid.valid()
            && a.geom_delta_valid.source >= a.geom_alt_valid.source
        {
            a.geom_alt = a.baro_alt + a.geom_delta;
            let (b, d) = (a.baro_alt_valid, a.geom_delta_valid);
            combine_validity(&mut a.geom_alt_valid, &b, &d, now);
        }
        if mm.geom_alt.is_some()
            && a.geom_alt_valid.updated > a.geom_delta_valid.updated
            && a.alt_baro_reliable(&cfg)
            && a.baro_alt_valid.age(now) < SECONDS
            && accept(
                &mut a.geom_delta_valid,
                mm.source,
                mm,
                seen,
                Reduce::Often,
                cfg.beast_reduce_interval,
            )
        {
            let (b, g) = (a.baro_alt_valid, a.geom_alt_valid);
            combine_validity(&mut a.geom_delta_valid, &b, &g, now);
            a.geom_delta = a.geom_alt - a.baro_alt;
        }

        if cpr_new {
            let c = mm.cpr.expect("cpr_new implies a cpr");
            if mm.addrtype >= a.addrtype {
                if c.kind == CprType::Airborne
                    && (a.last_cpr_type == CprType::Surface || mm.airground == AirGround::Airborne)
                    && accept(
                        &mut a.airground_valid,
                        mm.source,
                        mm,
                        seen,
                        Reduce::Rare,
                        cfg.beast_reduce_interval,
                    )
                {
                    if mm.airground != a.airground {
                        mm.reduce_forward = true;
                    }
                    a.airground = AirGround::Airborne;
                }
                if c.kind == CprType::Airborne
                    && mm.airground == AirGround::Uncertain
                    && a.airground == AirGround::Ground
                    && accept(
                        &mut a.airground_valid,
                        mm.source,
                        mm,
                        seen,
                        Reduce::Rare,
                        cfg.beast_reduce_interval,
                    )
                {
                    if mm.airground != a.airground {
                        mm.reduce_forward = true;
                    }
                    a.airground = AirGround::Uncertain;
                }
                if c.kind == CprType::Surface
                    && accept(
                        &mut a.airground_valid,
                        mm.source,
                        mm,
                        seen,
                        Reduce::Rare,
                        cfg.beast_reduce_interval,
                    )
                {
                    if mm.airground != a.airground {
                        mm.reduce_forward = true;
                    }
                    a.airground = AirGround::Ground;
                }
            }
            update_position(&cfg, &mut self.stats, a, mm, c, now);
            if mm.outline_position
                && let Some(o) = self.outline.as_mut()
            {
                o.update(a, now, cfg.json_reliable);
            }
        }

        if cpr_new {
            a.last_cpr_type = mm.cpr.map_or(CprType::Invalid, |c| c.kind);
        }

        // all-call replies with a CRC-protected address: forward one every 4 intervals
        if mm.msgtype == 11
            && mm.iid == 0
            && mm.corrected_bits == 0
            && now > a.next_reduce_forward_df11
        {
            a.next_reduce_forward_df11 = now + 4 * cfg.beast_reduce_interval;
            mm.reduce_forward = true;
        }

        if let Some(mut scratch) = scratch
            && (mm.pos_bad || mm.duplicate)
        {
            scratch.trace = std::mem::take(&mut a.trace);
            *a = scratch;
        }

        if !(mm.source < a.position_valid.source || mm.in_disc_cache || mm.pos_ignore) {
            if mm.pos_bad {
                position_bad(&cfg, a, mm, now);
            }
            a.speed_unreliable = (a.speed_unreliable + mm.speed_unreliable).clamp(0, 16);
            a.track_unreliable = (a.track_unreliable + mm.track_unreliable).clamp(0, 16);
        }

        // put it on the active list once it is worth showing
        let include = {
            let a = &*a;
            !self.active_set.contains(&addr) && Self::include_in_json(now, a)
        };
        if include {
            if let Some(a) = self.aircraft.get_mut(&addr) {
                a.update_validities(now);
            }
            self.active.push(addr);
            self.active_set.insert(addr);
        }
        Some(addr)
    }
}

/// 2.5 seconds maximum between messages used for calculating wind and temperature.
const TRACK_WT_TIMEOUT: i64 = 2500;

/// Estimate the wind from true airspeed, ground speed, true heading and track.
/// The result goes into `mm.wind`; the tracker stores it into the aircraft afterwards.
fn calc_wind(a: &Aircraft, mm: &mut ModesMessage, now: i64) {
    if a.airground == AirGround::Ground {
        return;
    }
    if a.tas_valid.age(now) > TRACK_WT_TIMEOUT
        || a.gs_valid.age(now) > TRACK_WT_TIMEOUT
        || a.true_heading_valid.age(now) > TRACK_WT_TIMEOUT / 2
        || a.track_valid.age(now) > TRACK_WT_TIMEOUT / 2
    {
        return;
    }
    // only in steady flight
    if a.roll_valid.valid() && a.roll.abs() > 10.0 {
        return;
    }
    if a.track_rate_valid.valid() && a.track_rate.abs() > 0.5 {
        return;
    }
    // at most one estimate per 700 ms
    if now < a.wind_updated + 700 {
        return;
    }
    use std::f64::consts::PI;
    let trk = (PI / 180.0) * a.track as f64;
    let hdg = (PI / 180.0) * a.true_heading as f64;
    let (tas, gs) = (a.tas as f64, a.gs as f64);
    let crab = norm_diff(hdg - trk, PI);
    let hw = tas - crab.cos() * gs;
    let cw = crab.sin() * gs;
    let ws = (hw * hw + cw * cw).sqrt();
    let wd = norm_angle(hdg + cw.atan2(hw), PI) * (180.0 / PI);
    if ws > 250.0 {
        return;
    }
    mm.wind = Some(crate::comm_b::Wind {
        speed: ws as f32,
        direction: wd as f32,
    });
}

/// Estimate outside and total air temperature from true airspeed and Mach number.
fn calc_temp(a: &mut Aircraft, now: i64) {
    if a.airground == AirGround::Ground {
        return;
    }
    if a.tas_valid.age(now) > TRACK_WT_TIMEOUT || a.mach_valid.age(now) > TRACK_WT_TIMEOUT {
        return;
    }
    if a.mach < 0.395 {
        return;
    }
    let fraction = a.tas as f64 / 661.47 / a.mach;
    let oat = (fraction * fraction * 288.15) - 273.15;
    let tat = -273.15 + ((oat + 273.15) * (1.0 + 0.2 * a.mach * a.mach));
    if !(-100.0..=60.0).contains(&oat) {
        return;
    }
    a.oat = oat;
    a.tat = tat;
    a.oat_updated = now;
    a.tat_updated = now;
}

/// Refresh the magnetic declination at the aircraft's position (at most every 5 s).
/// Returns non-zero if it can't be computed yet (no altitude or position).
fn update_declination(a: &mut Aircraft, now: i64) -> i32 {
    if now < a.updated_declination + 5 * SECONDS {
        return 0;
    }
    if !a.baro_alt_valid.valid() || !a.position_valid.valid() {
        return 1;
    }
    let year = geomag::decimal_year(now);
    let f = geomag::field(a.baro_alt as f64 * 0.0003048, a.lat, a.lon, year);
    a.updated_declination = now;
    a.magnetic_declination = f.declination;
    0
}

fn update_addrtype(a: &mut Aircraft, mm: &ModesMessage, now: i64) {
    let rank = |t: AddrType| {
        if t == AddrType::ModeS {
            4.5f32
        } else {
            t as i32 as f32
        }
    };
    let (new_type, old_type) = (rank(mm.addrtype), rank(a.addrtype));
    // change type ranking without messing with the enum
    if (new_type <= old_type && now - a.addrtype_updated > TRACK_EXPIRE * 3 / 4)
        || (new_type > old_type && now - a.addrtype_updated > TRACK_EXPIRE * 3 / 2)
    {
        if mm.addrtype == AddrType::AdsbIcao && a.position_valid.source != DataSource::Adsb {
            // don't set to ADS-B without a position
            if mm.msgtype == 17 {
                a.addrtype = AddrType::ModeS;
                a.addrtype_updated = now;
            }
        } else {
            a.addrtype = mm.addrtype;
            a.addrtype_updated = now;
        }
        if a.addrtype > AddrType::AdsbIcaoNt {
            a.adsb_version = -1; // reset the version if a non ADS-B message type is received
        }
    }
}

fn compute_v0_nacp(mm: &ModesMessage) -> Option<u8> {
    if mm.msgtype != 17 && mm.msgtype != 18 {
        return None;
    }
    Some(match mm.metype {
        0 | 8 | 18 | 22 => 0,
        5 | 9 | 20 => 11,
        6 | 10 | 21 => 10,
        7 | 11 => 8,
        12 => 7,
        13 => 6,
        14 => 5,
        15 => 4,
        16 | 17 => 1,
        _ => return None,
    })
}

fn compute_v0_sil(mm: &ModesMessage) -> Option<u8> {
    if mm.msgtype != 17 && mm.msgtype != 18 {
        return None;
    }
    match mm.metype {
        0 | 18 | 22 => Some(0),
        5..=17 | 20 | 21 => Some(2),
        _ => None,
    }
}

fn compute_nic(metype: u32, version: i32, nic_a: bool, nic_b: bool, nic_c: bool) -> u32 {
    match metype {
        5 | 9 | 20 => 11,
        6 | 10 | 21 => 10,
        7 => match version {
            2 if nic_a && !nic_c => 9,
            1 if nic_a => 9,
            _ => 8,
        },
        8 => {
            if version == 2 {
                match (nic_a, nic_c) {
                    (true, true) => 7,
                    (true, false) | (false, true) => 6,
                    (false, false) => 0,
                }
            } else {
                0
            }
        }
        11 => match version {
            2 if nic_a && nic_b => 9,
            1 if nic_a => 9,
            _ => 8,
        },
        12 => 7,
        13 => 6,
        14 => 5,
        15 => 4,
        16 => {
            if nic_a && nic_b {
                3
            } else {
                2
            }
        }
        17 => 1,
        _ => 0,
    }
}

/// Containment radius in metres, 0 (`RC_UNKNOWN`) if unknown.
fn compute_rc(metype: u32, version: i32, nic_a: bool, nic_b: bool, nic_c: bool) -> u32 {
    match metype {
        5 | 9 | 20 => 8,
        6 | 10 | 21 => 25,
        7 => match version {
            2 if nic_a && !nic_c => 75,
            1 if nic_a => 75,
            _ => 186,
        },
        8 => {
            if version == 2 {
                match (nic_a, nic_c) {
                    (true, true) => 371,
                    (true, false) => 556,
                    (false, true) => 926,
                    (false, false) => 0,
                }
            } else {
                0
            }
        }
        11 => match version {
            2 if nic_a && nic_b => 75,
            1 if nic_a => 75,
            _ => 186,
        },
        12 => 371,
        13 => match version {
            2 => match (nic_a, nic_b) {
                (false, true) => 556,
                (false, false) => 926,
                (true, true) => 1112,
                (true, false) => 0,
            },
            1 if nic_a => 1112,
            _ => 926,
        },
        14 => 1852,
        15 => 3704,
        16 => match version {
            2 | 1 if nic_a && (version == 1 || nic_b) => 7408,
            2 | 1 => 14816,
            _ => 18520,
        },
        17 => 37040,
        _ => 0,
    }
}

fn nic_rc_from_message(mm: &ModesMessage, a: &Aircraft) -> (u32, u32) {
    let nic_a = a.nic_a_valid.valid() && a.nic_a;
    let nic_b = mm.accuracy.nic_b.unwrap_or(false);
    let nic_c = a.nic_c_valid.valid() && a.nic_c;
    (
        compute_nic(mm.metype, a.adsb_version, nic_a, nic_b, nic_c),
        compute_rc(mm.metype, a.adsb_version, nic_a, nic_b, nic_c),
    )
}

fn accept_cpr(
    cfg: &TrackerConfig,
    a: &mut Aircraft,
    mm: &mut ModesMessage,
    c: crate::mode_s::CprFields,
) -> bool {
    let seen = a.seen;
    if !c.odd
        && accept(
            &mut a.cpr_even_valid,
            mm.source,
            mm,
            seen,
            Reduce::Often,
            cfg.beast_reduce_interval,
        )
    {
        a.cpr_even_type = c.kind;
        a.cpr_even_lat = c.lat;
        a.cpr_even_lon = c.lon;
        let (nic, rc) = nic_rc_from_message(mm, a);
        a.cpr_even_nic = nic;
        a.cpr_even_rc = rc;
        return true;
    }
    if c.odd
        && accept(
            &mut a.cpr_odd_valid,
            mm.source,
            mm,
            seen,
            Reduce::Often,
            cfg.beast_reduce_interval,
        )
    {
        a.cpr_odd_type = c.kind;
        a.cpr_odd_lat = c.lat;
        a.cpr_odd_lon = c.lon;
        let (nic, rc) = nic_rc_from_message(mm, a);
        a.cpr_odd_nic = nic;
        a.cpr_odd_rc = rc;
        return true;
    }
    false
}

/// A CPR frame already seen from another receiver within 2 s is a duplicate.
fn cpr_duplicate_check(
    now: i64,
    a: &mut Aircraft,
    mm: &mut ModesMessage,
    lat: u32,
    lon: u32,
) -> bool {
    let in_cache = a.cpr_cache.iter().any(|c| {
        now - c.ts < 2 * SECONDS
            && c.cpr_lat == lat
            && c.cpr_lon == lon
            && c.receiver_id != mm.receiver_id
    });
    if in_cache {
        mm.duplicate = true;
        true
    } else {
        a.cpr_cache_index = (a.cpr_cache_index + 1) % CPR_CACHE;
        a.cpr_cache[a.cpr_cache_index] = CprCache {
            cpr_lat: lat,
            cpr_lon: lon,
            ts: now,
            receiver_id: mm.receiver_id,
        };
        false
    }
}

/// Is this decoded position the same as the last one or the one before (within 2 s)?
fn duplicate_check(
    now: i64,
    a: &Aircraft,
    new_lat: f64,
    new_lon: f64,
    mm: &mut ModesMessage,
) -> bool {
    if mm.duplicate {
        return true;
    }
    if now > a.seen_pos + 2 * SECONDS {
        return false;
    }
    if a.lat == new_lat && a.lon == new_lon {
        mm.duplicate = true;
        return true;
    }
    if now > a.prev_pos_time + 2 * SECONDS {
        return false;
    }
    if a.prev_lat == new_lat && a.prev_lon == new_lon {
        mm.duplicate = true;
        return true;
    }
    false
}

fn in_disc_cache(now: i64, a: &Aircraft, lat: u32, lon: u32, receiver_id: u64) -> bool {
    a.disc_cache.iter().any(|d| {
        (now - d.ts < 4 * SECONDS && d.cpr_lat == lat && d.cpr_lon == lon)
            || (now - d.ts < 300 && d.receiver_id == receiver_id)
    })
}

/// Result of a position decode attempt.
enum Located {
    Pos {
        lat: f64,
        lon: f64,
        nic: u32,
        rc: u32,
        relative_to: u8,
    },
    /// Not enough information (upstream -1).
    Skipped,
    /// Bad data / failed a check (upstream -2); position kept for logging.
    Bad { lat: f64, lon: f64 },
}

/// Plausibility check of a new position against the last: speed, track and range.
/// Returns true if the position is acceptable.
fn speed_check(
    cfg: &TrackerConfig,
    a: &Aircraft,
    source: DataSource,
    lat: f64,
    lon: f64,
    mm: &mut ModesMessage,
) -> bool {
    let now = mm.sys_timestamp;
    let elapsed = a.position_valid.age(now);

    if duplicate_check(now, a, lat, lon, mm) {
        // don't use duplicate positions, but don't fail them either
        mm.pos_ignore = true;
        return true;
    }

    if let Some(c) = mm.cpr
        && in_disc_cache(now, a, c.lat, c.lon, mm.receiver_id)
    {
        mm.in_disc_cache = true;
    }

    let old_lat = a.lat;
    let old_lon = a.lon;
    let surface = a.airground_valid.valid()
        && a.airground == AirGround::Ground
        && a.pos_surface
        && mm.cpr.is_none_or(|c| c.kind == CprType::Surface);

    let json_reliable = cfg.json_reliable;
    let mut override_ = false;
    let cpr_zero = mm.cpr.is_some_and(|c| c.lat == 0 && c.lon == 0);
    let cpr_half_zero = mm.cpr.is_some_and(|c| c.lat == 0 || c.lon == 0);
    if json_reliable == -1 || source == DataSource::Prio {
        override_ = true;
    } else if bogus_lat_lon(lat, lon)
        || cpr_zero
        || (cpr_half_zero && (a.position_valid.source < DataSource::Tisb || !a.pos_reliable(cfg)))
    {
        mm.pos_ignore = true; // don't decrement pos_reliable
    } else if a.pos_reliable_odd < 0.01 || a.pos_reliable_even < 0.01 {
        override_ = true;
    } else if now - a.position_valid.updated > POS_RELIABLE_TIMEOUT {
        override_ = true; // no reference or older than 60 minutes, assume OK
    } else if (source > a.position_valid.source
        && source > a.position_valid.last_source
        && source > a.pos_reliable_valid.source)
        // data is better quality
        || (source > a.position_valid.source && a.position_valid.source == DataSource::Indirect)
        || (source <= DataSource::Mlat && elapsed > 45 * SECONDS)
    {
        override_ = true;
    }
    if mm.in_disc_cache {
        override_ = false; // don't override if in discard cache
    }

    let mut speed: f32 = -1.0;
    if a.gs_valid.valid() {
        speed = if a.gs_last_pos > a.gs {
            a.gs_last_pos
        } else {
            a.gs
        };
        speed = speed
            + (3.0 * a.gs_valid.age(now) as f32 / 1000.0)
            + (3.0 * a.position_valid.age(now) as f32 / 1000.0);
    } else if a.tas_valid.valid() {
        speed = (a.tas * 4 / 3) as f32;
    } else if a.ias_valid.valid() {
        speed = (a.ias * 2) as f32;
    }
    let transmitted_speed = speed;

    let distance = greatcircle(old_lat, old_lon, lat, lon, false) as f32;
    mm.distance_traveled = distance as f64;

    let mut track_diff: f32 = -1.0;
    let mut track_bonus: f32;
    let track_max_age = 5 * SECONDS;
    let mut track_age: i64 = -1;
    let mut track: f32 = -1.0;
    if a.track_valid.age(now) < track_max_age {
        track = a.track;
        track_age = a.track_valid.age(now);
    } else if a.true_heading_valid.age(now) < track_max_age {
        track = a.true_heading;
        track_age = a.true_heading_valid.age(now);
    }

    if distance > 2.5 {
        let calc_track = bearing(old_lat, old_lon, lat, lon) as f32;
        mm.calculated_track = calc_track;
        if source != DataSource::Mlat && track > -1.0 && a.position_valid.age(now) < 7 * SECONDS {
            track_diff = norm_diff((track - calc_track) as f64, 180.0).abs() as f32;
        }
    }

    if track_diff > -1.0 {
        mm.track_unreliable = if track_diff > 70.0 && speed > 10.0 {
            1
        } else {
            -1
        };
    }
    if !a.pos_reliable(cfg) {
        track_diff = -1.0;
    }

    if speed < 0.0 || a.speed_unreliable > 8 {
        speed = if surface { 120.0 } else { 900.0 }; // guess
    }

    let mut range: f32 = -1.0;
    if speed > 10.0 && track_diff > -1.0 && a.track_unreliable < 8 {
        track_bonus = speed * (90.0 - track_diff) / 90.0;
        track_bonus *=
            (if surface { 0.9 } else { 1.0 }) * (1.0 - track_age as f32 / track_max_age as f32);
        if a.gs < 10.0 {
            track_bonus = track_bonus.max(0.0);
            speed += 2.0;
        }
        speed += track_bonus;
        if track_diff > 160.0 {
            mm.pos_old = true; // don't decrement pos_reliable
        }
        if speed > 40.0 && track_diff < 10.0 {
            range += 2e3;
        }
    } else {
        speed *= 1.3;
    }

    range += if surface { 10.0 } else { 30.0 };
    if elapsed < 2 && a.receiver_id == mm.receiver_id && source > DataSource::Mlat {
        range += 500.0; // 500 m extra in this case
    }
    speed = speed.min(2000.0);
    if source == DataSource::Mlat {
        speed *= 1.4;
        speed += 50.0;
        range += 250.0;
    }

    if transmitted_speed < 0.0 {
        mm.speed_unreliable = -1;
    } else if distance > 2.5 && (track_diff < 70.0 || track_diff == -1.0) {
        if distance
            <= range
                + ((elapsed as f32 + 50.0) * (1.0 / 1000.0))
                    * (transmitted_speed * KNOTS_TO_METERS_PER_SECOND)
        {
            mm.speed_unreliable = -1;
        } else if distance
            > range
                + ((elapsed as f32 + 400.0) * (1.0 / 1000.0))
                    * (transmitted_speed * KNOTS_TO_METERS_PER_SECOND)
        {
            mm.speed_unreliable = 1;
        }
    }

    range += ((elapsed as f32 + 200.0) * (1.0 / 1000.0)) * (speed * (1852.0 / 3600.0));
    let mut inrange = distance <= range;

    // Position jumped back while the aircraft is moving fast: don't trust it
    if !surface
        && a.gs > 10.0
        && track_diff > 135.0
        && elapsed < 2 * SECONDS
        && a.track_valid.age(now) < 2 * SECONDS
        && a.receiver_id != mm.receiver_id
    {
        inrange = false;
    }

    if override_ {
        inrange = true;
    }
    inrange
}

fn do_global_cpr(
    cfg: &TrackerConfig,
    stats: &mut TrackStats,
    a: &Aircraft,
    mm: &mut ModesMessage,
    c: crate::mode_s::CprFields,
) -> Located {
    let fflag = c.odd;
    let surface = c.kind == CprType::Surface;
    let nic = a.cpr_even_nic.min(a.cpr_odd_nic);
    let rc = a.cpr_even_rc.max(a.cpr_odd_rc);

    let result = if surface {
        let (reflat, reflon) =
            if let Some((la, lo)) = cfg.user_location.filter(|_| cfg.user_location_ref) {
                (la, lo)
            } else if a.seen_pos != 0 && a.surface_cpr_allow_ac_rel {
                (a.lat_reliable, a.lon_reliable)
            } else {
                return Located::Skipped;
            };
        let r = cpr::decode_surface(
            reflat,
            reflon,
            a.cpr_even_lat,
            a.cpr_even_lon,
            a.cpr_odd_lat,
            a.cpr_odd_lon,
            fflag,
        );
        if let Ok((la, lo)) = r
            && greatcircle(reflat, reflon, la, lo, false) > 450e3
        {
            return Located::Skipped;
        }
        r
    } else {
        cpr::decode_airborne(
            a.cpr_even_lat,
            a.cpr_even_lon,
            a.cpr_odd_lat,
            a.cpr_odd_lon,
            fflag,
        )
    };

    let (lat, lon) = match result {
        Ok(p) => p,
        Err(cpr::CprError::BadData) => return Located::Bad { lat: 0.0, lon: 0.0 },
        Err(_) => return Located::Skipped,
    };

    if cfg.max_range > 0.0
        && let Some((ulat, ulon)) = cfg.user_location
    {
        mm.receiver_distance = greatcircle(ulat, ulon, lat, lon, false);
        if mm.receiver_distance > cfg.max_range {
            if mm.source != DataSource::Mlat {
                stats.cpr_global_range_checks += 1;
            }
            return Located::Bad { lat, lon };
        }
    }
    if !speed_check(cfg, a, mm.source, lat, lon, mm) {
        if mm.source != DataSource::Mlat {
            stats.cpr_global_speed_checks += 1;
        }
        return Located::Bad { lat, lon };
    }
    Located::Pos {
        lat,
        lon,
        nic,
        rc,
        relative_to: 0,
    }
}

fn do_local_cpr(
    cfg: &TrackerConfig,
    stats: &mut TrackStats,
    a: &Aircraft,
    mm: &mut ModesMessage,
    c: crate::mode_s::CprFields,
) -> Located {
    let fflag = c.odd;
    let surface = c.kind == CprType::Surface;
    let (mut nic, mut rc) = if fflag {
        (a.cpr_odd_nic, a.cpr_odd_rc)
    } else {
        (a.cpr_even_nic, a.cpr_even_rc)
    };
    let now = mm.sys_timestamp;

    let (reflat, reflon, range_limit, relative_to);
    if now - a.seen_pos_reliable < 10 * MINUTES && a.local_cpr_allow_ac_rel {
        reflat = a.lat;
        reflon = a.lon;
        if a.pos_nic < nic {
            nic = a.pos_nic;
        }
        if a.pos_rc < rc {
            rc = a.pos_rc;
        }
        range_limit = 1852.0 * 100.0; // 100 NM
        relative_to = 1u8;
    } else if !surface
        && cfg.user_location_ref
        && let Some((la, lo)) = cfg.user_location
    {
        reflat = la;
        reflon = lo;
        if cfg.max_range == 0.0 {
            return Located::Skipped;
        } else if cfg.max_range <= 1852.0 * 180.0 {
            range_limit = cfg.max_range;
        } else if cfg.max_range < 1852.0 * 360.0 {
            range_limit = 1852.0 * 360.0 - cfg.max_range;
        } else {
            return Located::Skipped;
        }
        relative_to = 2;
    } else {
        return Located::Skipped;
    }

    let Ok((lat, lon)) = cpr::decode_relative(reflat, reflon, c.lat, c.lon, fflag, surface) else {
        return Located::Skipped;
    };

    if range_limit > 0.0 && greatcircle(reflat, reflon, lat, lon, false) > range_limit {
        if mm.source != DataSource::Mlat {
            stats.cpr_local_range_checks += 1;
        }
        return Located::Skipped;
    }

    if cfg.max_range > 0.0
        && let Some((ulat, ulon)) = cfg.user_location
    {
        mm.receiver_distance = greatcircle(ulat, ulon, lat, lon, false);
        if mm.receiver_distance > cfg.max_range {
            if mm.source != DataSource::Mlat {
                stats.cpr_local_range_checks += 1;
            }
            return Located::Bad { lat, lon };
        }
    }
    if !speed_check(cfg, a, mm.source, lat, lon, mm) {
        if mm.source != DataSource::Mlat {
            stats.cpr_local_speed_checks += 1;
        }
        return Located::Bad { lat, lon };
    }
    Located::Pos {
        lat,
        lon,
        nic,
        rc,
        relative_to,
    }
}

/// Max time between an even and an odd frame for a global decode (airborne).
fn cpr_global_airborne_max_elapsed(now: i64, a: &Aircraft) -> i64 {
    if !a.gs_valid.valid() || a.gs_valid.age(now) > 20 * SECONDS {
        return 10 * SECONDS;
    }
    let speed = (a.gs as i64).max(1);
    let ival = (19 * SECONDS * 500) / speed; // empirically tested
    ival.min(30 * SECONDS)
}

fn update_position(
    cfg: &TrackerConfig,
    stats: &mut TrackStats,
    a: &mut Aircraft,
    mm: &mut ModesMessage,
    c: crate::mode_s::CprFields,
    now: i64,
) {
    let surface = c.kind == CprType::Surface;
    a.pos_surface = a.airground_valid.valid() && a.airground == AirGround::Ground;

    let max_elapsed = if surface {
        if mm.source != DataSource::Mlat {
            stats.cpr_surface += 1;
        }
        if mm.gs.is_some_and(|g| g.selected <= 25.0) {
            50_000
        } else {
            25_000
        }
    } else {
        if mm.source != DataSource::Mlat {
            stats.cpr_airborne += 1;
        }
        cpr_global_airborne_max_elapsed(now, a)
    };

    let seen = a.seen;
    let mut global = false;
    // None = still undecided (upstream -1), Some(true)=position found
    let mut located: Option<Located> = None;
    let mut undecided = true;

    if a.cpr_odd_valid.valid()
        && a.cpr_even_valid.valid()
        && a.cpr_odd_valid.source == a.cpr_even_valid.source
        && a.cpr_odd_type == a.cpr_even_type
        && (a.cpr_odd_valid.updated - a.cpr_even_valid.updated).abs() <= max_elapsed
    {
        match do_global_cpr(cfg, stats, a, mm, c) {
            Located::Bad { lat, lon } => {
                if mm.source != DataSource::Mlat {
                    stats.cpr_global_bad += 1;
                }
                mm.pos_bad = true;
                mm.decoded_pos = Some((lat, lon));
                undecided = false;
            }
            Located::Skipped => {
                if mm.source != DataSource::Mlat {
                    stats.cpr_global_skipped += 1;
                }
            }
            pos @ Located::Pos { .. } => {
                if accept(
                    &mut a.position_valid,
                    mm.source,
                    mm,
                    seen,
                    Reduce::Double,
                    cfg.beast_reduce_interval,
                ) {
                    if mm.source != DataSource::Mlat {
                        stats.cpr_global_ok += 1;
                    }
                    global = true;
                    located = Some(pos);
                } else {
                    if mm.source != DataSource::Mlat {
                        stats.cpr_global_skipped += 1;
                    }
                    undecided = false; // upstream sets the result to -2 here
                }
                undecided = undecided && located.is_none();
            }
        }
    }

    if undecided && located.is_none() {
        match do_local_cpr(cfg, stats, a, mm, c) {
            Located::Bad { lat, lon } => {
                mm.pos_bad = true;
                mm.decoded_pos = Some((lat, lon));
            }
            Located::Pos {
                lat,
                lon,
                nic,
                rc,
                relative_to,
            } if accept(
                &mut a.position_valid,
                mm.source,
                mm,
                seen,
                Reduce::Double,
                cfg.beast_reduce_interval,
            ) =>
            {
                if mm.source != DataSource::Mlat {
                    stats.cpr_local_ok += 1;
                }
                mm.cpr_relative = true;
                if relative_to == 1 && mm.source != DataSource::Mlat {
                    stats.cpr_local_aircraft_relative += 1;
                }
                if relative_to == 2 && mm.source != DataSource::Mlat {
                    stats.cpr_local_receiver_relative += 1;
                }
                located = Some(Located::Pos {
                    lat,
                    lon,
                    nic,
                    rc,
                    relative_to,
                });
            }
            _ => {
                if mm.source != DataSource::Mlat {
                    stats.cpr_local_skipped += 1;
                }
            }
        }
    }

    if let Some(Located::Pos {
        lat, lon, nic, rc, ..
    }) = located
    {
        mm.cpr_decoded = true;
        mm.decoded_pos = Some((lat, lon));
        mm.decoded_nic = nic;
        mm.decoded_rc = rc;
        if a.gs_valid.valid() {
            a.gs_last_pos = a.gs;
        }
        if global {
            increment_reliable(cfg, a, mm, now, c.odd as u8);
        }
        set_position(cfg, stats, a, mm, now);
    }
}

fn increment_reliable(cfg: &TrackerConfig, a: &mut Aircraft, mm: &ModesMessage, now: i64, odd: u8) {
    a.local_cpr_allow_ac_rel = true;
    let threshold = cfg.json_reliable as f32;
    if a.seen_pos_reliable != 0
        && now - a.seen_pos_reliable > POS_RELIABLE_TIMEOUT
        && mm.source > DataSource::Jaero
        && a.pos_reliable_odd < threshold
        && a.pos_reliable_even < threshold
        && let Some((lat, lon)) = mm.decoded_pos
    {
        let distance = greatcircle(a.lat_reliable, a.lon_reliable, lat, lon, false);
        if distance < 50e3 {
            a.pos_reliable_odd = threshold.max(1.0);
            a.pos_reliable_even = threshold.max(1.0);
        }
    }
    let mut increment = 1.0f32;
    if odd == 3 {
        increment = cfg.json_reliable as f32;
    }
    let persistence = cfg.position_persistence as f32;
    if odd == 1 {
        a.pos_reliable_odd = (a.pos_reliable_odd + increment).min(persistence);
    }
    if odd == 0 || odd == 2 || odd == 3 {
        a.pos_reliable_even = (a.pos_reliable_even + increment).min(persistence);
    }
    if a.pos_reliable_odd < increment {
        a.pos_reliable_odd = increment;
    }
    if a.pos_reliable_even < increment {
        a.pos_reliable_even = increment;
    }
}

fn position_bad(cfg: &TrackerConfig, a: &mut Aircraft, mm: &mut ModesMessage, now: i64) {
    if let Some(c) = mm.cpr {
        a.disc_cache_index = (a.disc_cache_index + 1) % DISCARD_CACHE;
        a.disc_cache[a.disc_cache_index] = CprCache {
            cpr_lat: c.lat,
            cpr_lon: c.lon,
            ts: now,
            receiver_id: mm.receiver_id,
        };
    }
    a.pos_reliable_odd -= 0.26;
    a.pos_reliable_even -= 0.26;
    if a.pos_reliable_odd < 0.1 || a.pos_reliable_even < 0.1 {
        a.pos_reliable_odd = 0.0;
        a.pos_reliable_even = 0.0;
        a.cpr_even_valid.source = DataSource::Invalid;
        a.cpr_odd_valid.source = DataSource::Invalid;
        if let Some(c) = mm.cpr {
            accept_cpr(cfg, a, mm, c);
        }
    }
}

fn set_position(
    cfg: &TrackerConfig,
    stats: &mut TrackStats,
    a: &mut Aircraft,
    mm: &mut ModesMessage,
    now: i64,
) {
    let Some((lat, lon)) = mm.decoded_pos else {
        return;
    };

    // a recent, higher quality position already exists and this one barely moved
    if now < a.seen_pos + 10 * MINUTES
        && mm.source < a.position_valid.last_source
        && mm.distance_traveled < 20.0
    {
        mm.duplicate = true;
        mm.pos_ignore = true;
    }
    if duplicate_check(now, a, lat, lon, mm) {
        mm.pos_ignore = true;
    }
    if bogus_lat_lon(lat, lon) {
        return;
    }
    stats.pos_by_type[mm.addrtype as usize] += 1;
    stats.pos_all += 1;
    if mm.cpr.is_some() && mm.pos_bad {
        stats.pos_garbage += 1;
        return;
    }
    if mm.duplicate {
        stats.pos_duplicate += 1;
        return;
    }

    a.receiver_id = mm.receiver_id;
    if mm.source != DataSource::Jaero && mm.distance_traveled >= 100.0 {
        a.calc_track = if mm.calculated_track != -1.0 {
            mm.calculated_track as f64
        } else {
            bearing(a.lat, a.lon, lat, lon)
        };
    }

    a.prev_lat = a.lat;
    a.prev_lon = a.lon;
    a.prev_pos_time = a.seen_pos;
    a.lat = lat;
    a.lon = lon;
    a.pos_nic = mm.decoded_nic;
    a.pos_rc = mm.decoded_rc;
    a.seen_pos = now;
    a.pos_surface = a.airground_valid.valid() && a.airground == AirGround::Ground;

    if a.pos_reliable(cfg)
        && accept(
            &mut a.pos_reliable_valid,
            mm.source,
            mm,
            a.seen,
            Reduce::Often,
            cfg.beast_reduce_interval,
        )
    {
        if mm.source < DataSource::Tisb {
            a.nac_p_valid.source = DataSource::Invalid;
            a.nac_v_valid.source = DataSource::Invalid;
            a.sil_valid.source = DataSource::Invalid;
            a.gva_valid.source = DataSource::Invalid;
            a.sda_valid.source = DataSource::Invalid;
            a.sil_type = SilType::Invalid;
        }
        if cfg.globe_index {
            a.globe_index = crate::globe::globe_index(lat, lon);
        }
        a.addrtype = mm.addrtype;
        a.addrtype_updated = now;
        let trace_stale = now > a.seen_pos_reliable + TRACE_STALE;
        a.seen_pos_reliable = now;
        a.lat_reliable = lat;
        a.lon_reliable = lon;
        a.pos_nic_reliable = mm.decoded_nic;
        a.pos_rc_reliable = mm.decoded_rc;
        a.surface_cpr_allow_ac_rel = true; // allow aircraft-relative CPR for ground positions
        if a.gs_valid.valid() {
            a.gs_reliable = a.gs;
        }
        if cfg.write_traces {
            crate::trace::trace_add(cfg, &cfg.trace, a, now, trace_stale, &|a, ts| {
                crate::json::trace_details(cfg, a, ts)
            });
        }
        if let Some((ulat, ulon)) = cfg.user_location {
            if mm.receiver_distance == 0.0 {
                mm.receiver_distance = greatcircle(ulat, ulon, a.lat, a.lon, false);
            }
            a.receiver_distance = mm.receiver_distance;
            a.receiver_direction = bearing(ulat, ulon, a.lat, a.lon);
            if mm.source == DataSource::Adsb
                && a.nac_p_valid.valid()
                && a.nac_p >= 2
                && mm.decoded_rc != 0
                && mm.decoded_rc < 5 * 1852
            {
                stats.distance_max = stats.distance_max.max(mm.receiver_distance as u64);
                stats.distance_max_bucket =
                    stats.distance_max_bucket.max(mm.receiver_distance as u64);
                mm.outline_position = true;
            } else if mm.source == DataSource::Adsr {
                mm.outline_position = true;
            }
        }
    }
}

fn update_altitude(
    cfg: &TrackerConfig,
    now: i64,
    a: &mut Aircraft,
    mm: &mut ModesMessage,
    alt: i32,
) {
    let delta = alt - a.baro_alt;
    let mut max_fpm = 12500;
    let mut min_fpm = -12500;
    let low_delta = 300;
    let baro_age = a.baro_alt_valid.age(now);
    let seen = a.seen;

    let mut fpm = 0;
    if delta.abs() >= low_delta {
        fpm = delta * 60 * 10 / ((baro_age / 100) as i32).abs().saturating_add(10);
        if a.geom_rate_valid.valid() && a.geom_rate_valid.age(now) < a.baro_rate_valid.age(now) {
            let slack = 1500 + ((a.geom_rate_valid.age(now) / 2).min(11000)) as i32;
            min_fpm = a.geom_rate - slack;
            max_fpm = a.geom_rate + slack;
        } else if a.baro_rate_valid.valid() {
            let slack = 1500 + ((a.baro_rate_valid.age(now) / 2).min(11000)) as i32;
            min_fpm = a.baro_rate - slack;
            max_fpm = a.baro_rate + slack;
        }
        if a.baro_alt_valid.valid() && baro_age < 30 * SECONDS {
            a.alt_reliable = (ALTITUDE_BARO_RELIABLE_MAX
                - (ALTITUDE_BARO_RELIABLE_MAX as i64 * baro_age / (30 * SECONDS)) as i32)
                .min(a.alt_reliable);
        } else {
            a.alt_reliable = 0;
        }
    }

    let mut good_crc = 0;
    if mm.crc == 0 && mm.source > DataSource::ModeSChecked {
        good_crc = if baro_age < 2 * SECONDS {
            ALTITUDE_BARO_RELIABLE_MAX
        } else {
            ALTITUDE_BARO_RELIABLE_MAX / 3
        };
    }
    if matches!(mm.source, DataSource::Jaero | DataSource::Sbs) {
        good_crc = ALTITUDE_BARO_RELIABLE_MAX;
    }
    if mm.source == DataSource::Mlat {
        // Upstream trusts an MLAT altitude only when the mlat-server reported that several
        // receivers heard it, which arrives with SBS-format input only (not supported here).
        // Beast-fed results from mlat-client carry the altitude the client was given, a poor
        // source: ignore it, except for anonymous (non-ICAO) addresses where no altitude at
        // all would look worse.
        if a.addr & crate::mode_s::NON_ICAO_ADDRESS == 0 {
            return;
        }
        good_crc = 0;
    }

    let discard =
        if a.baro_alt > 50175 && mm.alt_q_bit && a.alt_reliable > ALTITUDE_BARO_RELIABLE_MAX / 4 {
            good_crc = 0;
            true
        } else if delta.abs() < low_delta || (fpm < max_fpm && fpm > min_fpm) {
            false
        } else if good_crc >= a.alt_reliable || mm.source > a.baro_alt_valid.source {
            a.alt_reliable = 0; // reset alt_reliable
            false
        } else {
            true
        };

    if !discard {
        let score_add = good_crc + 1;
        if accept(
            &mut a.baro_alt_valid,
            mm.source,
            mm,
            seen,
            Reduce::Often,
            cfg.beast_reduce_interval,
        ) {
            a.alt_reliable = (a.alt_reliable + score_add).clamp(0, ALTITUDE_BARO_RELIABLE_MAX);
            a.baro_alt = alt;
        }
        return;
    }

    // discard_alt
    if !will_accept(&a.baro_alt_valid, mm.source, now, seen) {
        return;
    }
    a.alt_reliable -= good_crc + 1;
    if a.alt_reliable <= 0 {
        a.alt_reliable = 0;
        if a.pos_reliable_valid.source != DataSource::Jaero {
            a.baro_alt_valid.source = DataSource::Invalid;
        }
    }
    let _ = cfg;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode_s::Decoder;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn feed(d: &mut Decoder, t: &mut Tracker, frame: &str, at: i64) -> ModesMessage {
        let mut mm = ModesMessage::from_bytes(&hex(frame));
        mm.sys_timestamp = at;
        mm.signal_level = 0.01;
        d.decode(&mut mm);
        t.update_from_message(&mut mm);
        mm
    }

    /// A realistic epoch time, so "new aircraft" logic sees a long gap since `seen = 0`.
    const T0: i64 = 1_700_000_000_000;
    const IDENT: &str = "8D4840D6202CC371C32CE0576098";
    const EVEN: &str = "8D40621D58C382D690C8AC2863A7";
    const ODD: &str = "8D40621D58C386435CC412692AD6";
    const VEL: &str = "8D485020994409940838175B284F";

    /// `accept` with an explicit time, for tests of the validity rules alone.
    fn accept(d: &mut Validity, source: DataSource, now: i64, seen: i64) -> bool {
        let mut mm = ModesMessage::from_bytes(&[0u8; 14]);
        mm.sys_timestamp = now;
        super::accept(d, source, &mut mm, seen, Reduce::Often, 250)
    }

    #[test]
    fn validity_priority_rules() {
        let mut d = Validity::default();
        assert!(accept(&mut d, DataSource::ModeS, 1000, 1000));
        // a better source replaces it, a worse one does not while reliable data is fresh
        assert!(accept(&mut d, DataSource::Adsb, 2000, 2000));
        assert!(!accept(&mut d, DataSource::ModeS, 3000, 3000 - 1));
        // ... but once `seen` is 15 s past the update, the worse source gets in
        assert!(accept(&mut d, DataSource::ModeS, 20_000, 20_000));
        assert!(!will_accept(&d, DataSource::Invalid, 30_000, 30_000));
        assert!(
            !will_accept(&d, DataSource::Adsb, 10, 30_000),
            "time went backwards"
        );
    }

    #[test]
    fn validity_expires() {
        let mut v = Validity::default();
        accept(&mut v, DataSource::Adsb, 1000, 1000);
        v.update(1000 + TRACK_STALE + 1, TRACK_EXPIRE);
        assert!(v.valid() && v.stale);
        v.update(1000 + TRACK_EXPIRE + 1, TRACK_EXPIRE);
        assert!(!v.valid());
    }

    #[test]
    fn creates_aircraft_from_reliable_address_only() {
        let mut d = Decoder::new(1);
        let mut t = Tracker::new(TrackerConfig::default());
        // an address/parity message from an unknown aircraft is not attributed
        let mut mm = ModesMessage::from_bytes(&hex("20001938000000"));
        mm.sys_timestamp = 1000;
        d.decode(&mut mm);
        assert!(t.update_from_message(&mut mm).is_none());
        assert!(t.is_empty());
        feed(&mut d, &mut t, IDENT, 2000);
        assert_eq!(t.len(), 1);
        let a = t.get(0x4840D6).unwrap();
        assert_eq!(a.messages, 1);
        assert_eq!(a.callsign, "KLM1023 ");
        assert_eq!(a.category, 0xA0);
        assert_eq!(a.seen, 2000);
    }

    #[test]
    fn json_inclusion_needs_two_messages() {
        let mut d = Decoder::new(1);
        let mut t = Tracker::new(TrackerConfig::default());
        feed(&mut d, &mut t, IDENT, T0 + 2000);
        assert_eq!(t.active().count(), 0, "one message is not enough");
        feed(&mut d, &mut t, IDENT, T0 + 3000);
        assert_eq!(t.active().count(), 1);
        assert_eq!(t.stats.messages_total, 2);
        assert_eq!(t.stats.unique_aircraft, 1);
    }

    #[test]
    fn global_cpr_position_and_reliability() {
        let mut d = Decoder::new(1);
        let mut t = Tracker::new(TrackerConfig::new(Some(52.0), Some(4.0), None));
        feed(&mut d, &mut t, EVEN, 10_000);
        let a = t.get(0x40621D).unwrap();
        assert!(!a.position_valid.valid(), "one frame is not enough");
        assert_eq!(a.baro_alt, 38000);
        feed(&mut d, &mut t, ODD, 10_500);
        let a = t.get(0x40621D).unwrap();
        assert!(a.position_valid.valid());
        assert!(
            (a.lat - 52.26578).abs() < 1e-3 && (a.lon - 3.93891).abs() < 1e-3,
            "{} {}",
            a.lat,
            a.lon
        );
        assert_eq!(t.stats.cpr_global_ok, 1);
        assert_eq!(t.stats.cpr_airborne, 2);
        // json_reliable is 1 with a receiver location, so one global decode is enough
        assert!(a.pos_reliable(&t.cfg));
        assert!(a.pos_reliable_valid.valid());
        assert_eq!(a.pos_nic_reliable, 8);
    }

    #[test]
    fn needs_two_decodes_without_a_receiver_location() {
        let mut d = Decoder::new(1);
        let mut t = Tracker::new(TrackerConfig::default());
        assert_eq!(t.cfg.json_reliable, 2);
        feed(&mut d, &mut t, EVEN, 10_000);
        feed(&mut d, &mut t, ODD, 10_500);
        assert!(
            !t.get(0x40621D).unwrap().pos_reliable(&t.cfg),
            "one decode, odd/even both at 1"
        );
        feed(&mut d, &mut t, EVEN, 11_000);
        feed(&mut d, &mut t, ODD, 11_500);
        // duplicate positions are ignored, so reliability does not climb on repeats
        assert!(t.stats.cpr_global_ok >= 1);
    }

    #[test]
    fn velocity_and_altitude_fields() {
        let mut d = Decoder::new(1);
        let mut t = Tracker::new(TrackerConfig::default());
        feed(&mut d, &mut t, VEL, 5000);
        let a = t.get(0x485020).unwrap();
        assert!(a.gs_valid.valid() && (a.gs - 159.2).abs() < 0.5);
        assert!(a.track_valid.valid() && (a.track - 182.88).abs() < 0.05);
        assert!(a.geom_rate_valid.valid() || a.baro_rate_valid.valid());
        assert_eq!(a.nac_v, 0);
        assert!(a.nac_v_valid.valid());
    }

    #[test]
    fn stale_aircraft_are_removed() {
        let mut d = Decoder::new(1);
        let mut t = Tracker::new(TrackerConfig::default());
        feed(&mut d, &mut t, IDENT, 2000);
        t.remove_stale(2000 + 4 * MINUTES);
        assert_eq!(t.len(), 1);
        t.remove_stale(2000 + 6 * MINUTES);
        assert!(t.is_empty());
        assert_eq!(t.stats.single_message_aircraft, 1);
    }

    #[test]
    fn mode_ac_is_only_counted() {
        let mut t = Tracker::new(TrackerConfig::default());
        let mut mm = ModesMessage::from_mode_a(0x1200);
        mm.sys_timestamp = 1000;
        assert!(t.update_from_message(&mut mm).is_none());
        assert_eq!(t.mode_ac_count[mode_a_to_index(0x1200) as usize], 1);
        assert!(t.is_empty());
    }

    /// A DF20 Comm-B reply from `addr` carrying `mb`, at 38000 ft, with address/parity.
    /// Deliver `frame` as an mlat-client result: a Beast message with the magic MLAT timestamp.
    fn feed_mlat(d: &mut Decoder, t: &mut Tracker, frame: &[u8], at: i64) -> ModesMessage {
        let mut mm = crate::beast::message_from_wire(
            d,
            frame,
            crate::beast::MAGIC_MLAT_TIMESTAMP,
            None,
            at,
            0,
            false,
        )
        .unwrap();
        t.update_from_message(&mut mm);
        mm
    }

    /// `frame` with its first byte replaced and the parity recomputed.
    fn with_first_byte(frame: &str, first: u8) -> Vec<u8> {
        let mut b = hex(frame);
        b[0] = first;
        let n = b.len();
        b[n - 3..].fill(0);
        let c = crate::crc::checksum(&b);
        b[n - 3] = (c >> 16) as u8;
        b[n - 2] = (c >> 8) as u8;
        b[n - 1] = c as u8;
        b
    }

    #[test]
    fn mlat_results_give_a_position_but_no_altitude_and_no_cpr_stats() {
        let mut d = Decoder::new(1);
        let mut t = Tracker::new(TrackerConfig::new(Some(52.0), Some(4.0), None));
        let mm = feed_mlat(&mut d, &mut t, &hex(EVEN), 10_000);
        assert_eq!(mm.source, DataSource::Mlat);
        feed_mlat(&mut d, &mut t, &hex(ODD), 10_500);
        let a = t.get(0x40621D).unwrap();
        assert_eq!(a.addrtype, crate::mode_s::AddrType::Mlat);
        assert_eq!(a.position_valid.source, DataSource::Mlat);
        assert!(
            (a.lat - 52.26578).abs() < 1e-3 && (a.lon - 3.93891).abs() < 1e-3,
            "{} {}",
            a.lat,
            a.lon
        );
        // an MLAT position is reliable at once, whatever json_reliable says
        assert!(a.pos_reliable(&t.cfg));
        assert_eq!(a.pos_reliable_valid.source, DataSource::Mlat);
        // the frames carry 38000 ft, but a Beast-fed MLAT altitude is not trusted
        assert!(
            !a.baro_alt_valid.valid(),
            "altitude from an ICAO-address MLAT result must be ignored"
        );
        // MLAT results are not counted in the CPR statistics
        assert_eq!(t.stats.cpr_airborne, 0);
        assert_eq!(t.stats.cpr_global_ok, 0);
        assert_eq!(t.stats.messages_total, 2);
        let j = crate::json::aircraft_object(&t, a, 11_000);
        assert!(j.contains("\"type\":\"mlat\""), "{j}");
        assert!(
            j.contains("\"mlat\":[\"lat\",\"lon\",\"nic\",\"rc\""),
            "{j}"
        );
        assert!(j.contains("\"tisb\":[]"), "{j}");
        assert!(!j.contains("alt_baro"), "{j}");
    }

    #[test]
    fn anonymous_mlat_results_keep_their_altitude() {
        // DF18 with CF=1: ADS-B with a non-ICAO address, as mlat-client sends anonymous results
        let even = with_first_byte(EVEN, 0x91);
        let odd = with_first_byte(ODD, 0x91);
        let mut d = Decoder::new(1);
        let mut t = Tracker::new(TrackerConfig::new(Some(52.0), Some(4.0), None));
        feed_mlat(&mut d, &mut t, &even, 10_000);
        feed_mlat(&mut d, &mut t, &odd, 10_500);
        let a = t
            .get(0x40621D | crate::mode_s::NON_ICAO_ADDRESS)
            .expect("aircraft with a non-ICAO address");
        assert_eq!(a.addrtype, crate::mode_s::AddrType::Mlat);
        assert!(a.position_valid.valid());
        assert!(
            a.baro_alt_valid.valid(),
            "anonymous MLAT keeps its altitude"
        );
        assert_eq!(a.baro_alt, 38000);
        assert_eq!(a.baro_alt_valid.source, DataSource::Mlat);
    }

    /// An aircraft at `dist_nm` in direction `dir` from the receiver, with given reliability.
    fn at(dir: f64, dist_nm: f64, reliable: f32, alt: i32) -> Aircraft {
        let mut a = Aircraft::new(0x400000);
        a.receiver_direction = dir;
        a.receiver_distance = dist_nm * 1852.0;
        a.lat = 51.0 + dist_nm / 100.0;
        a.lon = 0.5;
        a.pos_reliable_odd = reliable;
        a.pos_reliable_even = reliable;
        a.baro_alt = alt;
        a.baro_alt_valid.source = DataSource::Adsb;
        a
    }

    #[test]
    fn range_outline_keeps_the_farthest_point_per_direction() {
        let day = 24 * HOURS;
        let mut o = RangeOutline::new(day);
        let t = 1_000;
        o.update(&at(90.0, 40.0, 4.0, 30000), t, 1);
        o.update(&at(90.2, 60.0, 4.0, 35000), t, 1); // same 1-degree direction, farther
        o.update(&at(89.8, 50.0, 4.0, 1000), t, 1); // nearer: ignored
        o.update(&at(359.6, 10.0, 4.0, 2000), t, 1); // rounds to 360, i.e. direction 0
        let p = o.points();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].alt, 2000, "direction 0 comes first");
        assert_eq!(p[1].alt, 35000);
        assert!((f64::from(p[1].distance) - 60.0 * 1852.0).abs() < 1.0);

        // an unreliable position may extend a direction by at most 50 nmi over its maximum
        o.update(&at(90.0, 200.0, 1.0, 1), t, 1);
        assert_eq!(
            o.points()[1].alt,
            35000,
            "200 nmi from a 60 nmi maximum is rejected"
        );
        o.update(&at(90.0, 100.0, 1.0, 1), t, 1);
        assert_eq!(o.points()[1].alt, 1, "100 nmi is within 60 + 50");

        // a slice is cleared when time comes back to it, so old maxima age out
        // one slice is a 63rd of the window; round up so k * slice is in slice k
        let slice = day / (RANGEDIRS_IVALS as i64 - 1) + 1;
        let mut o = RangeOutline::new(day);
        o.update(&at(10.0, 80.0, 4.0, 5), 0, 1);
        o.update(&at(10.0, 20.0, 4.0, 6), slice, 1);
        assert_eq!(
            o.points()[0].alt,
            5,
            "the old slice still holds the farther point"
        );
        for k in 2..=RANGEDIRS_IVALS as i64 {
            o.rotate(k * slice);
        }
        assert_eq!(
            o.points()[0].alt,
            6,
            "after a full cycle only the newer point is left"
        );

        // a message a little behind a slice boundary the tick already crossed does not clear
        // its own slice (the tick and the messages use slightly different clocks)
        let mut o2 = RangeOutline::new(day);
        o2.update(&at(20.0, 70.0, 4.0, 7), 3 * slice - 1_000, 1);
        o2.rotate(3 * slice + 10); // tick: next slice
        o2.update(&at(30.0, 10.0, 4.0, 8), 3 * slice - 200, 1); // late message, old slice
        assert_eq!(o2.points().len(), 2, "both points kept");
        // a clock set back by hours starts over instead of freezing the window
        o2.rotate(3 * slice + 10 * HOURS); // clock a few hours ahead
        o2.update(&at(40.0, 50.0, 4.0, 9), 3 * slice + 10 * HOURS, 1);
        o2.rotate(5 * slice); // set back
        o2.update(&at(50.0, 30.0, 4.0, 10), 5 * slice, 1);
        o2.rotate(6 * slice); // and forward again from there: slice 6 is cleared normally
        assert!(
            o2.points().iter().any(|p| p.alt == 10),
            "new data after the step is recorded"
        );

        let j = crate::json::outline_json(&o);
        assert!(
            j.starts_with("{ \"actualRange\": { \"last24h\": { \"points\": [\n["),
            "{j}"
        );
        assert!(j.ends_with("6]\n]}}}\n"), "{j}");
        let empty = crate::json::outline_json(&RangeOutline::new(day));
        assert_eq!(
            empty,
            "{ \"actualRange\": { \"last24h\": { \"points\": [\n]}}}\n"
        );
    }

    #[test]
    fn range_outline_state_round_trips_in_upstreams_format() {
        let day = 24 * HOURS;
        let slice = day / (RANGEDIRS_IVALS as i64 - 1) + 1;
        let t0 = 1_794_200_000_000; // a real clock time, so the slice number wraps
        let mut o = RangeOutline::new(day);
        o.update(&at(90.0, 60.0, 4.0, 35000), t0, 1);
        o.update(&at(180.0, 120.0, 4.0, 2000), t0 + 2 * slice, 1);
        let bytes = o.to_state_bytes();
        assert_eq!(bytes.len(), RangeOutline::STATE_LEN);
        assert_eq!(
            bytes.len(),
            368_644,
            "upstream: sizeof(int) + RANGEDIRSSIZE"
        );
        let cur = i32::from_le_bytes(bytes[..4].try_into().unwrap());
        let abs = (t0 + 2 * slice) * (RANGEDIRS_IVALS as i64 - 1) / day;
        assert_eq!(i64::from(cur), abs % RANGEDIRS_IVALS as i64);
        // one entry, by hand: slice of t0, direction 90, packed f32 f32 f32 i32
        let s0 = ((t0 * (RANGEDIRS_IVALS as i64 - 1) / day) % RANGEDIRS_IVALS as i64) as usize;
        let at90 = 4 + (s0 * RANGEDIRS_BUCKETS + 90) * 16;
        let f = |i: usize| f32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        assert!((f(at90) - 60.0 * 1852.0).abs() < 1.0);
        assert_eq!(
            i32::from_le_bytes(bytes[at90 + 12..at90 + 16].try_into().unwrap()),
            35000
        );

        // restarted a minute later: everything is back, and the next update keeps both
        let saved_at = t0 + 2 * slice;
        let mut r = RangeOutline::new(day);
        r.load_state_bytes(&bytes, saved_at, saved_at + MINUTES)
            .unwrap();
        assert_eq!(r.points(), o.points());
        assert_eq!(r.to_state_bytes(), bytes, "same state written back");
        r.update(&at(270.0, 30.0, 4.0, 9), saved_at + MINUTES, 1);
        assert_eq!(r.points().len(), 3);

        // down for 10 slices: only the slices passed through are cleared (t0's survives,
        // the window is 64 slices)
        let mut r = RangeOutline::new(day);
        r.load_state_bytes(&bytes, saved_at, saved_at + 10 * slice)
            .unwrap();
        assert_eq!(r.points().len(), 2, "nothing older than the window yet");
        // down for longer than the window: nothing is kept
        let mut r = RangeOutline::new(day);
        r.load_state_bytes(&bytes, saved_at, saved_at + 2 * day)
            .unwrap();
        assert!(r.points().is_empty(), "a day-old outline is not shown");
        // down for most of the window: t0's slice was passed through and cleared, the
        // newer one not yet
        let mut r = RangeOutline::new(day);
        r.load_state_bytes(&bytes, saved_at, t0 + 64 * slice)
            .unwrap();
        assert_eq!(r.points().len(), 1);
        assert_eq!(r.points()[0].alt, 2000);

        // resetRangeOutline: everything goes, recording carries on in the same slice
        let mut r = RangeOutline::new(day);
        r.load_state_bytes(&bytes, saved_at, saved_at).unwrap();
        r.reset();
        assert!(r.points().is_empty());
        r.update(&at(10.0, 5.0, 4.0, 11), saved_at, 1);
        assert_eq!(r.points().len(), 1);
        assert_eq!(&r.to_state_bytes()[..4], &bytes[..4], "same current slice");

        // broken files are refused and change nothing
        let mut r = RangeOutline::new(day);
        assert!(r.load_state_bytes(&bytes[1..], saved_at, saved_at).is_err());
        let mut bad = bytes.clone();
        bad[..4].copy_from_slice(&64i32.to_le_bytes());
        assert!(r.load_state_bytes(&bad, saved_at, saved_at).is_err());
        assert!(r.points().is_empty());
    }

    /// A DF11 all-call reply (IID 0) for `addr` with a correct parity field.
    fn df11(addr: u32) -> Vec<u8> {
        let mut b = vec![
            0x5D,
            (addr >> 16) as u8,
            (addr >> 8) as u8,
            addr as u8,
            0,
            0,
            0,
        ];
        let c = crate::crc::checksum(&b);
        b[4] = (c >> 16) as u8;
        b[5] = (c >> 8) as u8;
        b[6] = c as u8;
        b
    }

    #[test]
    fn beast_reduce_pacing_follows_upstream() {
        let mut d = Decoder::new(1);
        let mut t = Tracker::new(TrackerConfig::new(Some(52.0), Some(4.0), None));
        assert_eq!(t.cfg.beast_reduce_interval, 250);
        let fwd =
            |d: &mut Decoder, t: &mut Tracker, f: &str, at: i64| feed(d, t, f, at).reduce_forward;
        // measurements: forwarded when new, then every 7/8 of the interval (219 ms)
        assert!(fwd(&mut d, &mut t, VEL, 5000), "first velocity");
        assert!(
            !fwd(&mut d, &mut t, VEL, 5100),
            "repeat inside the interval"
        );
        assert!(fwd(&mut d, &mut t, VEL, 5300), "after the interval");
        // the ident is both a rare field and a status message: every 4 intervals (1 s)
        assert!(fwd(&mut d, &mut t, IDENT, 6000));
        assert!(!fwd(&mut d, &mut t, IDENT, 6500));
        assert!(fwd(&mut d, &mut t, IDENT, 7100));
        // DF11 all-call replies: one every 4 intervals
        let df11_at = |d: &mut Decoder, t: &mut Tracker, at: i64| {
            let mut mm = ModesMessage::from_bytes(&df11(0x4840D6));
            mm.sys_timestamp = at;
            mm.signal_level = 0.01;
            d.decode(&mut mm);
            assert!(mm.decode_result >= 0, "DF11 decodes");
            t.update_from_message(&mut mm);
            mm.reduce_forward
        };
        assert!(df11_at(&mut d, &mut t, 20_000));
        assert!(!df11_at(&mut d, &mut t, 20_500));
        assert!(df11_at(&mut d, &mut t, 21_100));
        // positions: a CPR half is new, its repeat 50 ms later is not
        assert!(fwd(&mut d, &mut t, EVEN, 30_000));
        assert!(!fwd(&mut d, &mut t, EVEN, 30_050));
        assert!(fwd(&mut d, &mut t, ODD, 30_500), "completes a position");
        // a frame that does not decode (three bits flipped, beyond repair) creates nothing
        // and is never forwarded, even when every field's interval has passed
        let mut bad = hex(VEL);
        bad[5] ^= 0x80;
        bad[7] ^= 0x10;
        bad[9] ^= 0x02;
        let mut mm = ModesMessage::from_bytes(&bad);
        mm.sys_timestamp = 50_000;
        d.decode(&mut mm);
        t.update_from_message(&mut mm);
        assert!(mm.decode_result < 0, "the corrupt frame is rejected");
        assert!(!mm.reduce_forward);
    }

    fn df20(addr: u32, mb: &str) -> String {
        // DF20, FS 0, DR 0, UM 0, AC13 = Q-bit altitude 38000 ft
        let n = ((38000 + 1000) / 25) as u32;
        let ac13 = ((n & 0x7E0) << 2) | ((n & 0x10) << 1) | 0x10 | (n & 0x0F);
        let mut f = vec![0xA0, 0x00, ((ac13 >> 8) & 0x1F) as u8, ac13 as u8];
        f.extend(hex(mb));
        f.extend([0, 0, 0]);
        let p = crate::crc::checksum(&f) ^ addr;
        let n = f.len();
        f[n - 3] = (p >> 16) as u8;
        f[n - 2] = (p >> 8) as u8;
        f[n - 1] = p as u8;
        f.iter().map(|b| format!("{b:02X}")).collect()
    }

    #[test]
    fn comm_b_fills_roll_track_rate_tas_mach_and_derived_values() {
        let mut d = Decoder::new(1);
        let mut t = Tracker::new(TrackerConfig::new(Some(52.0), Some(4.0), None));
        // the ADS-B position messages teach the ICAO address and give position and altitude
        feed(&mut d, &mut t, EVEN, T0);
        feed(&mut d, &mut t, ODD, T0 + 500);
        let addr = 0x40621D;
        assert!(t.get(addr).unwrap().position_valid.valid());

        // BDS 5,0 then BDS 6,0 (heading 120.06 magnetic, IAS 265, Mach 0.78) 300 ms later
        let bds50 = df20(addr, "81951536E024D4");
        let bds60 = df20(addr, "AABA1330FFF7FD");
        feed(&mut d, &mut t, &bds50, T0 + 1000);
        let a = t.get(addr).unwrap();
        assert!(
            a.roll_valid.valid() && (a.roll - 2.1).abs() < 0.05,
            "roll {}",
            a.roll
        );
        assert!((a.track_rate - 0.125).abs() < 1e-6 && a.track_rate_valid.valid());
        assert_eq!(a.tas, 424);
        assert_eq!(a.gs, 438.0);
        assert!((a.track - 114.258).abs() < 0.001);

        feed(&mut d, &mut t, &bds60, T0 + 1300);
        let a = t.get(addr).unwrap();
        assert_eq!(a.ias, 265);
        assert!((a.mach - 0.78).abs() < 1e-6 && a.mach_valid.valid());
        assert!((a.mag_heading - 120.0586).abs() < 0.001);
        // true heading = magnetic heading + declination at the aircraft's position
        let dec = crate::geomag::field(
            38000.0 * 0.0003048,
            a.lat,
            a.lon,
            crate::geomag::decimal_year(T0 + 1300),
        )
        .declination;
        assert!(dec.abs() > 0.1, "a non-trivial declination, got {dec}");
        let want = norm_angle(120.0586 + dec, 180.0);
        assert!(
            a.true_heading_valid.valid() && (a.true_heading as f64 - want).abs() < 0.01,
            "true heading {} want {want}",
            a.true_heading
        );
        assert_eq!(a.true_heading_valid.source, DataSource::Indirect);
        // TAS 424 kt and Mach 0.78 give an outside air temperature of about -78.5 C
        assert!((a.oat - -78.5).abs() < 0.5, "oat {}", a.oat);
        assert!(a.tat > a.oat);
        // steady flight with TAS, GS, heading and track fresh: a wind estimate exists
        assert!(a.wind_updated == T0 + 1300, "wind not estimated");
        assert!(a.wind_speed.is_finite() && a.wind_speed <= 250.0);
        let j = crate::json::aircraft_object(&t, t.get(addr).unwrap(), T0 + 1500);
        for needle in [
            "\"ias\":265",
            "\"mach\":0.780",
            "\"tas\":424",
            "\"roll\":2.",
            "\"track_rate\":0.12",
            "\"oat\":-79",
            "\"ws\":",
            "\"wd\":",
        ] {
            assert!(j.contains(needle), "{needle} missing in {j}");
        }
    }

    #[test]
    fn nic_and_rc_tables() {
        assert_eq!(compute_nic(11, 2, true, true, false), 9);
        assert_eq!(compute_nic(11, 0, true, true, false), 8);
        assert_eq!(compute_rc(11, 2, true, true, false), 75);
        assert_eq!(compute_rc(13, 2, false, true, false), 556);
        assert_eq!(compute_rc(16, 0, false, false, false), 18520);
        assert_eq!(compute_rc(16, 1, true, false, false), 7408);
        assert_eq!(compute_rc(99, 2, false, false, false), 0);
    }
}
