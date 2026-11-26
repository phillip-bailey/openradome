//! JSON documents for tar1090 and friends: `aircraft.json`, `receiver.json`, `stats.json`.
//! Port of the relevant parts of upstream `json_out.c` and `stats.c`.

use std::collections::VecDeque;
use std::fmt::Write;

use crate::demod::DemodStats;
use crate::mode_s::{AddrType, AirGround, DataSource, Emergency, SilType, nav_mode};
use crate::track::{Aircraft, AircraftCounts, HOURS, TRACK_EXPIRE, TrackStats, Tracker, Validity};

/// `type` field of an aircraft: where its address came from.
pub fn addrtype_str(t: AddrType) -> &'static str {
    match t {
        AddrType::AdsbIcao => "adsb_icao",
        AddrType::AdsbIcaoNt => "adsb_icao_nt",
        AddrType::AdsrIcao => "adsr_icao",
        AddrType::TisbIcao => "tisb_icao",
        AddrType::Jaero => "adsc",
        AddrType::Mlat => "mlat",
        AddrType::Other => "other",
        AddrType::ModeS => "mode_s",
        AddrType::AdsbOther => "adsb_other",
        AddrType::AdsrOther => "adsr_other",
        AddrType::TisbTrackfile => "tisb_trackfile",
        AddrType::TisbOther => "tisb_other",
        AddrType::ModeA => "mode_ac",
        AddrType::Unknown => "unknown",
    }
}

/// Every address type, in upstream's enum order.
const ALL_TYPES: [AddrType; 14] = [
    AddrType::AdsbIcao,
    AddrType::AdsbIcaoNt,
    AddrType::AdsrIcao,
    AddrType::TisbIcao,
    AddrType::Jaero,
    AddrType::Mlat,
    AddrType::Other,
    AddrType::ModeS,
    AddrType::AdsbOther,
    AddrType::AdsrOther,
    AddrType::TisbTrackfile,
    AddrType::TisbOther,
    AddrType::ModeA,
    AddrType::Unknown,
];

fn emergency_str(e: Emergency) -> &'static str {
    match e {
        Emergency::None => "none",
        Emergency::General => "general",
        Emergency::Lifeguard => "lifeguard",
        Emergency::MinFuel => "minfuel",
        Emergency::NoRadio => "nordo",
        Emergency::Unlawful => "unlawful",
        Emergency::Downed => "downed",
        Emergency::Reserved => "reserved",
    }
}

fn sil_type_str(t: SilType) -> &'static str {
    match t {
        SilType::Unknown => "unknown",
        SilType::PerHour => "perhour",
        SilType::PerSample => "persample",
        SilType::Invalid => "invalid",
    }
}

/// Escape a string for a JSON string literal (upstream escapes `"`, `\` and control chars).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn nav_modes_list(flags: u8) -> String {
    const NAMES: [(u8, &str); 6] = [
        (nav_mode::AUTOPILOT, "autopilot"),
        (nav_mode::VNAV, "vnav"),
        (nav_mode::ALT_HOLD, "althold"),
        (nav_mode::APPROACH, "approach"),
        (nav_mode::LNAV, "lnav"),
        (nav_mode::TCAS, "tcas"),
    ];
    NAMES
        .iter()
        .filter(|(f, _)| flags & f != 0)
        .map(|(_, n)| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(",")
}

