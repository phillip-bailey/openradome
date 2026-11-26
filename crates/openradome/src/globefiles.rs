//! The files behind `--write-json-globe-index` and `--write-globe-history`:
//!
//! * `globe_XXXX.binCraft.zst` (and `.binCraft` gzipped with `--write-binCraft-old`): the aircraft in
//!   each globe tile, which tar1090 fetches for the part of the map on screen;
//! * `traces/XX/trace_recent_<hex>.json` and `trace_full_<hex>.json` (gzipped): each aircraft's track;
//! * `<history>/YYYY/MM/DD/traces/XX/trace_full_<hex>.json`: one gzipped file per aircraft and UTC day.
//!
//! Scheduling follows upstream's `traceWrite`: a recent trace is rewritten whenever a point was added,
//! the full trace when about 90 points were added or after 30 minutes (twice that for an aircraft that
//! has been quiet for an hour), and the history file every ten minutes while it changes.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use openradome_core::bincraft;
use openradome_core::globe;
use openradome_core::mode_s::NON_ICAO_ADDRESS;
use openradome_core::trace::{self, Kind, RECENT_POINTS};
use openradome_core::track::{HOURS, KEEP_TRACES, MINUTES, Tracker};

use crate::jsonout::{gzip_level, write_atomic, zstd};

/// Upstream's `GLOBE_MEM_IVAL`.
const FULL_EVERY: i64 = 30 * MINUTES;
/// Upstream's `GLOBE_PERM_IVAL`: how often a history file is considered.
const PERM_EVERY: i64 = 10 * MINUTES;

/// `--json-trace-hist-only` bits: 1 = no recent files, 2 = no full files.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub tiles: bool,
    pub traces: bool,
    pub bin_gz: bool,
    pub hist_only: u8,
    pub history_dir: Option<PathBuf>,
    pub full_trace_dir: Option<PathBuf>,
}

pub struct GlobeWriter {
    dir: PathBuf,
    opts: Options,
    version: &'static str,
    indexes: Vec<i32>,
    /// Tiles whose file currently lists aircraft (so an emptied tile is rewritten once, empty).
    nonempty: HashSet<i32>,
    made_dirs: HashSet<String>,
}

impl GlobeWriter {
    pub fn new(dir: &Path, opts: Options, version: &'static str) -> io::Result<Self> {
        let w = Self {
            dir: dir.to_path_buf(),
            opts,
            version,
            indexes: globe::all_indexes(),
            nonempty: HashSet::new(),
            made_dirs: HashSet::new(),
        };
        if w.opts.tiles {
            // every tile gets a (small, empty) file once, so clients never see a missing one
            let empty_header = |i: i32| bincraft::globe_bin_empty(i);
            for &i in &w.indexes {
                let bin = empty_header(i);
                write_atomic(&w.dir, &format!("globe_{i:04}.binCraft.zst"), zstd(&bin))?;
            }
        }
        Ok(w)
    }

    /// Write what is due. `message_rate` is the decoder-wide rate for the headers.
    /// `final_write`: at exit, write every full trace so none is lost.
    pub fn tick(
        &mut self,
        now: i64,
        t: &mut Tracker,
        message_rate: f64,
        final_write: bool,
    ) -> io::Result<()> {
        if self.opts.tiles {
            self.write_tiles(now, t, message_rate)?;
        }
        if self.opts.traces {
            self.write_traces(now, t, final_write)?;
        }
        Ok(())
    }

    fn write_tiles(&mut self, now: i64, t: &Tracker, message_rate: f64) -> io::Result<()> {
        let mut by_tile: HashMap<i32, usize> = HashMap::new();
        for a in t.active() {
            if a.globe_index >= 0 && Tracker::include_in_json(now, a) {
                *by_tile.entry(a.globe_index).or_default() += 1;
            }
        }
        let now_tiles: HashSet<i32> = by_tile.keys().copied().collect();
        let emptied: Vec<i32> = self.nonempty.difference(&now_tiles).copied().collect();
        for i in now_tiles.iter().copied().chain(emptied) {
            let bin = bincraft::globe_bin(t, i, false, now, message_rate);
            if self.opts.bin_gz {
                write_atomic(
                    &self.dir,
                    &format!("globe_{i:04}.binCraft"),
                    gzip_level(&bin, 1)?,
                )?;
            }
            write_atomic(&self.dir, &format!("globe_{i:04}.binCraft.zst"), zstd(&bin))?;
        }
        self.nonempty = now_tiles;
        Ok(())
    }

    fn trace_dir(&mut self, base: &Path, addr: u32) -> io::Result<PathBuf> {
        let rel = format!("traces/{:02x}", addr % 256);
        let full = base.join(&rel);
        if self.made_dirs.insert(full.display().to_string()) {
            fs::create_dir_all(&full)?;
        }
        Ok(full)
    }

