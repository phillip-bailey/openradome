//! Aircraft traces: the track each aircraft flew, for tar1090's history view.
//!
//! A port of the recording rules in upstream's `traceAddInternal` (globe_index.c): which positions are
//! kept (changes in altitude, speed, vertical rate, track and ground state, plus a maximum interval),
//! the one-position buffer that lets a saved point be the one *before* a change, the conversion to
//! upstream's fixed-point `struct state`, and the JSON of `trace_recent_*.json` / `trace_full_*.json`.
//!
//! Not ported: leg markers (`mark_legs`), the extra high-resolution points before landing (`traceLast`),
//! and upstream's compressed in-memory chunks (points are kept uncompressed, capped per aircraft).

use std::fmt::Write as _;

use crate::geo::greatcircle;
use crate::mode_s::{AirGround, DataSource};
use crate::track::{Aircraft, SECONDS, TrackerConfig, Validity};

const ALT_FACTOR: f32 = 1.0 / 6.25;
const RATE_FACTOR: f32 = 1.0 / 8.0;
const GS_FACTOR: f32 = 10.0;
const TRACK_FACTOR: f32 = 100.0;
const ROLL_FACTOR: f32 = 10.0;

/// Upstream's `TRACE_RECENT_POINTS`.
pub const RECENT_POINTS: usize = 92;
/// Detail objects are stored for every fourth point (`SFOUR`).
const DETAIL_EVERY: usize = 4;
/// Points kept per aircraft at most (upstream sizes this from the trace interval; this keeps memory bounded).
const MAX_POINTS: usize = 40_000;

/// One trace point (upstream's `struct state`).
#[derive(Debug, Clone)]
pub struct Point {
    pub timestamp: i64,
    pub lat: i32,
    pub lon: i32,
    pub on_ground: bool,
    pub stale: bool,
    pub leg_marker: bool,
    pub gs: Option<u16>,
    pub track: Option<u16>,
    pub baro_alt: Option<i16>,
    pub baro_rate: Option<i16>,
    pub geom_alt: Option<i16>,
    pub geom_rate: Option<i16>,
    pub ias: Option<u16>,
    pub roll: Option<i16>,
    pub addrtype: &'static str,
    /// The aircraft object shown with every fourth point (upstream's `state_all`), already rendered.
    pub details: Option<Box<str>>,
}

/// An aircraft's recorded trace.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    pub points: Vec<Point>,
    /// The latest position that was not kept: the next saved point may be this one instead.
    buffered: Option<Point>,
    /// Points added since the last full trace write (`trace_writeCounter`).
    pub write_counter: u32,
    /// A new point arrived since the last recent-trace write.
    pub recent_pending: bool,
    /// When the next full trace is due (ms; 0 = at the next pass).
    pub next_full: i64,
    /// Timestamp of the last point written to the permanent history.
    pub perm_last: i64,
    pub next_perm: i64,
}

fn ni(x: f64) -> f64 {
    x.round_ties_even()
}

fn track_v_state(now: i64, v: &Validity, pos: &Validity) -> bool {
    if pos.source <= DataSource::Jaero {
        return v.valid();
    }
    v.valid() && now < v.updated + 35 * SECONDS
}

fn alt_baro_reliable_trace(cfg: &TrackerConfig, now: i64, a: &Aircraft) -> bool {
    a.alt_baro_reliable(cfg) && track_v_state(now, &a.baro_alt_valid, &a.pos_reliable_valid)
}

fn norm_diff(angle: f32, bound: f32) -> f32 {
    let mut a = angle;
    if a < -bound {
        a += 2.0 * bound;
    }
    if a >= bound {
        a -= 2.0 * bound;
    }
    a
}

