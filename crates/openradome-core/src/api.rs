//! The query side of upstream's HTTP API (`--net-api-port`): parse a request's query string, search a
//! snapshot of the aircraft list and build the reply body (JSON or binCraft).
//!
//! A port of `parseFetch` and `apiReq` from upstream's `api.c`. The HTTP server itself is in the
//! `openradome` crate; this module has no I/O so it can be tested on its own. Like upstream, a request
//! is lower-cased before parsing, and the snapshot is a list sorted by longitude that is replaced as a
//! whole once per `--write-json-every` interval.

use std::fmt::Write as _;

use crate::bincraft::{self, RECORD_SIZE, View, bit};
use crate::geo::{bearing, greatcircle};
use crate::json;
use crate::track::Tracker;

/// Upstream's `API_REQ_LIST_MAX`.
pub const LIST_MAX: usize = 1024;

/// One aircraft in a snapshot: its binCraft record and its `aircraft.json` object.
#[derive(Clone)]
pub struct Entry {
    pub bin: [u8; RECORD_SIZE],
    pub json: String,
}

/// The aircraft list as of one moment, sorted by longitude (aircraft without a position last).
pub struct Snapshot {
    pub now_ms: i64,
    pub entries: Vec<Entry>,
    pub aircraft_with_position: u32,
    pub message_count: u32,
    pub message_rate: f64,
    /// Header flag bit 0 (a decoder with the globe index or API).
    pub flags: u32,
}

impl Snapshot {
    /// `apiUpdate`: take the active aircraft, convert each, sort by longitude.
    pub fn build(t: &Tracker, now: i64, message_rate: f64) -> Self {
        let mut entries = Vec::new();
        for a in t.active() {
            if !Tracker::include_in_json(now, a) {
                continue;
            }
            let mut bin = bincraft::record(t, a, now, t.cfg.message_rate_mode);
            if !a.pos_reliable_valid.valid() {
                // position-less aircraft sort last and are never inside a box
                bin[8..12].copy_from_slice(&i32::MAX.to_le_bytes());
                bin[12..16].copy_from_slice(&i32::MAX.to_le_bytes());
            }
            entries.push(Entry {
                bin,
                json: json::aircraft_object(t, a, now),
            });
        }
        entries.sort_by_key(|e| View(&e.bin).lon());
        Self {
            now_ms: now,
            entries,
            aircraft_with_position: t.counts(now).with_position,
            message_count: t.stats.messages_total as u32,
            message_rate,
            flags: t.cfg.message_rate_mode as u32,
        }
    }
}

/// What a request asks for.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Query {
    /// south, north, west, east
    pub r#box: Option<[f64; 4]>,
    /// lat, lon, radius in metres, only the closest
    pub circle: Option<(f64, f64, f64, bool)>,
    pub hex_list: Option<Vec<u32>>,
    pub callsign_list: Option<Vec<String>>,
    pub reg_list: Option<Vec<String>>,
    pub type_list: Option<Vec<String>>,
    pub squawk_list: Option<Vec<u32>>,
    pub all: bool,
    pub all_with_pos: bool,
    pub filter_with_pos: bool,
    pub filter_squawk: Option<Vec<u32>>,
    pub filter_type: Option<Vec<String>>,
    pub filter_callsign_exact: Option<String>,
    pub filter_callsign_prefix: Option<String>,
    /// above, below (feet), when either is given
    pub filter_alt_baro: Option<(i32, i32)>,
    /// mil, interesting, pia, ladd
    pub db_flags: Option<[bool; 4]>,
    pub jamesv2: bool,
    pub bincraft: bool,
    pub zstd: bool,
    pub include_version: bool,
}

fn parse_doubles(v: &str, max: usize) -> Option<Vec<f64>> {
    let mut out = Vec::new();
    for tok in v.split(',') {
        let x: f64 = tok.parse().ok()?;
        if !x.is_finite() || out.len() >= max {
            return None;
        }
        out.push(x);
    }
    Some(out)
}

fn split_list(v: &str) -> Vec<&str> {
    v.split(',')
        .filter(|t| !t.is_empty())
        .take(LIST_MAX)
        .collect()
}

