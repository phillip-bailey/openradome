//! `--interactive`: the aircraft table on the terminal, refreshed twice a second.
//!
//! A port of upstream's `interactive.c`. It draws with plain ANSI escape codes on the terminal's
//! alternate screen instead of ncurses, so there is no C library to link. Columns, widths and
//! sorting follow upstream: nearest first when the receiver position is known, otherwise by altitude.

use std::fmt::Write as _;
use std::io::{self, Write};

use rustadsb_core::mode_s::{AirGround, NON_ICAO_ADDRESS};
use rustadsb_core::track::{Aircraft, Tracker};

/// Upstream's `MODES_INTERACTIVE_REFRESH_TIME`.
const REFRESH_MS: i64 = 500;
/// The list is re-sorted every three seconds, not on every refresh.
const SORT_EVERY_MS: i64 = 3_000;
/// Upstream's `MODES_INTERACTIVE_DISPLAY_TTL` (the `--interactive-ttl` default is 60 s).
#[cfg(test)]
const DEFAULT_TTL_MS: i64 = 60_000;

pub struct Interactive {
    ttl_ms: i64,
    metric: bool,
    use_gnss: bool,
    has_location: bool,
    next_update: i64,
    next_sort: i64,
    next_size: i64,
    order: Vec<u32>,
    rows: usize,
    started: bool,
}

impl Interactive {
    pub fn new(ttl_ms: i64, metric: bool, use_gnss: bool, has_location: bool) -> Self {
        Self {
            ttl_ms,
            metric,
            use_gnss,
            has_location,
            next_update: 0,
            next_sort: 0,
            next_size: 0,
            order: Vec::new(),
            rows: 24,
            started: false,
        }
    }