/// `to_state`: the fixed-point point for aircraft `a` now.
fn to_state(
    cfg: &TrackerConfig,
    a: &Aircraft,
    now: i64,
    on_ground: bool,
    track: f32,
    stale: bool,
) -> Point {
    let pv = &a.pos_reliable_valid;
    let tv = |v: &Validity| track_v_state(now, v, pv);
    Point {
        timestamp: now,
        lat: ni(a.lat_reliable * 1e6) as i32,
        lon: ni(a.lon_reliable * 1e6) as i32,
        on_ground,
        stale,
        leg_marker: false,
        gs: tv(&a.gs_valid).then(|| ni((a.gs * GS_FACTOR) as f64) as u16),
        track: (track > -1.0).then(|| ni((track * TRACK_FACTOR) as f64) as u16),
        baro_alt: alt_baro_reliable_trace(cfg, now, a)
            .then(|| ni((a.baro_alt as f32 * ALT_FACTOR) as f64) as i16),
        baro_rate: tv(&a.baro_rate_valid)
            .then(|| ni((a.baro_rate as f32 * RATE_FACTOR) as f64) as i16),
        geom_alt: tv(&a.geom_alt_valid).then(|| ni((a.geom_alt as f32 * ALT_FACTOR) as f64) as i16),
        geom_rate: tv(&a.geom_rate_valid)
            .then(|| ni((a.geom_rate as f32 * RATE_FACTOR) as f64) as i16),
        ias: tv(&a.ias_valid).then_some(a.ias as u16),
        roll: tv(&a.roll_valid).then(|| ni((a.roll * ROLL_FACTOR) as f64) as i16),
        addrtype: crate::json::addrtype_str(a.addrtype),
        details: None,
    }
}

impl Trace {
    /// `traceUsePosBuffered`: keep the buffered position as a point.
    fn use_buffered(&mut self) -> bool {
        match self.buffered.take() {
            Some(p) => {
                self.points.push(p);
                self.write_counter += 1;
                self.recent_pending = true;
                true
            }
            None => false,
        }
    }

    /// Drop points older than `keep_ms`, in multiples of four so detail alignment holds.
    pub fn prune(&mut self, now: i64, keep_ms: i64) {
        let old = self.points.partition_point(|p| p.timestamp < now - keep_ms);
        let mut drop_n = old / DETAIL_EVERY * DETAIL_EVERY;
        if self.points.len() - drop_n > MAX_POINTS {
            drop_n = (self.points.len() - MAX_POINTS).div_ceil(DETAIL_EVERY) * DETAIL_EVERY;
        }
        if drop_n > 0 {
            self.points.drain(..drop_n);
        }
    }
}

/// What the recorder needs from the tracker's settings.
#[derive(Debug, Clone, Copy)]
pub struct TraceSettings {
    /// `--json-trace-interval` in ms (default 20 s).
    pub interval_ms: i64,
    /// Seconds of high-resolution points after a ground transition (default 60 s).
    pub after_ground_ms: i64,
}

impl Default for TraceSettings {
    fn default() -> Self {
        Self {
            interval_ms: 20 * SECONDS,
            after_ground_ms: 60 * SECONDS,
        }
    }
}

/// `traceAdd`: called when a reliable position was accepted. `render` produces the detail object for
/// points that get one.
pub fn trace_add(
    cfg: &TrackerConfig,
    s: &TraceSettings,
    a: &mut Aircraft,
    now: i64,
    stale: bool,
    render: &dyn Fn(&Aircraft, i64) -> String,
) {
    add_internal(cfg, s, a, now, stale, render);
}

enum Decision {
    /// Keep this position (after trying the buffered one first).
    Save,
    /// Keep this position without looking at the buffer.
    SaveNoBuf,
    /// Do not keep it, buffer it.
    NoSave,
}