    fn write_traces(&mut self, now: i64, t: &mut Tracker, final_write: bool) -> io::Result<()> {
        let version = self.version;
        let dir = self.dir.clone();
        let hist_dir = self.opts.history_dir.clone();
        let full_dir = self.opts.full_trace_dir.clone();
        let mask = self.opts.hist_only;
        let mut work: Vec<(u32, bool, bool, bool)> = Vec::new();
        for a in t.aircraft_iter_mut() {
            let tr = &mut a.trace;
            if tr.points.is_empty() {
                continue;
            }
            let last_ts = tr.points.last().map_or(0, |p| p.timestamp);
            let mut recent = tr.recent_pending && mask & 1 == 0;
            tr.recent_pending = false;
            // full: first time, enough new points, or on schedule
            let mut full = tr.next_full == 0
                || tr.write_counter as usize >= RECENT_POINTS - 2
                || now >= tr.next_full
                || final_write;
            if full && last_ts < now - KEEP_TRACES {
                full = false; // all data is older than we keep
            }
            if tr.next_full == 0 {
                recent = recent || mask & 1 == 0;
            }
            if full {
                let quiet = now - a.seen_pos_reliable > HOURS;
                let jitter = (a.addr as i64 * 2_654_435_761 % (FULL_EVERY / 8)).abs();
                let ival = FULL_EVERY + jitter;
                tr.next_full = now + if quiet { 2 * ival } else { ival };
                tr.write_counter = 0;
            }
            if mask & 2 != 0 {
                full = false;
            }
            let mut perm = false;
            if hist_dir.is_some() && now >= tr.next_perm {
                tr.next_perm = now + PERM_EVERY + (a.addr as i64 % 60) * 1000;
                perm = tr.perm_last != last_ts;
            }
            if recent || full || perm {
                work.push((a.addr, recent, full, perm));
            }
        }
        for (addr, recent, full, perm) in work {
            let Some(a) = t.aircraft_by_addr(addr) else {
                continue;
            };
            let name = |kind: &str| {
                format!(
                    "trace_{kind}_{}{:06x}.json",
                    if addr & NON_ICAO_ADDRESS != 0 {
                        "~"
                    } else {
                        ""
                    },
                    addr & 0xFF_FFFF
                )
            };
            let points = &a.trace.points;
            if recent {
                let json = trace::trace_json(addr, points, Kind::Recent, 0, None, 0, version);
                let d = self.trace_dir(&dir, addr)?;
                write_atomic(&d, &name("recent"), gzip_level(json.as_bytes(), 1)?)?;
            }
            if full {
                let start = points.partition_point(|p| p.timestamp < now - KEEP_TRACES);
                if start < points.len() {
                    let json = trace::trace_json(addr, points, Kind::Full, start, None, 0, version);
                    let gz = gzip_level(json.as_bytes(), 5)?;
                    match &full_dir {
                        Some(fd) => {
                            let d = self.trace_dir(fd, addr)?;
                            write_atomic(&d, &name("full"), gz)?;
                            link_full_trace(
                                &d.join(name("full")),
                                &self.trace_dir(&dir, addr)?.join(name("full")),
                            );
                        }
                        None => {
                            let d = self.trace_dir(&dir, addr)?;
                            write_atomic(&d, &name("full"), gz)?;
                        }
                    }
                }
            }
            if perm && let Some(hd) = &hist_dir {
                let written = write_perm(hd, addr, points, now, version, &name("full"))?;
                if let Some(a) = t.aircraft_iter_mut().find(|a| a.addr == addr) {
                    a.trace.perm_last = written.unwrap_or(a.trace.perm_last);
                }
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn link_full_trace(target: &Path, link: &Path) {
    let _ = std::os::unix::fs::symlink(target, link);
}

#[cfg(not(unix))]
fn link_full_trace(_target: &Path, _link: &Path) {}

/// (year, month, day) of a UTC time in ms since the epoch (days-from-civil, Howard Hinnant's algorithm).
pub fn utc_date(ms: i64) -> (i64, u32, u32) {
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The permanent history file for the UTC day that began 55 minutes ago (so a flight that crosses
/// midnight lands in the day it mostly belongs to). Returns the last timestamp written.
fn write_perm(
    history: &Path,
    addr: u32,
    points: &[trace::Point],
    now: i64,
    version: &str,
    file_name: &str,
) -> io::Result<Option<i64>> {
    let day_ms = now - 55 * MINUTES;
    let start_of_day = day_ms.div_euclid(86_400_000) * 86_400_000;
    let end_of_day = start_of_day + 86_400_000;
    let start = points.partition_point(|p| p.timestamp < start_of_day);
    let end = points.partition_point(|p| p.timestamp < end_of_day);
    if start >= end {
        return Ok(None);
    }
    let (first, last) = (&points[start], &points[end - 1]);
    // non-ICAO traces that are only on the ground are not worth keeping
    if addr & NON_ICAO_ADDRESS != 0
        && (first.on_ground || first.baro_alt.is_none())
        && (last.on_ground || last.baro_alt.is_none())
    {
        return Ok(Some(last.timestamp));
    }
    let json = trace::trace_json(
        addr,
        points,
        Kind::Full,
        start,
        Some(end - 1),
        start_of_day,
        version,
    );
    let (y, m, d) = utc_date(day_ms);
    let dir = history.join(format!("{y:04}/{m:02}/{d:02}/traces/{:02x}", addr % 256));
    fs::create_dir_all(&dir)?;
    write_atomic(&dir, file_name, gzip_level(json.as_bytes(), 9)?)?;
    Ok(Some(last.timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_from_epoch_milliseconds() {
        assert_eq!(utc_date(0), (1970, 1, 1));
        assert_eq!(utc_date(951_782_400_000), (2000, 2, 29));
        assert_eq!(utc_date(1_795_554_917_256), (2026, 11, 24));
        assert_eq!(utc_date(-1), (1969, 12, 31));
    }
}