/// C's `strtol(s, &end, 16)`: the leading hex digits, and whether there were any.
fn strtol16(s: &str) -> Option<u32> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let n = s.chars().take_while(char::is_ascii_hexdigit).count();
    (n > 0).then(|| u32::from_str_radix(&s[..n.min(8)], 16).unwrap_or(0))
}

/// Parse the query string (everything after `?`, already lower-cased). `None` means a 400.
pub fn parse_query(query: &str) -> Option<Query> {
    let mut q = Query::default();
    let mut alt = (i32::MIN, i32::MAX);
    let mut alt_given = false;
    let mut flags = [false; 4];
    let mut flags_given = false;
    for tok in query.split('&').filter(|t| !t.is_empty()) {
        match tok.split_once('=') {
            Some((name, value)) => match name {
                "box" => {
                    let b = parse_doubles(value, 4)?;
                    if b.len() < 4 {
                        return None;
                    }
                    for (i, v) in b.iter().enumerate() {
                        let limit = if i < 2 { 90.0 } else { 180.0 };
                        if v.abs() > limit {
                            return None;
                        }
                    }
                    if b[0] > b[1] {
                        return None;
                    }
                    q.r#box = Some([b[0], b[1], b[2], b[3]]);
                }
                "closest" | "circle" => {
                    let c = parse_doubles(value, 3)?;
                    if c.len() < 3 {
                        return None;
                    }
                    let radius = c[2] * 1852.0;
                    if c[0].abs() > 90.0
                        || c[1].abs() > 180.0
                        || !(0.0..=20_037_500.0).contains(&radius)
                    {
                        return None;
                    }
                    q.circle = Some((c[0], c[1], radius, name == "closest"));
                }
                "find_hex" | "hexlist" => {
                    let mut hexes = Vec::new();
                    for t in split_list(value) {
                        let (other, t) = match t.strip_prefix('~') {
                            Some(r) => (true, r),
                            None => (false, t),
                        };
                        if let Some(h) = strtol16(t) {
                            hexes.push(
                                h | if other {
                                    crate::mode_s::NON_ICAO_ADDRESS
                                } else {
                                    0
                                },
                            );
                        }
                    }
                    if hexes.is_empty() {
                        return None;
                    }
                    q.hex_list = Some(hexes);
                }
                "find_callsign" => {
                    let l: Vec<String> = split_list(value).iter().map(|s| s.to_string()).collect();
                    if l.is_empty() {
                        return None;
                    }
                    q.callsign_list = Some(l);
                }
                "find_reg" => {
                    let l: Vec<String> = split_list(value).iter().map(|s| s.to_string()).collect();
                    if l.is_empty() {
                        return None;
                    }
                    q.reg_list = Some(l);
                }
                "find_squawk" | "filter_squawk" => {
                    let l: Vec<u32> = split_list(value)
                        .iter()
                        .map(|s| strtol16(s).unwrap_or(0))
                        .collect();
                    if l.is_empty() {
                        return None;
                    }
                    if name == "find_squawk" {
                        q.squawk_list = Some(l);
                    } else {
                        q.filter_squawk = Some(l);
                    }
                }
                "find_type" | "filter_type" => {
                    let l: Vec<String> = split_list(value).iter().map(|s| s.to_string()).collect();
                    if l.is_empty() {
                        return None;
                    }
                    if name == "find_type" {
                        q.type_list = Some(l);
                    } else {
                        q.filter_type = Some(l);
                    }
                }
                "filter_callsign_exact" => {
                    q.filter_callsign_exact = Some(value.chars().take(8).collect())
                }
                "filter_callsign_prefix" => {
                    q.filter_callsign_prefix = Some(value.chars().take(8).collect())
                }
                "above_alt_baro" => {
                    alt_given = true;
                    alt.0 = value.parse().unwrap_or(0);
                }
                "below_alt_baro" => {
                    alt_given = true;
                    alt.1 = value.parse().unwrap_or(0);
                }
                // not supported here: receiver ids need --net-receiver-id
                "filter_uuid" => return None,
                _ => return None,
            },
            None => match tok {
                "json" => {}
                "jv2" => q.jamesv2 = true,
                "zstd" => q.zstd = true,
                "bincraft" => q.bincraft = true,
                "all" => q.all = true,
                "all_with_pos" => q.all_with_pos = true,
                "filter_with_pos" => q.filter_with_pos = true,
                "filter_mil" => {
                    flags[0] = true;
                    flags_given = true;
                }
                "filter_interesting" => {
                    flags[1] = true;
                    flags_given = true;
                }
                "filter_pia" => {
                    flags[2] = true;
                    flags_given = true;
                }
                "filter_ladd" => {
                    flags[3] = true;
                    flags_given = true;
                }
                "include_version" => q.include_version = true,
                _ => return None,
            },
        }
    }
    if alt_given {
        q.filter_alt_baro = Some(alt);
    }
    if flags_given {
        q.db_flags = Some(flags);
    }
    let main_options = [
        q.r#box.is_some(),
        q.circle.is_some(),
        q.hex_list.is_some(),
        q.callsign_list.is_some(),
        q.reg_list.is_some(),
        q.type_list.is_some(),
        q.squawk_list.is_some(),
        q.all,
        q.all_with_pos,
    ]
    .iter()
    .filter(|&&b| b)
    .count();
    // a box together with a hex list is the one allowed pair
    let ok = main_options == 1 || (main_options == 2 && q.r#box.is_some() && q.hex_list.is_some());
    if !ok {
        return None;
    }
    if q.squawk_list.is_some() && q.filter_squawk.is_some() {
        return None;
    }
    if q.type_list.is_some() && q.filter_type.is_some() {
        return None;
    }
    Some(q)
}

/// A search result: the entry plus the circle's distance (metres) and bearing.
struct Hit<'a> {
    e: &'a Entry,
    dist: f32,
    dir: f32,
}