/// List the fields whose current value came from `source` (the `mlat` / `tisb` arrays).
fn flags(a: &Aircraft, source: DataSource) -> String {
    let mut v: Vec<&str> = Vec::new();
    let mut add = |valid: &Validity, names: &[&'static str]| {
        if valid.source == source {
            v.extend_from_slice(names);
        }
    };
    add(&a.callsign_valid, &["callsign"]);
    add(&a.baro_alt_valid, &["altitude", "alt_baro"]);
    add(&a.geom_alt_valid, &["alt_geom"]);
    add(&a.gs_valid, &["gs"]);
    add(&a.ias_valid, &["ias"]);
    add(&a.tas_valid, &["tas"]);
    add(&a.mach_valid, &["mach"]);
    add(&a.track_valid, &["track"]);
    add(&a.track_rate_valid, &["track_rate"]);
    add(&a.roll_valid, &["roll"]);
    add(&a.mag_heading_valid, &["mag_heading"]);
    add(&a.true_heading_valid, &["true_heading"]);
    add(&a.baro_rate_valid, &["baro_rate"]);
    add(&a.geom_rate_valid, &["geom_rate"]);
    add(&a.squawk_valid, &["squawk"]);
    add(&a.emergency_valid, &["emergency"]);
    add(&a.nav_qnh_valid, &["nav_qnh"]);
    add(&a.nav_altitude_mcp_valid, &["nav_altitude_mcp"]);
    add(&a.nav_altitude_fms_valid, &["nav_altitude_fms"]);
    add(&a.nav_heading_valid, &["nav_heading"]);
    add(&a.nav_modes_valid, &["nav_modes"]);
    add(&a.pos_reliable_valid, &["lat", "lon", "nic", "rc"]);
    add(&a.nic_baro_valid, &["nic_baro"]);
    add(&a.nac_p_valid, &["nac_p"]);
    add(&a.nac_v_valid, &["nac_v"]);
    add(&a.sil_valid, &["sil", "sil_type"]);
    add(&a.gva_valid, &["gva"]);
    add(&a.sda_valid, &["sda"]);
    format!(
        "[{}]",
        v.iter()
            .map(|n| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// One aircraft as a JSON object, as in upstream `sprintAircraftObject` (print mode 0).
pub fn aircraft_object(t: &Tracker, a: &Aircraft, now: i64) -> String {
    object(&t.cfg, a, now, false)
}

/// The aircraft object shown with every fourth trace point (upstream's `printMode == 1`): the same
/// fields as `aircraft.json` without the hex address, altitude, speed, position block and counters.
pub fn trace_details(cfg: &crate::track::TrackerConfig, a: &Aircraft, now: i64) -> String {
    object(cfg, a, now, true)
}

fn object(cfg: &crate::track::TrackerConfig, a: &Aircraft, now: i64, trace: bool) -> String {
    let mut s = String::with_capacity(512);
    if trace {
        let _ = write!(s, "{{\"type\":\"{}\"", addrtype_str(a.addrtype));
    } else {
        let _ = write!(
            s,
            "{{\"hex\":\"{}{:06x}\",\"type\":\"{}\"",
            if a.addr & crate::mode_s::NON_ICAO_ADDRESS != 0 {
                "~"
            } else {
                ""
            },
            a.addr & 0xFF_FFFF,
            addrtype_str(a.addrtype)
        );
    }
    if a.callsign_valid.valid() {
        let _ = write!(s, ",\"flight\":\"{}\"", escape(&a.callsign));
    }
    if trace {
    } else if a.airground_valid.valid() && a.airground == AirGround::Ground {
        s.push_str(",\"alt_baro\":\"ground\"");
    } else if a.alt_baro_reliable(cfg) {
        let _ = write!(s, ",\"alt_baro\":{}", a.baro_alt);
    }
    if a.geom_alt_valid.valid() {
        let _ = write!(s, ",\"alt_geom\":{}", a.geom_alt);
    }
    if !trace && a.gs_valid.valid() {
        let _ = write!(s, ",\"gs\":{:.1}", a.gs as f64);
    }
    if a.ias_valid.valid() {
        let _ = write!(s, ",\"ias\":{}", a.ias);
    }
    if a.tas_valid.valid() {
        let _ = write!(s, ",\"tas\":{}", a.tas);
    }
    if a.mach_valid.valid() {
        let _ = write!(s, ",\"mach\":{:.3}", a.mach);
    }
    if now < a.wind_updated + TRACK_EXPIRE && (a.wind_altitude - a.baro_alt).abs() < 500 {
        let _ = write!(
            s,
            ",\"wd\":{:.0},\"ws\":{:.0}",
            a.wind_direction as f64, a.wind_speed as f64
        );
    }
    if now < a.oat_updated + TRACK_EXPIRE {
        let _ = write!(s, ",\"oat\":{:.0}", a.oat);
    }
    if now < a.tat_updated + TRACK_EXPIRE {
        let _ = write!(s, ",\"tat\":{:.0}", a.tat);
    }
    if a.track_valid.valid() {
        let _ = write!(s, ",\"track\":{:.2}", a.track as f64);
    } else if !trace
        && a.pos_reliable_valid.valid()
        && !(a.airground_valid.valid() && a.airground == AirGround::Ground)
    {
        let _ = write!(s, ",\"calc_track\":{:.0}", a.calc_track);
    }
    if a.track_rate_valid.valid() {
        let _ = write!(s, ",\"track_rate\":{:.2}", a.track_rate as f64);
    }
    if a.roll_valid.valid() {
        let _ = write!(s, ",\"roll\":{:.2}", a.roll as f64);
    }
    if a.mag_heading_valid.valid() {
        let _ = write!(s, ",\"mag_heading\":{:.2}", a.mag_heading as f64);
    }
    if a.true_heading_valid.valid() {
        let _ = write!(s, ",\"true_heading\":{:.2}", a.true_heading as f64);
    }
    if a.baro_rate_valid.valid() {
        let _ = write!(s, ",\"baro_rate\":{}", a.baro_rate);
    }
    if a.geom_rate_valid.valid() {
        let _ = write!(s, ",\"geom_rate\":{}", a.geom_rate);
    }
    if a.squawk_valid.valid() {
        let _ = write!(s, ",\"squawk\":\"{:04x}\"", a.squawk);
    }
    if a.emergency_valid.valid() {
        let _ = write!(s, ",\"emergency\":\"{}\"", emergency_str(a.emergency));
    }
    if a.category != 0 {
        let _ = write!(s, ",\"category\":\"{:02X}\"", a.category);
    }
    if a.nav_qnh_valid.valid() {
        let _ = write!(s, ",\"nav_qnh\":{:.1}", a.nav_qnh as f64);
    }
    if a.nav_altitude_mcp_valid.valid() {
        let _ = write!(s, ",\"nav_altitude_mcp\":{}", a.nav_altitude_mcp);
    }
    if a.nav_altitude_fms_valid.valid() {
        let _ = write!(s, ",\"nav_altitude_fms\":{}", a.nav_altitude_fms);
    }
    if a.nav_heading_valid.valid() {
        let _ = write!(s, ",\"nav_heading\":{:.2}", a.nav_heading as f64);
    }
    if a.nav_modes_valid.valid() {
        let _ = write!(s, ",\"nav_modes\":[{}]", nav_modes_list(a.nav_modes));
    }
    if trace {
        if a.pos_reliable_valid.valid() {
            let _ = write!(
                s,
                ",\"nic\":{},\"rc\":{}",
                a.pos_nic_reliable, a.pos_rc_reliable
            );
        }
    } else if a.pos_reliable_valid.valid() {
        let seen_pos = if now < a.pos_reliable_valid.updated {
            0.0
        } else {
            (now - a.pos_reliable_valid.updated) as f64 / 1000.0
        };
        let _ = write!(
            s,
            ",\"lat\":{:.6},\"lon\":{:.6},\"nic\":{},\"rc\":{},\"seen_pos\":{:.3}",
            a.lat_reliable, a.lon_reliable, a.pos_nic_reliable, a.pos_rc_reliable, seen_pos
        );
        if cfg.user_location.is_some() {
            let _ = write!(
                s,
                ",\"r_dst\":{:.3},\"r_dir\":{:.1}",
                a.receiver_distance / 1852.0,
                a.receiver_direction
            );
        }
    } else if now < a.seen_pos_reliable + 14 * 24 * HOURS {
        let seen_pos = if now < a.seen_pos_reliable {
            0.0
        } else {
            (now - a.seen_pos_reliable) as f64 / 1000.0
        };
        let _ = write!(
            s,
            ",\"lastPosition\":{{\"lat\":{:.6},\"lon\":{:.6},\"nic\":{},\"rc\":{},\"seen_pos\":{:.3}}}",
            a.lat_reliable, a.lon_reliable, a.pos_nic_reliable, a.pos_rc_reliable, seen_pos
        );
    }
    if a.adsb_version >= 0 {
        let _ = write!(s, ",\"version\":{}", a.adsb_version);
    }
    if a.nic_baro_valid.valid() {
        let _ = write!(s, ",\"nic_baro\":{}", a.nic_baro as u8);
    }
    if a.nac_p_valid.valid() {
        let _ = write!(s, ",\"nac_p\":{}", a.nac_p);
    }
    if a.nac_v_valid.valid() {
        let _ = write!(s, ",\"nac_v\":{}", a.nac_v);
    }
    if a.sil_valid.valid() {
        let _ = write!(s, ",\"sil\":{}", a.sil);
    }
    if a.sil_type != SilType::Invalid {
        let _ = write!(s, ",\"sil_type\":\"{}\"", sil_type_str(a.sil_type));
    }
    if a.gva_valid.valid() {
        let _ = write!(s, ",\"gva\":{}", a.gva);
    }
    if a.sda_valid.valid() {
        let _ = write!(s, ",\"sda\":{}", a.sda);
    }
    if a.alert_valid.valid() {
        let _ = write!(s, ",\"alert\":{}", a.alert as u8);
    }
    if a.spi_valid.valid() {
        let _ = write!(s, ",\"spi\":{}", a.spi as u8);
    }
    if trace {
        s.push('}');
        return s;
    }
    let seen = if now < a.seen {
        0.0
    } else {
        (now - a.seen) as f64 / 1000.0
    };
    let _ = write!(
        s,
        ",\"mlat\":{},\"tisb\":{},\"messages\":{},\"seen\":{:.1},\"rssi\":{:.1}}}",
        flags(a, DataSource::Mlat),
        flags(a, DataSource::Tisb),
        a.messages,
        seen,
        a.signal_db() as f64
    );
    s
}

/// The whole `aircraft.json` document.
pub fn aircraft_json(t: &Tracker, now: i64) -> String {
    let mut s = format!(
        "{{ \"now\" : {:.3},\n  \"messages\" : {},\n  \"aircraft\" : [",
        now as f64 / 1000.0,
        t.stats.messages_total
    );
    let mut first = true;
    for a in t.active() {
        if !Tracker::include_in_json(now, a) {
            continue;
        }
        if !first {
            s.push(',');
        }
        first = false;
        s.push('\n');
        s.push_str(&aircraft_object(t, a, now));
    }
    s.push_str("\n  ]\n}\n");
    s
}

/// Settings shown in `receiver.json`.
#[derive(Debug, Clone)]
pub struct ReceiverInfo {
    pub refresh_ms: u32,
    pub location: Option<(f64, f64)>,
    pub version: String,
    /// `outline.json` is written (`"outlineJson": true`).
    pub outline: bool,
    /// `--tar1090-use-api`: `"reapi": true`.
    pub reapi: bool,
    /// Traces are written: `"haveTraces": true` and the trace interval in seconds.
    pub trace_interval_s: Option<f64>,
    /// The globe index is written: `globeIndexGrid` and the special tiles.
    pub globe: bool,
}

pub fn receiver_json(r: &ReceiverInfo) -> String {
    let mut s = format!("{{ \"refresh\": {}, \"history\": 1", r.refresh_ms);
    if let Some((lat, lon)) = r.location {
        let _ = write!(s, ", \"lat\": {lat:.6}, \"lon\": {lon:.6}");
    }
    let _ = write!(
        s,
        ", \"jaeroTimeout\": {:.1}, \"readsb\": true",
        crate::track::TRACK_EXPIRE_JAERO as f64 / (60.0 * 1000.0),
    );
    if let Some(i) = r.trace_interval_s {
        let _ = write!(s, ", \"haveTraces\": true, \"json_trace_interval\": {i:.1}");
    }
    if r.globe {
        let _ = write!(
            s,
            ", \"globeIndexGrid\": {}, \"globeIndexSpecialTiles\": [ ",
            crate::globe::GRID
        );
        let tiles: Vec<String> = crate::globe::SPECIAL_TILES
            .iter()
            .map(|t| {
                format!(
                    "{{\"south\":{},\"east\":{},\"north\":{},\"west\":{}}}",
                    t.south, t.east, t.north, t.west
                )
            })
            .collect();
        s.push_str(&tiles.join(","));
        s.push_str(" ]");
    }
    if r.reapi {
        s.push_str(", \"reapi\": true");
    }
    // the binary files are always written, in the zstd form
    s.push_str(", \"binCraft\": true, \"zstd\": true");
    if r.outline {
        s.push_str(", \"outlineJson\": true");
    }
    let _ = writeln!(s, ", \"version\": \"{}\" }}", escape(&r.version));
    s
}

/// `outline.json`: the farthest position per direction over the outline window, in
/// upstream's format (tar1090 draws it as the receiver's actual range).
pub fn outline_json(o: &crate::track::RangeOutline) -> String {
    let mut s = String::from("{ \"actualRange\": { \"last24h\": { \"points\": [");
    let points: Vec<String> = o
        .points()
        .iter()
        .map(|p| {
            format!(
                "\n[{:.4},{:.4},{}]",
                f64::from(p.lat),
                f64::from(p.lon),
                p.alt
            )
        })
        .collect();
    s.push_str(&points.join(","));
    s.push_str("\n]}}}\n");
    s
}

// ---------------------------------------------------------------------------
// stats.json
// ---------------------------------------------------------------------------

/// Everything `stats.json` reports for one time window.
#[derive(Debug, Clone, Default)]
pub struct StatsBlock {
    pub start: i64,
    pub end: i64,
    pub samples_processed: u64,
    pub samples_lost: u64,
    pub samples_dropped: u64,
    pub demod: DemodStats,
    pub track: TrackStats,
}

macro_rules! sum_fields {
    ($out:expr, $a:expr, $b:expr, $op:tt; $($f:ident),+) => { $( $out.$f = $a.$f $op $b.$f; )+ };
}

fn diff_demod(a: &DemodStats, b: &DemodStats) -> DemodStats {
    let mut o = DemodStats::default();
    sum_fields!(o, a, b, -; demod_preambles, demod_rejected_bad, demod_rejected_unknown_icao,
        demod_modeac, signal_power_sum, signal_power_count, noise_power_sum, noise_power_count,
        strong_signal_count);
    for i in 0..3 {
        o.demod_accepted[i] = a.demod_accepted[i] - b.demod_accepted[i];
    }
    for i in 0..5 {
        o.demod_preamble_phase[i] = a.demod_preamble_phase[i] - b.demod_preamble_phase[i];
        o.demod_best_phase[i] = a.demod_best_phase[i] - b.demod_best_phase[i];
    }
    o.peak_signal_power = a.peak_signal_power;
    o
}

fn diff_track(a: &TrackStats, b: &TrackStats) -> TrackStats {
    let mut o = TrackStats::default();
    sum_fields!(o, a, b, -; messages_total, cpr_surface, cpr_airborne, cpr_global_ok, cpr_global_bad,
        cpr_global_range_checks, cpr_global_speed_checks, cpr_global_skipped, cpr_local_ok,
        cpr_local_aircraft_relative, cpr_local_receiver_relative, cpr_local_skipped,
        cpr_local_range_checks, cpr_local_speed_checks, cpr_filtered, suppressed_altitude_messages,
        unique_aircraft, single_message_aircraft, pos_all, pos_duplicate, pos_garbage);
    for i in 0..14 {
        o.pos_by_type[i] = a.pos_by_type[i] - b.pos_by_type[i];
    }
    // the bucket's own furthest position (the cumulative maximum would repeat the all-time record
    // in every period)
    o.distance_max = a.distance_max_bucket;
    o
}

fn add_block(into: &mut StatsBlock, b: &StatsBlock) {
    into.samples_processed += b.samples_processed;
    into.samples_lost += b.samples_lost;
    into.samples_dropped += b.samples_dropped;
    let (d, t) = (&mut into.demod, &mut into.track);
    sum_fields!(d, d.clone(), b.demod, +; demod_preambles, demod_rejected_bad,
        demod_rejected_unknown_icao, demod_modeac, signal_power_sum, signal_power_count,
        noise_power_sum, noise_power_count, strong_signal_count);
    for i in 0..3 {
        d.demod_accepted[i] += b.demod.demod_accepted[i];
    }
    for i in 0..5 {
        d.demod_preamble_phase[i] += b.demod.demod_preamble_phase[i];
        d.demod_best_phase[i] += b.demod.demod_best_phase[i];
    }
    d.peak_signal_power = d.peak_signal_power.max(b.demod.peak_signal_power);
    sum_fields!(t, t.clone(), b.track, +; messages_total, cpr_surface, cpr_airborne, cpr_global_ok,
        cpr_global_bad, cpr_global_range_checks, cpr_global_speed_checks, cpr_global_skipped,
        cpr_local_ok, cpr_local_aircraft_relative, cpr_local_receiver_relative, cpr_local_skipped,
        cpr_local_range_checks, cpr_local_speed_checks, cpr_filtered, suppressed_altitude_messages,
        unique_aircraft, single_message_aircraft, pos_all, pos_duplicate, pos_garbage);
    for i in 0..14 {
        t.pos_by_type[i] += b.track.pos_by_type[i];
    }
    t.distance_max = t.distance_max.max(b.track.distance_max);
    into.end = into.end.max(b.end);
    if into.start == 0 || (b.start != 0 && b.start < into.start) {
        into.start = b.start;
    }
}

/// The cumulative counters as they were when the last bucket closed.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    at: i64,
    demod: DemodStats,
    track: TrackStats,
    samples_processed: u64,
    samples_lost: u64,
    samples_dropped: u64,
}

/// Keeps 10 second statistics buckets and builds the rolling windows `stats.json` reports.
#[derive(Debug, Default)]
pub struct StatsRecorder {
    buckets: VecDeque<StatsBlock>,
    last: Option<Snapshot>,
    started: i64,
    next_update: i64,
}

/// Bucket length and the number of buckets kept (15 minutes).
const BUCKET_MS: i64 = 10_000;
const MAX_BUCKETS: usize = 90;

impl StatsRecorder {
    pub fn new(now: i64) -> Self {
        Self {
            started: now,
            next_update: now + BUCKET_MS,
            ..Self::default()
        }
    }

    /// Call regularly with the cumulative counters; closes a bucket every 10 s. Returns whether
    /// it closed one (so `stats.json` is due), which the bucket count can't tell once it is full.
    /// A deadline more than one bucket ahead means the clock was set back: the bucket closes now
    /// instead of `stats.json` standing still until the clock catches up.
    pub fn update(
        &mut self,
        now: i64,
        demod: &mut DemodStats,
        track: &mut TrackStats,
        samples_processed: u64,
        samples_lost: u64,
        samples_dropped: u64,
    ) -> bool {
        if now < self.next_update && self.next_update - now <= BUCKET_MS {
            return false;
        }
        let prev = self.last.take().unwrap_or(Snapshot {
            at: self.started,
            ..Snapshot::default()
        });
        let bucket = StatsBlock {
            start: prev.at,
            end: now,
            samples_processed: samples_processed - prev.samples_processed,
            samples_lost: samples_lost - prev.samples_lost,
            samples_dropped: samples_dropped - prev.samples_dropped,
            demod: diff_demod(demod, &prev.demod),
            track: diff_track(track, &prev.track),
        };
        // the peak and the furthest position are per bucket: restart them
        let mut kept = demod.clone();
        kept.peak_signal_power = 0.0;
        demod.peak_signal_power = 0.0;
        track.distance_max_bucket = 0;
        self.last = Some(Snapshot {
            at: now,
            demod: kept,
            track: track.clone(),
            samples_processed,
            samples_lost,
            samples_dropped,
        });
        self.buckets.push_back(bucket);
        while self.buckets.len() > MAX_BUCKETS {
            self.buckets.pop_front();
        }
        self.next_update = now + BUCKET_MS;
        true
    }

    /// The bucket closed most recently.
    pub fn last(&self) -> Option<&StatsBlock> {
        self.buckets.back()
    }

    /// Number of closed 10 s buckets.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    fn window(&self, buckets: usize) -> StatsBlock {
        let mut out = StatsBlock::default();
        for b in self.buckets.iter().rev().take(buckets) {
            add_block(&mut out, b);
        }
        out
    }

    /// Cumulative since start, including the still open bucket.
    fn total(
        &self,
        now: i64,
        demod: &DemodStats,
        track: &TrackStats,
        samples_processed: u64,
        samples_lost: u64,
        samples_dropped: u64,
    ) -> StatsBlock {
        StatsBlock {
            start: self.started,
            end: now,
            samples_processed,
            samples_lost,
            samples_dropped,
            demod: demod.clone(),
            track: track.clone(),
        }
    }
}

fn append_block(
    s: &mut String,
    key: &str,
    b: &StatsBlock,
    nfix: u8,
    with_local: bool,
    peak_total: f64,
) {
    let _ = write!(
        s,
        ",\n\"{key}\":{{\"start\":{:.1},\"end\":{:.1}",
        b.start as f64 / 1000.0,
        b.end as f64 / 1000.0
    );
    if with_local {
        let d = &b.demod;
        let _ = write!(
            s,
            ",\"local\":{{\"samples_processed\":{},\"samples_dropped\":{},\"samples_lost\":{},\"modeac\":{},\"modes\":{},\"bad\":{},\"unknown_icao\":{}",
            b.samples_processed,
            b.samples_dropped,
            b.samples_lost,
            d.demod_modeac,
            d.demod_preambles,
            d.demod_rejected_bad,
            d.demod_rejected_unknown_icao
        );
        for i in 0..=(nfix as usize) {
            let _ = write!(
                s,
                "{}{}",
                if i == 0 { ",\"accepted\":[" } else { "," },
                d.demod_accepted[i]
            );
        }
        s.push(']');
        if d.signal_power_sum > 0.0 && d.signal_power_count > 0 {
            let _ = write!(
                s,
                ",\"signal\":{:.1}",
                10.0 * (d.signal_power_sum / d.signal_power_count as f64).log10()
            );
        }
        if d.noise_power_sum > 0.0 && d.noise_power_count > 0 {
            let _ = write!(
                s,
                ",\"noise\":{:.1}",
                10.0 * (d.noise_power_sum / d.noise_power_count as f64).log10()
            );
        }
        let peak = d.peak_signal_power.max(peak_total);
        if peak > 0.0 {
            let _ = write!(s, ",\"peak_signal\":{:.1}", 10.0 * peak.log10());
        }
        let _ = write!(s, ",\"strong_signals\":{}", d.strong_signal_count);
        let phases = |v: &[u64; 5]| {
            v.iter()
                .map(|n| format!("{n:9}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        let _ = write!(
            s,
            ",\n\"pre_phase_1\":[{}],\"best_phase\" :[{}]}}",
            phases(&d.demod_preamble_phase),
            phases(&d.demod_best_phase)
        );
    }
    let t = &b.track;
    let _ = write!(s, ",\"messages_valid\": {}", t.messages_total);
    let _ = write!(s, ",\"position_count_total\": {}", t.pos_all);
    s.push_str(",\"position_count_by_type\": {");
    let parts: Vec<String> = ALL_TYPES
        .iter()
        .map(|ty| format!("\"{}\": {}", addrtype_str(*ty), t.pos_by_type[*ty as usize]))
        .collect();
    s.push_str(&parts.join(","));
    s.push('}');
    let _ = write!(
        s,
        ",\"cpr\":{{\"surface\":{},\"airborne\":{},\"global_ok\":{},\"global_bad\":{},\"global_range\":{},\"global_speed\":{},\"global_skipped\":{},\"local_ok\":{},\"local_aircraft_relative\":{},\"local_receiver_relative\":{},\"local_skipped\":{},\"local_range\":{},\"local_speed\":{},\"filtered\":{}}},\"altitude_suppressed\":{},\"cpu\":{{\"demod\":0,\"reader\":0,\"background\":0,\"aircraft_json\":0,\"globe_json\":0,\"binCraft\":0,\"trace_json\":0,\"heatmap_and_state\":0,\"api_workers\":0,\"api_update\":0,\"remove_stale\":0}},\"tracks\":{{\"all\":{},\"single_message\":{}}},\"messages\":{},\"max_distance\":{}}}",
        t.cpr_surface,
        t.cpr_airborne,
        t.cpr_global_ok,
        t.cpr_global_bad,
        t.cpr_global_range_checks,
        t.cpr_global_speed_checks,
        t.cpr_global_skipped,
        t.cpr_local_ok,
        t.cpr_local_aircraft_relative,
        t.cpr_local_receiver_relative,
        t.cpr_local_skipped,
        t.cpr_local_range_checks,
        t.cpr_local_speed_checks,
        t.cpr_filtered,
        t.suppressed_altitude_messages,
        t.unique_aircraft,
        t.single_message_aircraft,
        t.messages_total,
        t.distance_max
    );
}

/// Cumulative counters since start, the input to [`stats_json`].
#[derive(Debug, Clone, Copy)]
pub struct Cumulative<'a> {
    pub demod: &'a DemodStats,
    pub track: &'a TrackStats,
    pub samples_processed: u64,
    pub samples_lost: u64,
    pub samples_dropped: u64,
    pub counts: AircraftCounts,
    /// Tuner gain in dB, if known.
    pub gain_db: Option<f64>,
}

/// The `stats.json` document. `with_local` includes the SDR receiver block, `nfix` is
/// the number of correctable bit errors (sizes the `accepted` array).
pub fn stats_json(
    rec: &StatsRecorder,
    now: i64,
    cum: &Cumulative<'_>,
    nfix: u8,
    with_local: bool,
) -> String {
    let mut s = format!("{{ \"now\" : {:.1}", now as f64 / 1000.0);
    if with_local {
        let _ = write!(
            s,
            ", \"gain_db\" : {:.1}, \"estimated_ppm\" : 0.0",
            cum.gain_db.unwrap_or(99999.9)
        );
    }
    let c = &cum.counts;
    let _ = write!(
        s,
        ",\n\"aircraft_with_pos\": {}, \"aircraft_without_pos\": {}, \"aircraft_count_by_type\": {{",
        c.with_position, c.without_position
    );
    let types: Vec<String> = ALL_TYPES
        .iter()
        .map(|ty| format!(" \"{}\": {}", addrtype_str(*ty), c.by_type[*ty as usize]))
        .collect();
    s.push_str(&types.join(","));
    s.push('}');
    let peak_total = rec
        .buckets
        .iter()
        .map(|b| b.demod.peak_signal_power)
        .fold(0.0, f64::max);
    append_block(&mut s, "last1min", &rec.window(6), nfix, with_local, 0.0);
    append_block(&mut s, "last5min", &rec.window(30), nfix, with_local, 0.0);
    append_block(&mut s, "last15min", &rec.window(90), nfix, with_local, 0.0);
    let total = rec.total(
        now,
        cum.demod,
        cum.track,
        cum.samples_processed,
        cum.samples_lost,
        cum.samples_dropped,
    );
    append_block(&mut s, "total", &total, nfix, with_local, peak_total);
    s.push_str("\n}\n");
    s
}

/// Is `a` fresh enough that [`TRACK_EXPIRE`] has not passed? (helper for callers)
pub fn is_recent(a: &Aircraft, now: i64) -> bool {
    now < a.seen + TRACK_EXPIRE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode_s::{Decoder, ModesMessage};
    use crate::track::TrackerConfig;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    const T0: i64 = 1_700_000_000_000;

    fn feed(d: &mut Decoder, t: &mut Tracker, frame: &str, at: i64) {
        let mut mm = ModesMessage::from_bytes(&hex(frame));
        mm.sys_timestamp = at;
        mm.signal_level = 0.01;
        d.decode(&mut mm);
        t.update_from_message(&mut mm);
    }

    #[test]
    fn aircraft_json_shape() {
        let mut d = Decoder::new(1);
        let mut t = Tracker::new(TrackerConfig::new(Some(52.0), Some(4.0), None));
        let (e, o) = (
            "8D40621D58C382D690C8AC2863A7",
            "8D40621D58C386435CC412692AD6",
        );
        feed(&mut d, &mut t, e, T0);
        feed(&mut d, &mut t, o, T0 + 500);
        feed(&mut d, &mut t, "8D4840D6202CC371C32CE0576098", T0 + 600);
        feed(&mut d, &mut t, "8D4840D6202CC371C32CE0576098", T0 + 700);
        let j = aircraft_json(&t, T0 + 1500);
        assert!(
            j.starts_with("{ \"now\" : 1700000001.500,\n  \"messages\" : 4,\n  \"aircraft\" : ["),
            "{j}"
        );
        assert!(j.contains("\"hex\":\"40621d\",\"type\":\"adsb_icao\""));
        assert!(j.contains("\"lat\":52.265780,\"lon\":3.938913"), "{j}");
        assert!(j.contains("\"flight\":\"KLM1023 \""));
        assert!(j.contains("\"mlat\":[],\"tisb\":[],\"messages\":2"));
        assert!(j.ends_with("\n  ]\n}\n"));
        // valid JSON-ish: balanced braces
        assert_eq!(j.matches('{').count(), j.matches('}').count());
    }

    #[test]
    fn receiver_json_fields() {
        let j = receiver_json(&ReceiverInfo {
            refresh_ms: 1000,
            location: Some((52.0, 4.0)),
            version: "openradome 0.1.0".into(),
            outline: true,
            reapi: false,
            trace_interval_s: None,
            globe: false,
        });
        assert!(j.contains("\"refresh\": 1000"));
        assert!(j.contains("\"lat\": 52.000000, \"lon\": 4.000000"));
        assert!(
            j.contains(
                "\"readsb\": true, \"binCraft\": true, \"zstd\": true, \"outlineJson\": true, \"version\""
            ),
            "{j}"
        );
        let j = receiver_json(&ReceiverInfo {
            refresh_ms: 500,
            location: None,
            version: "x".into(),
            outline: false,
            reapi: true,
            trace_interval_s: Some(20.0),
            globe: true,
        });
        assert!(
            j.contains("\"haveTraces\": true, \"json_trace_interval\": 20.0, \"globeIndexGrid\": 3, \"globeIndexSpecialTiles\": [ {\"south\":60,\"east\":0,\"north\":90,\"west\":-126},"),
            "{j}"
        );
        assert!(j.contains("\"reapi\": true, \"binCraft\": true"), "{j}");
        assert!(!j.contains("\"lat\""));
        assert!(!j.contains("outlineJson"));
    }

    #[test]
    fn stats_windows_roll() {
        let mut rec = StatsRecorder::new(T0);
        let mut demod = DemodStats::default();
        let mut track = TrackStats::default();
        for i in 1..=8i64 {
            demod.demod_preambles += 10;
            track.messages_total += 5;
            rec.update(
                T0 + i * 10_000,
                &mut demod,
                &mut track,
                i as u64 * 1000,
                0,
                i as u64 * 10,
            );
        }
        let last1 = rec.window(6);
        assert_eq!(last1.demod.demod_preambles, 60);
        assert_eq!(last1.track.messages_total, 30);
        assert_eq!(
            rec.window(30).track.messages_total,
            40,
            "only 8 buckets exist"
        );
        let cum = Cumulative {
            demod: &demod,
            track: &track,
            samples_processed: 8000,
            samples_lost: 0,
            samples_dropped: 80,
            counts: AircraftCounts::default(),
            gain_db: None,
        };
        let j = stats_json(&rec, T0 + 80_000, &cum, 1, true);
        assert!(j.contains("\"last1min\":{\"start\":"));
        assert!(
            j.contains("\"gain_db\" : 99999.9, \"estimated_ppm\" : 0.0,\n\"aircraft_with_pos\": 0"),
            "{j}"
        );
        assert!(!j.contains("\"latest\""));
        assert!(j.contains("\"modes\":80,"), "{j}");
        assert!(j.contains("\"messages_valid\": 30"));
        assert!(j.contains("\"samples_dropped\":60,"), "last minute: {j}");
        assert!(j.contains("\"samples_dropped\":80,"), "total: {j}");
        assert_eq!(j.matches('{').count(), j.matches('}').count());

        // update() reports every closed bucket, also once the 15 minute window is full
        let mut rec = StatsRecorder::new(T0);
        let (mut demod, mut track) = (DemodStats::default(), TrackStats::default());
        for i in 1..=200 {
            assert!(
                rec.update(T0 + i * 10_000, &mut demod, &mut track, 0, 0, 0),
                "bucket {i} closed"
            );
            assert!(!rec.update(T0 + i * 10_000 + 5_000, &mut demod, &mut track, 0, 0, 0));
        }
        assert_eq!(rec.len(), 90, "15 minutes of buckets are kept");

        // without an SDR (net-only) there is no receiver block and no gain, like upstream
        let j = stats_json(&rec, T0 + 80_000, &cum, 1, false);
        assert!(!j.contains("gain_db") && !j.contains("\"local\""), "{j}");
    }

    #[test]
    fn stats_keep_updating_when_the_clock_goes_back() {
        let mut rec = StatsRecorder::new(T0);
        let (mut demod, mut track) = (DemodStats::default(), TrackStats::default());
        assert!(rec.update(T0 + 10_000, &mut demod, &mut track, 0, 0, 0));
        // the clock is set back an hour: the next bucket must not wait an hour
        let back = T0 + 12_000 - 3_600_000;
        track.messages_total += 7;
        assert!(
            rec.update(back, &mut demod, &mut track, 0, 0, 0),
            "closes at once"
        );
        assert_eq!(rec.window(1).track.messages_total, 7, "nothing is lost");
        // then the usual 10 s rhythm from the new time
        assert!(!rec.update(back + 9_000, &mut demod, &mut track, 0, 0, 0));
        assert!(rec.update(back + 10_000, &mut demod, &mut track, 0, 0, 0));
        assert_eq!(rec.len(), 3);
    }

    #[test]
    fn max_distance_is_per_period() {
        let mut rec = StatsRecorder::new(T0);
        let (mut demod, mut track) = (DemodStats::default(), TrackStats::default());
        let seen = |track: &mut TrackStats, m: u64| {
            track.distance_max = track.distance_max.max(m);
            track.distance_max_bucket = track.distance_max_bucket.max(m);
        };
        // a 150 km position in the first bucket, then 90 buckets (15 min) of 100 km at most
        seen(&mut track, 150_000);
        assert!(rec.update(T0 + 10_000, &mut demod, &mut track, 0, 0, 0));
        assert_eq!(track.distance_max_bucket, 0, "restarted with the bucket");
        assert_eq!(track.distance_max, 150_000, "the all-time record stays");
        for i in 2..=91i64 {
            seen(&mut track, if i == 60 { 100_000 } else { 50_000 });
            assert!(rec.update(T0 + i * 10_000, &mut demod, &mut track, 0, 0, 0));
        }
        assert_eq!(rec.window(6).track.distance_max, 50_000, "last minute");
        assert_eq!(
            rec.window(90).track.distance_max,
            100_000,
            "last 15 minutes"
        );
        let cum = Cumulative {
            demod: &demod,
            track: &track,
            samples_processed: 0,
            samples_lost: 0,
            samples_dropped: 0,
            counts: AircraftCounts::default(),
            gain_db: None,
        };
        let j = stats_json(&rec, T0 + 910_000, &cum, 1, true);
        let maxes: Vec<&str> = j
            .split("\"max_distance\":")
            .skip(1)
            .map(|p| p.split('}').next().unwrap())
            .collect();
        assert_eq!(maxes, ["50000", "50000", "100000", "150000"], "{j}");
    }

    #[test]
    fn escape_works() {
        assert_eq!(escape("A\"B\\C\u{1}"), "A\\\"B\\\\C\\u0001");
    }
}