fn add_internal(
    cfg: &TrackerConfig,
    s: &TraceSettings,
    a: &mut Aircraft,
    now: i64,
    stale: bool,
    render: &dyn Fn(&Aircraft, i64) -> String,
) {
    let mut max_elapsed = s.interval_ms;
    let mut min_elapsed = 250.min(max_elapsed / 4);
    let turn_density = 5.0f32;
    let mut max_speed_diff = 5.0f32;

    let alt = a.baro_alt;
    let alt_valid = alt_baro_reliable_trace(cfg, now, a);
    if alt_valid && a.baro_alt > 10_000 {
        max_speed_diff *= 2.0;
    }
    let mlat_pos = a.pos_reliable_valid.source == DataSource::Mlat;
    if max_elapsed > 5 * SECONDS && mlat_pos {
        min_elapsed = 1500;
        max_elapsed = (max_elapsed / 2).max(5 * SECONDS);
    }
    if a.squawk_valid.valid() && a.squawk == 0x7777 {
        min_elapsed = max_elapsed;
    }

    let pv = a.pos_reliable_valid;
    let mut on_ground = false;
    let mut track: f32 =
        if track_v_state(now, &a.track_valid, &pv) && a.track_valid.source != DataSource::Mlat {
            a.track
        } else {
            -1.0
        };
    let mut ag_valid = false;
    if a.airground_valid.valid() {
        ag_valid = true;
        if a.airground == AirGround::Ground {
            on_ground = true;
            track = if track_v_state(now, &a.true_heading_valid, &pv) {
                a.true_heading
            } else {
                -1.0
            };
        }
    }
    if max_elapsed > 5 * SECONDS && !mlat_pos && track == -1.0 {
        max_elapsed = (max_elapsed / 4).max(5 * SECONDS);
    }

    let decision;
    let mut elapsed: i64 = 0;
    let mut duplicate = false;

    if a.trace.points.is_empty() {
        decision = Decision::Save;
    } else {
        let last = a.trace.points.last().unwrap().clone();
        if now >= last.timestamp {
            elapsed = now - last.timestamp;
        }
        let buffered_elapsed = a
            .trace
            .buffered
            .as_ref()
            .map_or(0, |b| b.timestamp - last.timestamp);
        let mut buffered = a.trace.buffered.is_some();
        if buffered_elapsed < 0 {
            a.trace.buffered = None;
            buffered = false;
        }

        let new_lat = ni(a.lat * 1e6) as i32;
        let new_lon = ni(a.lon * 1e6) as i32;
        duplicate = elapsed < SECONDS && new_lat == last.lat && new_lon == last.lon;

        let last_alt = last.baro_alt.map(|v| (v as f32 / ALT_FACTOR) as i32);
        let alt_diff = match (last_alt, alt_valid) {
            (Some(l), true) => (a.baro_alt - l).abs(),
            _ => 0,
        };
        let mut speed_diff = 0.0f32;
        if let Some(last_gs) = last.gs
            && a.gs_valid.valid()
            && a.gs_valid.source != DataSource::Mlat
        {
            speed_diff = ((last_gs as f32 / GS_FACTOR) - a.gs).abs();
        }
        let mut baro_rate_diff = 0.0f32;
        if let Some(last_rate) = last.baro_rate
            && a.baro_rate_valid.valid()
            && a.baro_rate_valid.source != DataSource::Mlat
        {
            baro_rate_diff = ((last_rate as f32 / RATE_FACTOR) - a.baro_rate as f32).abs();
        }
        if !ag_valid && !alt_valid {
            on_ground = last.on_ground;
        }
        if on_ground {
            track = if track_v_state(now, &a.true_heading_valid, &pv) {
                a.true_heading
            } else {
                -1.0
            };
        }
        let last_track = last.track.unwrap_or(0) as f32 / TRACK_FACTOR;
        let mut track_diff = 0.0f32;
        if last.track.is_some() && track > -1.0 {
            track_diff = norm_diff(track - last_track, 180.0).abs();
        }
        let distance = greatcircle(
            last.lat as f64 / 1e6,
            last.lon as f64 / 1e6,
            a.lat,
            a.lon,
            false,
        );

        let mut save_no_buf = speed_diff >= max_speed_diff || baro_rate_diff >= 200.0;

        decision = 'decide: {
            if on_ground != last.on_ground {
                break 'decide Decision::Save;
            }
            if now - a.last_air_ground_change < s.after_ground_ms && elapsed > 750 {
                save_no_buf = true;
            }
            if elapsed > 10 * max_elapsed {
                break 'decide Decision::Save;
            }
            if alt_valid && last_alt.is_none() {
                break 'decide Decision::Save;
            }
            if alt_diff > 0 {
                let max_diff = if alt <= 7000 {
                    75
                } else if alt <= 12000 {
                    200
                } else {
                    400
                };
                if alt_diff >= max_diff {
                    if alt_diff < 250 && alt_diff * 3 <= max_diff * 4 {
                        save_no_buf = true;
                    } else {
                        break 'decide Decision::Save;
                    }
                }
                let base = if alt <= 7000 {
                    125
                } else if alt <= 12000 {
                    250
                } else {
                    800
                };
                let too_long = ((max_elapsed / 4) as f32 * base as f32 / alt_diff as f32) as i64;
                if alt_diff >= 25 && elapsed > too_long {
                    // upstream compares the altitude in feet with the buffered point's fixed-point value
                    let buffered_alt = a.trace.buffered.as_ref().and_then(|b| b.baro_alt);
                    if buffered && buffered_alt.is_some_and(|b| b as i32 == alt) {
                        break 'decide Decision::Save;
                    }
                    save_no_buf = true;
                }
            }
            if elapsed < min_elapsed {
                break 'decide Decision::NoSave;
            }
            if a.squawk == 0x7700 {
                break 'decide Decision::Save;
            }
            if !on_ground && elapsed > max_elapsed {
                break 'decide Decision::Save;
            }
            if a.addr == 0xa19b53 && elapsed > max_elapsed / 4 {
                break 'decide Decision::Save;
            }
            if stale {
                break 'decide Decision::Save;
            }
            if on_ground {
                if elapsed > 4 * max_elapsed {
                    break 'decide Decision::Save;
                }
                if distance > 10.0 && elapsed > max_elapsed {
                    break 'decide Decision::Save;
                }
                if a.gs > 5.0 && elapsed > max_elapsed / 2 {
                    break 'decide Decision::Save;
                }
                if distance * track_diff as f64 > 130.0 {
                    break 'decide Decision::Save;
                }
                if distance > 40.0 {
                    break 'decide Decision::Save;
                }
            }
            if track_diff > 0.5 && (elapsed as f32 / 1000.0 * track_diff * turn_density > 100.0) {
                break 'decide Decision::Save;
            }
            if save_no_buf {
                Decision::SaveNoBuf
            } else {
                Decision::NoSave
            }
        };
        // a ground-state change keeps the previous position too
        if let Decision::Save = decision
            && on_ground != last.on_ground
        {
            a.trace.use_buffered();
        }
    }

    if let Decision::Save = decision {
        // always prefer the buffered position over the current one: it shows the change better
        if a.trace.use_buffered() {
            add_internal(cfg, s, a, now, stale, render);
            return;
        }
    }
    let pos_used = !matches!(decision, Decision::NoSave);

    if duplicate {
        return;
    }
    if a.trace.points.len() + 1 >= MAX_POINTS + 64 {
        return;
    }

    let mut point = to_state(cfg, a, now, on_ground, track, stale);
    // every fourth point (by its place in the trace) carries the full aircraft object
    if a.trace.points.len().is_multiple_of(DETAIL_EVERY) {
        point.details = Some(render(a, now).into_boxed_str());
    }
    if pos_used {
        a.trace.buffered = None;
        a.trace.points.push(point);
        a.trace.write_counter += 1;
        a.trace.recent_pending = true;
    } else {
        a.trace.buffered = Some(point);
    }
}