fn hit(e: &Entry) -> Hit<'_> {
    Hit {
        e,
        dist: 0.0,
        dir: 0.0,
    }
}

fn in_lat_range(e: &Entry, lat1: i32, lat2: i32, bincraft: bool) -> bool {
    let v = View(&e.bin);
    v.lat() >= lat1 && v.lat() <= lat2 && (v.flag(bit::POSITION_VALID) || bincraft)
}

/// Entries with `from <= lon <= to`, in list order.
fn lon_range(list: &[Entry], from: i32, to: i32) -> impl Iterator<Item = &Entry> {
    list.iter().filter(move |e| {
        let lon = View(&e.bin).lon();
        lon >= from && lon <= to
    })
}

fn upper(s: &str) -> String {
    s.to_ascii_uppercase()
}

/// Run a query on a snapshot. The result is the body, uncompressed; `elapsed_ms` is how long the
/// request took so far (for `ptime`).
pub fn reply(snap: &Snapshot, q: &Query, elapsed_ms: f64) -> Vec<u8> {
    let list = &snap.entries;
    let flagged: Vec<Entry>;
    let haystack: &[Entry] = if q.db_flags.is_some() {
        flagged = list
            .iter()
            .filter(|e| View(&e.bin).db_flags() != 0)
            .cloned()
            .collect();
        &flagged
    } else {
        list
    };

    let mut hits: Vec<Hit> = Vec::new();
    if let Some(b) = q.r#box {
        let (lat1, lat2) = ((b[0] * 1e6) as i32, (b[1] * 1e6) as i32);
        let (lon1, lon2) = ((b[2] * 1e6) as i32, (b[3] * 1e6) as i32);
        let ranges: Vec<(i32, i32)> = if lon1 <= lon2 {
            vec![(lon1, lon2)]
        } else {
            vec![(lon1, 180_000_000), (-180_000_000, lon2)]
        };
        for (from, to) in ranges {
            hits.extend(
                lon_range(haystack, from, to)
                    .filter(|e| in_lat_range(e, lat1, lat2, q.bincraft))
                    .map(hit),
            );
        }
        if let Some(hexes) = &q.hex_list {
            hits.extend(find_hex(list, hexes));
        }
    } else if let Some((lat, lon, radius, only_closest)) = q.circle {
        hits = in_circle(haystack, lat, lon, radius, only_closest, q.bincraft);
    } else if let Some(hexes) = &q.hex_list {
        hits = find_hex(list, hexes);
    } else if let Some(regs) = &q.reg_list {
        for r in regs {
            let r = upper(r);
            if let Some(e) = list
                .iter()
                .find(|e| View(&e.bin).registration() == r.trim_end().as_bytes())
            {
                hits.push(hit(e));
            }
        }
    } else if let Some(calls) = &q.callsign_list {
        for c in calls {
            let c = upper(c);
            if let Some(e) = list
                .iter()
                .find(|e| View(&e.bin).callsign() == c.trim_end().as_bytes())
            {
                hits.push(hit(e));
            }
        }
    } else if let Some(types) = &q.type_list {
        let types: Vec<String> = types.iter().map(|t| upper(t)).collect();
        hits = haystack
            .iter()
            .filter(|e| {
                types
                    .iter()
                    .any(|t| View(&e.bin).type_code() == t.as_bytes())
            })
            .map(hit)
            .collect();
    } else if let Some(sq) = &q.squawk_list {
        hits = haystack
            .iter()
            .filter(|e| {
                let v = View(&e.bin);
                v.flag(bit::SQUAWK_VALID) && sq.contains(&(v.squawk() as u32))
            })
            .map(hit)
            .collect();
    } else if q.all {
        hits = haystack.iter().map(hit).collect();
    } else if q.all_with_pos {
        hits = lon_range(haystack, -180_000_000, 180_000_000)
            .map(hit)
            .collect();
    }

    if q.filter_with_pos || q.all_with_pos {
        hits.retain(|h| View(&h.e.bin).flag(bit::POSITION_VALID));
    }
    if let Some(f) = q.db_flags {
        hits.retain(|h| {
            let d = View(&h.e.bin).db_flags();
            (f[0] && d & 1 != 0)
                || (f[1] && d & 2 != 0)
                || (f[2] && d & 4 != 0)
                || (f[3] && d & 8 != 0)
        });
    }
    if let Some((above, below)) = q.filter_alt_baro {
        hits.retain(|h| {
            let v = View(&h.e.bin);
            let alt = if v.flag(bit::BARO_ALT_VALID) {
                ((v.baro_alt() as f32) * 25.0) as i32
            } else if v.airground() == 1 {
                0
            } else {
                i32::MIN
            };
            alt >= above && alt <= below && alt != i32::MIN
        });
    }
    if let Some(p) = &q.filter_callsign_prefix {
        let p = upper(p);
        hits.retain(|h| {
            let v = View(&h.e.bin);
            v.flag(bit::CALLSIGN_VALID) && v.callsign().starts_with(p.as_bytes())
        });
    }
    if let Some(c) = &q.filter_callsign_exact {
        let c = upper(c);
        hits.retain(|h| {
            let v = View(&h.e.bin);
            v.flag(bit::CALLSIGN_VALID) && v.callsign() == c.trim_end().as_bytes()
        });
    }
    if let Some(sq) = &q.filter_squawk {
        hits.retain(|h| {
            let v = View(&h.e.bin);
            v.flag(bit::SQUAWK_VALID) && sq.contains(&(v.squawk() as u32))
        });
    }
    if let Some(types) = &q.filter_type {
        let types: Vec<String> = types.iter().map(|t| upper(t)).collect();
        hits.retain(|h| {
            types
                .iter()
                .any(|t| View(&h.e.bin).type_code() == t.as_bytes())
        });
    }

    if q.bincraft {
        bin_reply(snap, q, &hits)
    } else {
        json_reply(snap, q, &hits, elapsed_ms)
    }
}