    /// Switch to the alternate screen and hide the cursor.
    pub fn start(&mut self) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J");
        let _ = out.flush();
        self.started = true;
    }

    /// Give the terminal back.
    pub fn stop(&mut self) {
        if self.started {
            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[?25h\x1b[?1049l");
            let _ = out.flush();
            self.started = false;
        }
    }

    /// Redraw if it is time. Call as often as you like.
    pub fn show(&mut self, now: i64, t: &Tracker) {
        if now < self.next_update {
            return;
        }
        self.next_update = now + REFRESH_MS;
        if now >= self.next_size {
            self.next_size = now + 5_000;
            if let Some(r) = terminal_rows() {
                self.rows = r;
            }
        }
        if now >= self.next_sort {
            self.next_sort = now + SORT_EVERY_MS;
            self.sort(t);
        }
        let screen = self.render(now, t);
        let mut out = io::stdout().lock();
        let _ = out.write_all(screen.as_bytes());
        let _ = out.flush();
    }

    fn sort(&mut self, t: &Tracker) {
        let mut list: Vec<&Aircraft> = t.active().collect();
        if self.has_location {
            list.sort_by(|a, b| compare_dist(a, b));
        } else {
            list.sort_by(|a, b| compare_alt(a, b));
        }
        self.order = list.iter().map(|a| a.addr).collect();
    }

    fn distance(&self, meters: i32) -> f64 {
        if self.metric {
            meters as f64 / 1000.0
        } else {
            meters as f64 / 1852.0
        }
    }

    fn altitude(&self, ft: i32) -> i32 {
        if self.metric {
            (ft as f64 / 3.2828) as i32
        } else {
            ft
        }
    }

    fn speed(&self, kts: i32) -> i32 {
        if self.metric {
            (kts as f64 * 1.852) as i32
        } else {
            kts
        }
    }

    /// The whole screen as one string (cursor home, header, one line per aircraft, clear below).
    pub fn render(&self, now: i64, t: &Tracker) -> String {
        let spinner = ['|', '/', '-', '\\'][((now / 1000) % 4) as usize];
        let mut s = String::from("\x1b[H");
        let header = if self.has_location {
            " Hex    Mode  Sqwk  Flight     Alt   Spd  Hdg     Dist      Dir  RSSI  Msgs Seen"
        } else {
            " Hex    Mode  Sqwk  Flight     Alt   Spd  Hdg      Lat     Long  RSSI  Msgs Seen"
        };
        s.push(spinner);
        s.push_str(&header[1..]);
        s.push_str("\x1b[K\r\n");
        s.push_str(&"\u{2500}".repeat(80));
        s.push_str("\x1b[K\r\n");
        let mut row = 2;
        // The sorted list is three seconds old at most: aircraft that appeared since come last, in
        // the tracker's own order, and ones that expired are skipped.
        let known: std::collections::HashSet<u32> = self.order.iter().copied().collect();
        let newer = t.active().map(|a| a.addr).filter(|a| !known.contains(a));
        for addr in self.order.iter().copied().chain(newer) {
            if row >= self.rows {
                break;
            }
            let Some(a) = t.aircraft_by_addr(addr) else {
                continue;
            };
            if now - a.seen >= self.ttl_ms || a.messages <= 1 {
                continue;
            }
            s.push_str(&self.line(now, a));
            s.push_str("\x1b[K\r\n");
            row += 1;
        }
        s.push_str("\x1b[J");
        s
    }

    fn line(&self, now: i64, a: &Aircraft) -> String {
        let squawk = if a.squawk_valid.valid() {
            format!("{:04x}", a.squawk)
        } else {
            " ".into()
        };
        let gs = if a.gs_valid.valid() {
            format!("{:4}", self.speed(a.gs as i32))
        } else {
            " ".into()
        };
        let track = if a.track_valid.valid() {
            format!("{:03.0}", a.track as f64)
        } else {
            " ".into()
        };
        let (mut lat, mut lon) = (String::from(" "), String::from(" "));
        if a.position_valid.valid() {
            if self.has_location {
                lat = format!("{:7.1}", self.distance(a.receiver_distance as i32));
                lon = format!("{:8.0}", a.receiver_direction);
            } else {
                lat = format!("{:7.3}", a.lat);
                lon = format!("{:8.3}", a.lon);
            }
        }
        let fl = if a.airground_valid.valid() && a.airground == AirGround::Ground {
            " grnd ".to_string()
        } else if self.use_gnss && a.geom_alt_valid.valid() {
            format!("{:5}H", self.altitude(a.geom_alt))
        } else if a.baro_alt_valid.valid() {
            format!("{:5} ", self.altitude(a.baro_alt))
        } else {
            " ".into()
        };
        let signal_average = a.signal_level.iter().map(|&x| x as f64).sum::<f64>() / 8.0;
        let msgs = a.messages.min(99_999);
        let mut s = String::new();
        let _ = write!(
            s,
            "{}{:06X} {:<4}  {:<4}  {:<8} {:>6} {:>4}  {:>3}  {:>7} {:>8} {:5.1} {:5}   {:2.0}",
            if a.addr & NON_ICAO_ADDRESS != 0 {
                "~"
            } else {
                " "
            },
            a.addr & 0xff_ffff,
            "S",
            squawk,
            a.callsign,
            fl,
            gs,
            track,
            lat,
            lon,
            10.0 * signal_average.log10(),
            msgs,
            (now - a.seen) as f64 / 1000.0
        );
        s
    }
}

impl Drop for Interactive {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Upstream's `compareDist`: aircraft with a position first, nearest first; the rest by address.
fn compare_dist(a: &Aircraft, b: &Aircraft) -> std::cmp::Ordering {
    let (va, vb) = (a.position_valid.valid(), b.position_valid.valid());
    match (va, vb) {
        (false, false) => a.addr.cmp(&b.addr),
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (true, true) => a
            .receiver_distance
            .partial_cmp(&b.receiver_distance)
            .unwrap_or(std::cmp::Ordering::Equal),
    }
}

/// Upstream's `compareAlt`: aircraft on the ground first, then those with an altitude, lowest
/// first, then the rest, each tie broken by address.
fn compare_alt(a: &Aircraft, b: &Aircraft) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let g = |x: &Aircraft| x.airground_valid.valid() && x.airground == AirGround::Ground;
    let (ga, gb) = (g(a), g(b));
    if ga || gb {
        return if ga && gb {
            a.addr.cmp(&b.addr)
        } else if ga {
            Less
        } else {
            Greater
        };
    }
    let (va, vb) = (a.baro_alt_valid.valid(), b.baro_alt_valid.valid());
    match (va, vb) {
        (false, false) => a.addr.cmp(&b.addr),
        (true, false) => Less,
        (false, true) => Greater,
        (true, true) => a
            .baro_alt
            .cmp(&b.baro_alt)
            .then_with(|| a.addr.cmp(&b.addr)),
    }
}

/// The terminal's height in rows: `stty size` on the controlling terminal (no unsafe, no libc).
fn terminal_rows() -> Option<usize> {
    #[cfg(unix)]
    {
        let tty = std::fs::File::open("/dev/tty").ok()?;
        let out = std::process::Command::new("stty")
            .arg("size")
            .stdin(tty)
            .output()
            .ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        text.split_whitespace().next()?.parse().ok()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_has_the_upstream_columns() {
        let t = Tracker::new(rustadsb_core::track::TrackerConfig::default());
        let ui = Interactive::new(DEFAULT_TTL_MS, false, false, false);
        let s = ui.render(0, &t);
        assert!(s.contains(
            "Hex    Mode  Sqwk  Flight     Alt   Spd  Hdg      Lat     Long  RSSI  Msgs Seen"
        ));
        let ui = Interactive::new(DEFAULT_TTL_MS, false, false, true);
        assert!(ui.render(0, &t).contains("Hdg     Dist      Dir"));
    }

    #[test]
    fn units_convert_like_upstream() {
        let ui = Interactive::new(DEFAULT_TTL_MS, true, false, false);
        assert_eq!(ui.altitude(10_000), 3046); // ft / 3.2828, truncated
        assert_eq!(ui.speed(100), 185); // kts * 1.852, truncated
        assert!((ui.distance(5_000) - 5.0).abs() < 1e-9);
        let ui = Interactive::new(DEFAULT_TTL_MS, false, false, false);
        assert_eq!(ui.altitude(10_000), 10_000);
        assert!((ui.distance(1852) - 1.0).abs() < 1e-9);
    }
}