/// The `_alt_factor` conversion: stored altitude back to feet.
fn feet(v: i16) -> i32 {
    (v as f32 / ALT_FACTOR) as i32
}

fn rate(v: i16) -> i32 {
    (v as f32 / RATE_FACTOR) as i32
}

/// One trace point as upstream's `sprintTracePoint` prints it.
fn print_point(s: &mut String, p: &Point, reference_ts: i64) {
    let (mut altitude, mut altitude_geom) = (None, false);
    if let Some(a) = p.baro_alt {
        altitude = Some(feet(a));
    } else if let Some(g) = p.geom_alt {
        altitude = Some(feet(g));
        altitude_geom = true;
    }
    let (mut vrate, mut rate_geom) = (None, false);
    if let Some(r) = p.baro_rate {
        vrate = Some(rate(r));
    } else if let Some(g) = p.geom_rate {
        vrate = Some(rate(g));
        rate_geom = true;
    }
    let _ = write!(
        s,
        "\n[{:.2},{:.6},{:.6}",
        (p.timestamp - reference_ts) as f64 / 1000.0,
        p.lat as f64 / 1e6,
        p.lon as f64 / 1e6
    );
    if p.on_ground {
        s.push_str(",\"ground\"");
    } else if let Some(a) = altitude {
        let _ = write!(s, ",{a}");
    } else {
        s.push_str(",null");
    }
    match p.gs {
        Some(g) => {
            let _ = write!(s, ",{:.1}", (g as f32 / GS_FACTOR) as f64);
        }
        None => s.push_str(",null"),
    }
    match p.track {
        Some(t) => {
            let _ = write!(s, ",{:.1}", (t as f32 / TRACK_FACTOR) as f64);
        }
        None => s.push_str(",null"),
    }
    let bitfield = ((altitude_geom as u8) << 3)
        | ((rate_geom as u8) << 2)
        | ((p.leg_marker as u8) << 1)
        | p.stale as u8;
    let _ = write!(s, ",{bitfield}");
    match vrate {
        Some(r) => {
            let _ = write!(s, ",{r}");
        }
        None => s.push_str(",null"),
    }
    match &p.details {
        Some(d) => {
            s.push(',');
            s.push_str(d);
        }
        None => s.push_str(",null"),
    }
    let _ = write!(s, ",\"{}\"", p.addrtype);
    match p.geom_alt {
        Some(g) => {
            let _ = write!(s, ",{}", feet(g));
        }
        None => s.push_str(",null"),
    }
    match p.geom_rate {
        Some(g) => {
            let _ = write!(s, ",{}", rate(g));
        }
        None => s.push_str(",null"),
    }
    match p.ias {
        Some(i) => {
            let _ = write!(s, ",{i}");
        }
        None => s.push_str(",null"),
    }
    match p.roll {
        Some(r) => {
            let _ = write!(s, ",{:.1}", (r as f32 / ROLL_FACTOR) as f64);
        }
        None => s.push_str(",null"),
    }
    s.push_str("],");
}