fn find_hex<'a>(list: &'a [Entry], hexes: &[u32]) -> Vec<Hit<'a>> {
    hexes
        .iter()
        .filter_map(|h| list.iter().find(|e| View(&e.bin).hex() == *h))
        .map(hit)
        .collect()
}

fn in_circle<'a>(
    list: &'a [Entry],
    lat: f64,
    lon: f64,
    radius: f64,
    only_closest: bool,
    bincraft: bool,
) -> Vec<Hit<'a>> {
    let circum = 40075e3;
    let fudge = 1.002;
    let londiff =
        fudge * radius / ((lat * std::f64::consts::PI / 180.0).cos() * circum + 1.0) * 360.0;
    let (mut o1, mut o2) = (lon - londiff, lon + londiff);
    if o1 < -180.0 {
        o1 += 360.0;
    }
    if o2 > 180.0 {
        o2 -= 360.0;
    }
    if londiff >= 180.0 {
        o1 = -180.0;
        o2 = 180.0;
    }
    let latdiff = fudge * radius / (circum / 2.0) * 180.0;
    let (a1, a2) = (lat - latdiff, lat + latdiff);
    if a1 < -90.0 || a2 > 90.0 {
        o1 = -180.0;
        o2 = 180.0;
    }
    let (lat1, lat2) = ((a1 * 1e6) as i32, (a2 * 1e6) as i32);
    let (lon1, lon2) = ((o1 * 1e6) as i32, (o2 * 1e6) as i32);
    let ranges: Vec<(i32, i32)> = if lon1 <= lon2 {
        vec![(lon1, lon2)]
    } else {
        vec![(lon1, 180_000_000), (-180_000_000, lon2)]
    };
    let mut out: Vec<Hit> = Vec::new();
    let mut closest: Option<(f64, Hit)> = None;
    for (from, to) in ranges {
        for e in lon_range(list, from, to) {
            if !in_lat_range(e, lat1, lat2, bincraft) {
                continue;
            }
            let v = View(&e.bin);
            let (elat, elon) = (v.lat() as f64 / 1e6, v.lon() as f64 / 1e6);
            let dist = greatcircle(lat, lon, elat, elon, false);
            if dist < radius {
                let h = Hit {
                    e,
                    dist: dist as f32,
                    dir: bearing(lat, lon, elat, elon) as f32,
                };
                if only_closest {
                    if closest.as_ref().is_none_or(|(d, _)| dist < *d) {
                        closest = Some((dist, h));
                    }
                } else {
                    out.push(h);
                }
            }
        }
    }
    if let Some((_, h)) = closest {
        out.push(h);
    }
    out
}

fn json_reply(snap: &Snapshot, q: &Query, hits: &[Hit], elapsed_ms: f64) -> Vec<u8> {
    let mut s = String::new();
    if q.jamesv2 {
        s.push_str("{\"ac\":[");
    } else {
        let _ = write!(
            s,
            "{{\"now\": {:.3}\n,\"aircraft\":[",
            snap.now_ms as f64 / 1000.0
        );
    }
    for h in hits {
        s.push('\n');
        if q.circle.is_some() {
            // the cached object ends with "}"; put the distance and bearing inside it
            let body = h.e.json.strip_suffix('}').unwrap_or(&h.e.json);
            let _ = write!(
                s,
                "{body},\"dst\":{:.3},\"dir\":{:.1}}},",
                h.dist as f64 / 1852.0,
                h.dir
            );
        } else {
            s.push_str(&h.e.json);
            s.push(',');
        }
    }
    if s.ends_with(',') {
        s.pop();
    }
    s.push_str("\n]");
    if q.jamesv2 {
        let _ = write!(
            s,
            "\n,\"msg\": \"No error\"\n,\"now\": {}\n,\"total\": {}\n,\"ctime\": {}\n,\"ptime\": {}",
            snap.now_ms,
            hits.len(),
            snap.now_ms,
            elapsed_ms.round() as i64
        );
    } else {
        let _ = write!(
            s,
            "\n,\"resultCount\": {}\n,\"ptime\": {:.3}",
            hits.len(),
            elapsed_ms
        );
    }
    s.push_str("\n}\n");
    s.into_bytes()
}