/// Which trace file to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The last [`RECENT_POINTS`] points.
    Recent,
    /// Everything kept in memory (from `start`).
    Full,
}

/// `generateTraceJson` for points `start..=last` (all when `last` is `None`). `reference_ts` of 0 means the
/// first point's timestamp; a permanent day file passes the start of the day.
pub fn trace_json(
    addr: u32,
    points: &[Point],
    kind: Kind,
    start: usize,
    last: Option<usize>,
    reference_ts: i64,
    version: &str,
) -> String {
    let (start, last) = match kind {
        Kind::Recent => (
            points.len().saturating_sub(RECENT_POINTS),
            points.len().saturating_sub(1),
        ),
        Kind::Full => (start, last.unwrap_or(points.len().saturating_sub(1))),
    };
    let mut s = String::with_capacity(300 * (last + 1).saturating_sub(start) + 256);
    let _ = write!(
        s,
        "{{\"icao\":\"{}{:06x}\"",
        if addr & crate::mode_s::NON_ICAO_ADDRESS != 0 {
            "~"
        } else {
            ""
        },
        addr & 0xFF_FFFF
    );
    let mut reference = reference_ts;
    if start < points.len() {
        let first = points[start].timestamp;
        if reference == 0 || first < reference {
            reference = first;
        }
    } else if !points.is_empty() {
        reference = points[0].timestamp;
    }
    let _ = write!(s, ",\n\"version\": \"{version}\"");
    let _ = write!(s, ",\n\"timestamp\": {:.3}", reference as f64 / 1000.0);
    s.push_str(",\n\"trace\":[ ");
    for p in points.iter().take(last + 1).skip(start) {
        print_point(&mut s, p, reference);
    }
    if s.ends_with(',') {
        s.pop();
    }
    s.push_str(" ]\n }\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(ts: i64, lat: f64, lon: f64, alt: i32, details: bool) -> Point {
        Point {
            timestamp: ts,
            lat: (lat * 1e6) as i32,
            lon: (lon * 1e6) as i32,
            on_ground: false,
            stale: false,
            leg_marker: false,
            gs: Some(2455),
            track: Some(4375),
            baro_alt: Some((alt as f32 * ALT_FACTOR).round() as i16),
            baro_rate: Some(104),
            geom_alt: None,
            geom_rate: None,
            ias: Some(279),
            roll: None,
            addrtype: "adsb_icao",
            details: details.then(|| "{\"type\":\"adsb_icao\"}".into()),
        }
    }

    #[test]
    fn a_point_prints_like_upstream() {
        let mut s = String::new();
        print_point(
            &mut s,
            &point(1_700_000_005_000, 52.249627, 0.853952, 30175, true),
            1_700_000_000_000,
        );
        assert_eq!(
            s,
            "\n[5.00,52.249627,0.853952,30175,245.5,43.8,0,832,{\"type\":\"adsb_icao\"},\"adsb_icao\",null,null,279,null],"
        );
    }

    #[test]
    fn ground_and_geometric_altitude_bits() {
        let mut p = point(1000, 1.0, 2.0, 0, false);
        p.on_ground = true;
        p.baro_alt = None;
        p.geom_alt = Some(160);
        p.baro_rate = None;
        p.geom_rate = Some(8);
        p.stale = true;
        let mut s = String::new();
        print_point(&mut s, &p, 0);
        // ground, bitfield = geom altitude (8) | geom rate (4) | stale (1) = 13, rate 64
        assert!(
            s.contains(",\"ground\",245.5,43.8,13,64,null,\"adsb_icao\",1000,64,"),
            "{s}"
        );
    }

    #[test]
    fn the_file_has_upstreams_frame() {
        let pts = vec![
            point(1000, 1.0, 2.0, 100, false),
            point(21_000, 1.1, 2.1, 100, true),
        ];
        let j = trace_json(0x4ca536, &pts, Kind::Full, 0, None, 0, "openradome test");
        assert!(j.starts_with("{\"icao\":\"4ca536\",\n\"version\": \"openradome test\",\n\"timestamp\": 1.000,\n\"trace\":[ \n[0.00,1.000000,2.000000,"), "{j}");
        assert!(
            j.ends_with("{\"type\":\"adsb_icao\"},\"adsb_icao\",null,null,279,null] ]\n }\n"),
            "{j}"
        );
        let n = trace_json(
            0x123456 | crate::mode_s::NON_ICAO_ADDRESS,
            &[],
            Kind::Recent,
            0,
            None,
            0,
            "v",
        );
        assert!(n.starts_with("{\"icao\":\"~123456\""));
        assert!(n.contains("\"trace\":[  ]"), "{n}");
    }

    #[test]
    fn recent_takes_the_last_points_only() {
        let pts: Vec<Point> = (0..200)
            .map(|i| point(i * 1000, 1.0, 2.0, 100, false))
            .collect();
        let j = trace_json(1, &pts, Kind::Recent, 0, None, 0, "v");
        assert_eq!(j.matches("\n[").count(), RECENT_POINTS);
        assert!(j.contains(&format!(
            "\"timestamp\": {:.3}",
            (200 - RECENT_POINTS) as f64
        )));
    }

    #[test]
    fn prune_keeps_alignment_and_a_cap() {
        let mut t = Trace {
            points: (0..10)
                .map(|i| point(i * 1000, 1.0, 2.0, 0, false))
                .collect(),
            ..Trace::default()
        };
        t.prune(8_500, 3_000); // points older than 5_500 ms: 0..=5 -> 6, rounded down to 4
        assert_eq!(t.points.len(), 6);
        assert_eq!(t.points[0].timestamp, 4000);
    }

    #[test]
    fn norm_diff_wraps() {
        assert_eq!(norm_diff(190.0, 180.0), -170.0);
        assert_eq!(norm_diff(-190.0, 180.0), 170.0);
        assert_eq!(norm_diff(10.0, 180.0), 10.0);
    }
}