fn bin_reply(snap: &Snapshot, q: &Query, hits: &[Hit]) -> Vec<u8> {
    let (mut south, mut west, mut north, mut east) = (-90i16, -180i16, 90i16, 180i16);
    if let Some(b) = q.r#box {
        south = b[0].round_ties_even() as i16;
        north = b[1].round_ties_even() as i16;
        west = b[2].round_ties_even() as i16;
        east = b[3].round_ties_even() as i16;
    }
    let mut h = [0u8; RECORD_SIZE];
    h[0..8].copy_from_slice(&snap.now_ms.to_le_bytes());
    h[8..12].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
    h[12..16].copy_from_slice(&snap.aircraft_with_position.to_le_bytes());
    // 16..20: index 0
    h[20..22].copy_from_slice(&south.to_le_bytes());
    h[22..24].copy_from_slice(&west.to_le_bytes());
    h[24..26].copy_from_slice(&north.to_le_bytes());
    h[26..28].copy_from_slice(&east.to_le_bytes());
    h[28..32].copy_from_slice(&snap.message_count.to_le_bytes());
    h[32..36].copy_from_slice(&(hits.len() as u32).to_le_bytes());
    // 36..40: dummy
    h[40..44].copy_from_slice(&bincraft::VERSION.to_le_bytes());
    h[44..48].copy_from_slice(&((snap.message_rate * 10.0).round_ties_even() as u32).to_le_bytes());
    h[48..52].copy_from_slice(&snap.flags.to_le_bytes());
    let mut out = Vec::with_capacity(RECORD_SIZE * (hits.len() + 1));
    out.extend_from_slice(&h);
    for hit in hits {
        out.extend_from_slice(&hit.e.bin);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(hex: u32, lat: f64, lon: f64, pos: bool, callsign: &str, alt_ft: i32) -> Entry {
        let mut bin = [0u8; RECORD_SIZE];
        bin[0..4].copy_from_slice(&hex.to_le_bytes());
        let (la, lo) = if pos {
            ((lat * 1e6) as i32, (lon * 1e6) as i32)
        } else {
            (i32::MAX, i32::MAX)
        };
        bin[8..12].copy_from_slice(&lo.to_le_bytes());
        bin[12..16].copy_from_slice(&la.to_le_bytes());
        bin[20..22].copy_from_slice(&((alt_ft as f32 / 25.0).round() as i16).to_le_bytes());
        let set = |bin: &mut [u8; RECORD_SIZE], b: usize| bin[64 + b / 8] |= 1 << (b % 8);
        if pos {
            set(&mut bin, bit::POSITION_VALID);
        }
        set(&mut bin, bit::BARO_ALT_VALID);
        if !callsign.is_empty() {
            set(&mut bin, bit::CALLSIGN_VALID);
            bin[78..78 + callsign.len()].copy_from_slice(callsign.as_bytes());
        }
        Entry {
            bin,
            json: format!("{{\"hex\":\"{hex:06x}\"}}"),
        }
    }

    fn snap() -> Snapshot {
        let mut entries = vec![
            entry(0xa, 51.5, 0.0, true, "BAW1", 30000),
            entry(0xb, 52.0, 1.0, true, "RYR2", 5000),
            entry(0xc, 10.0, -170.0, true, "UAL3", 36000),
            entry(0xd, 0.0, 0.0, false, "", 0),
        ];
        entries.sort_by_key(|e| View(&e.bin).lon());
        Snapshot {
            now_ms: 1_700_000_000_000,
            entries,
            aircraft_with_position: 3,
            message_count: 99,
            message_rate: 12.5,
            flags: 1,
        }
    }

    fn hexes(body: &[u8]) -> Vec<String> {
        let s = String::from_utf8(body.to_vec()).unwrap();
        s.split("\"hex\":\"")
            .skip(1)
            .map(|r| r[..6].to_string())
            .collect()
    }

    #[test]
    fn query_rules() {
        assert!(parse_query("all").is_some());
        assert!(
            parse_query("all&all_with_pos").is_none(),
            "one main option only"
        );
        assert!(
            parse_query("box=1,2,3,4&find_hex=a").is_some(),
            "box with a hex list is allowed"
        );
        assert!(parse_query("box=2,1,3,4").is_none(), "south above north");
        assert!(parse_query("box=1,2,3").is_none());
        assert!(parse_query("circle=91,0,5").is_none());
        assert!(parse_query("bogus").is_none());
        assert!(parse_query("find_squawk=7700&filter_squawk=7600").is_none());
        let q = parse_query("circle=51.5,0,10&jv2&zstd").unwrap();
        assert_eq!(q.circle, Some((51.5, 0.0, 10.0 * 1852.0, false)));
        assert!(q.jamesv2 && q.zstd);
        assert!(parse_query("closest=1,2,3").unwrap().circle.unwrap().3);
    }

    #[test]
    fn box_and_all_keep_longitude_order() {
        let s = snap();
        let all = reply(&s, &parse_query("all").unwrap(), 0.0);
        assert_eq!(hexes(&all), ["00000c", "00000a", "00000b", "00000d"]);
        let with_pos = reply(&s, &parse_query("all_with_pos").unwrap(), 0.0);
        assert_eq!(hexes(&with_pos), ["00000c", "00000a", "00000b"]);
        let b = reply(&s, &parse_query("box=50,53,-1,2").unwrap(), 0.0);
        assert_eq!(hexes(&b), ["00000a", "00000b"]);
        // a box across the antimeridian
        let wrap = reply(&s, &parse_query("box=0,20,170,-160").unwrap(), 0.0);
        assert_eq!(hexes(&wrap), ["00000c"]);
    }

    #[test]
    fn circle_adds_distance_and_closest_keeps_one() {
        let s = snap();
        let c =
            String::from_utf8(reply(&s, &parse_query("circle=51.5,0,10").unwrap(), 0.0)).unwrap();
        assert!(
            c.contains("\"hex\":\"00000a\",\"dst\":0.000,\"dir\":"),
            "{c}"
        );
        assert!(!c.contains("00000b"), "the second is more than 10 nmi away");
        let c = reply(&s, &parse_query("closest=51.9,0.9,500").unwrap(), 0.0);
        assert_eq!(hexes(&c), ["00000b"]);
    }

    #[test]
    fn lists_and_filters() {
        let s = snap();
        assert_eq!(
            hexes(&reply(&s, &parse_query("find_hex=b,~zz,a").unwrap(), 0.0)),
            ["00000b", "00000a"]
        );
        assert_eq!(
            hexes(&reply(&s, &parse_query("find_callsign=baw1").unwrap(), 0.0)),
            ["00000a"]
        );
        assert_eq!(
            hexes(&reply(
                &s,
                &parse_query("all&filter_callsign_prefix=r").unwrap(),
                0.0
            )),
            ["00000b"]
        );
        assert_eq!(
            hexes(&reply(
                &s,
                &parse_query("all&above_alt_baro=20000").unwrap(),
                0.0
            )),
            ["00000c", "00000a"]
        );
        // the aircraft at 0 ft with a valid altitude counts as below 10000 as well
        assert_eq!(
            hexes(&reply(
                &s,
                &parse_query("all&below_alt_baro=10000").unwrap(),
                0.0
            )),
            ["00000b", "00000d"]
        );
        assert!(
            hexes(&reply(&s, &parse_query("all&filter_mil").unwrap(), 0.0)).is_empty(),
            "no database loaded"
        );
    }

    #[test]
    fn json_and_binary_replies() {
        let s = snap();
        let j = String::from_utf8(reply(&s, &parse_query("find_hex=a").unwrap(), 1.5)).unwrap();
        assert!(j.starts_with("{\"now\": 1700000000.000\n,\"aircraft\":["));
        assert!(
            j.ends_with("\n]\n,\"resultCount\": 1\n,\"ptime\": 1.500\n}\n"),
            "{j}"
        );
        let v2 =
            String::from_utf8(reply(&s, &parse_query("find_hex=a&jv2").unwrap(), 1.5)).unwrap();
        assert!(v2.starts_with("{\"ac\":["));
        assert!(
            v2.contains("\n,\"msg\": \"No error\"\n,\"now\": 1700000000000\n,\"total\": 1"),
            "{v2}"
        );
        let b = reply(&s, &parse_query("find_hex=a,b&bincraft").unwrap(), 0.0);
        assert_eq!(b.len(), 3 * RECORD_SIZE);
        assert_eq!(&b[32..36], &2u32.to_le_bytes(), "resultCount in the header");
        assert_eq!(&b[40..44], &bincraft::VERSION.to_le_bytes());
        assert_eq!(&b[RECORD_SIZE..RECORD_SIZE + 4], &0xau32.to_le_bytes());
    }
}
