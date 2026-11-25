//! `--dashboard-port`: a status page for people, as opposed to the JSON files and the API for programs.
//!
//! The engine hands over each closed 10 s statistics bucket; they are summed into one point a minute
//! and the last 24 hours are kept (1440 points, a few tens of kB). A tokio task answers `GET /` with
//! one HTML page drawn here: live figures, SVG graphs and the open connections. The page has no
//! JavaScript (it reloads itself every 10 s) and loads nothing from elsewhere, so it works on a
//! station with no internet; its Content-Security-Policy forbids scripts and outside resources.
//! Nothing is drawn unless someone asks for the page.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustadsb_core::json::StatsBlock;
use rustadsb_core::track::Tracker;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Handle;

use crate::net::{self, ClientInfo};

/// Points kept: one a minute for 24 hours.
pub const HISTORY: usize = 24 * 60;
const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 24 * 60 * MINUTE_MS;
/// A request larger than this is refused.
const REQUEST_MAX: usize = 8 * 1024;
/// How long a client may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the page reloads itself, in seconds.
const REFRESH_S: u32 = 10;

/// One minute of statistics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Point {
    /// End of the minute, ms since the Unix epoch.
    pub t: i64,
    /// Usable messages per second.
    pub msg_rate: f64,
    /// Aircraft shown in aircraft.json at the end of the minute, and those of them with a position.
    pub aircraft: u32,
    pub with_pos: u32,
    /// Furthest position decoded in the minute, km.
    pub range_km: Option<f64>,
    /// Mean signal and noise power, dBFS (none without a local receiver).
    pub signal_db: Option<f64>,
    pub noise_db: Option<f64>,
    /// Messages above -3 dBFS (too strong: the gain may be too high).
    pub strong: u64,
    /// Samples the receiver dropped.
    pub dropped: u64,
    /// Positions decoded.
    pub positions: u64,
    /// The decoder's CPU use over the minute, % of one core (Linux only).
    pub cpu_pct: Option<f64>,
}

/// The buckets of the minute being summed.
#[derive(Debug, Default)]
struct Minute {
    start: i64,
    end: i64,
    messages: u64,
    signal_sum: f64,
    signal_count: u64,
    noise_sum: f64,
    noise_count: u64,
    strong: u64,
    dropped: u64,
    positions: u64,
    distance_max: u64,
}

impl Minute {
    fn add(&mut self, b: &StatsBlock) {
        if self.end == 0 {
            self.start = b.start;
        }
        self.end = b.end;
        self.messages += b.track.messages_total;
        self.signal_sum += b.demod.signal_power_sum;
        self.signal_count += b.demod.signal_power_count;
        self.noise_sum += b.demod.noise_power_sum;
        self.noise_count += b.demod.noise_power_count;
        self.strong += b.demod.strong_signal_count;
        self.dropped += b.samples_dropped;
        self.positions += b.track.pos_all;
        self.distance_max = self.distance_max.max(b.track.distance_max);
    }

    fn point(&self, aircraft: u32, with_pos: u32, cpu_pct: Option<f64>) -> Point {
        let secs = (self.end - self.start).max(1) as f64 / 1000.0;
        let db = |sum: f64, n: u64| (sum > 0.0 && n > 0).then(|| 10.0 * (sum / n as f64).log10());
        Point {
            t: self.end,
            msg_rate: self.messages as f64 / secs,
            aircraft,
            with_pos,
            range_km: (self.distance_max > 0).then(|| self.distance_max as f64 / 1000.0),
            signal_db: db(self.signal_sum, self.signal_count),
            noise_db: db(self.noise_sum, self.noise_count),
            strong: self.strong,
            dropped: self.dropped,
            positions: self.positions,
            cpu_pct,
        }
    }
}

/// The figures at the top of the page, refreshed every second.
#[derive(Debug, Clone, Default)]
struct Live {
    now: i64,
    aircraft: u32,
    with_pos: u32,
    /// Messages per second over the last 10 s bucket.
    msg_rate: f64,
    messages_total: u64,
    rss_kb: Option<u64>,
}

#[derive(Debug, Default)]
struct State {
    history: VecDeque<Point>,
    live: Live,
}

struct Shared {
    state: Mutex<State>,
    version: &'static str,
    started: i64,
    /// There is a local receiver (SDR or IQ file), so signal and noise mean something.
    local: bool,
}

/// The engine's half: collects the figures. The page is served by a task on the runtime.
pub struct Dashboard {
    shared: Arc<Shared>,
    minute: Minute,
    next_live: i64,
    /// CPU time used by this process at the last point (ms) and when that was taken.
    cpu_last: Option<(u64, i64)>,
}

impl Dashboard {
    /// Listen on `bind:port` and start serving the page.
    pub fn start(
        rt: &Handle,
        bind: IpAddr,
        port: u16,
        now: i64,
        local: bool,
        version: &'static str,
    ) -> io::Result<Self> {
        let std_listener = std::net::TcpListener::bind(SocketAddr::new(bind, port))?;
        std_listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            version,
            started: now,
            local,
        });
        let listener = {
            let _guard = rt.enter();
            TcpListener::from_std(std_listener)?
        };
        let s = shared.clone();
        rt.spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((sock, _)) => {
                        tokio::spawn(serve(sock, s.clone()));
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
        });
        Ok(Self {
            shared,
            minute: Minute::default(),
            next_live: now,
            cpu_last: process_cpu_ms().map(|c| (c, now)),
        })
    }

    /// Call as time advances, with the bucket the statistics recorder has just closed, if any.
    pub fn tick(&mut self, now: i64, tracker: &Tracker, closed: Option<&StatsBlock>) {
        if let Some(b) = closed {
            self.minute.add(b);
            if self.minute.end - self.minute.start >= MINUTE_MS - 1_000 {
                let c = tracker.counts(now);
                let cpu = process_cpu_ms();
                let cpu_pct = match (cpu, self.cpu_last) {
                    (Some(c1), Some((c0, t0))) if now > t0 => {
                        Some(c1.saturating_sub(c0) as f64 * 100.0 / (now - t0) as f64)
                    }
                    _ => None,
                };
                self.cpu_last = cpu.map(|c| (c, now));
                let point = self.minute.point(
                    c.with_position + c.without_position,
                    c.with_position,
                    cpu_pct,
                );
                self.minute = Minute::default();
                let mut st = self.lock();
                st.history.push_back(point);
                while st.history.len() > HISTORY {
                    st.history.pop_front();
                }
            }
            let secs = (b.end - b.start).max(1) as f64 / 1000.0;
            self.lock().live.msg_rate = b.track.messages_total as f64 / secs;
        }
        if crate::jsonout::due(now, self.next_live, 1_000) {
            self.next_live = now + 1_000;
            let c = tracker.counts(now);
            let rss_kb = process_rss_kb();
            let mut st = self.lock();
            st.live.now = now;
            st.live.aircraft = c.with_position + c.without_position;
            st.live.with_pos = c.with_position;
            st.live.messages_total = tracker.stats.messages_total;
            st.live.rss_kb = rss_kb;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// CPU time (user + system) this process has used, in ms. Linux reports it in clock ticks, which
/// are 1/100 s on every Linux this runs on.
#[cfg(target_os = "linux")]
fn process_cpu_ms() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // the command name, field 2, is in parentheses and may contain spaces
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // utime and stime are fields 14 and 15; `rest` starts at field 3
    let ticks: u64 = fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
    Some(ticks * 10)
}

#[cfg(not(target_os = "linux"))]
fn process_cpu_ms() -> Option<u64> {
    None
}

/// Resident memory of this process, kB.
#[cfg(target_os = "linux")]
fn process_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn process_rss_kb() -> Option<u64> {
    None
}

// ---- HTTP ----------------------------------------------------------------------------------------

async fn serve(mut sock: TcpStream, shared: Arc<Shared>) {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let read = async {
        loop {
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                return true;
            }
            if buf.len() > REQUEST_MAX {
                return false;
            }
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return false,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    };
    let complete = matches!(tokio::time::timeout(REQUEST_TIMEOUT, read).await, Ok(true));
    let reply = if complete {
        let page = || {
            let (history, live) = {
                let st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                (
                    st.history.iter().cloned().collect::<Vec<_>>(),
                    st.live.clone(),
                )
            };
            render(&PageData {
                version: shared.version,
                started: shared.started,
                local: shared.local,
                now: live.now.max(net::now_ms()),
                live,
                history,
                clients: net::clients(),
            })
        };
        respond(&buf, page)
    } else {
        reply(
            "400 Bad Request",
            "text/plain; charset=utf-8",
            b"bad request\n",
            false,
        )
    };
    let _ = sock.write_all(&reply).await;
    let _ = sock.shutdown().await;
}

/// The reply to one request. `page` draws the page, only when it is asked for.
fn respond(request: &[u8], page: impl FnOnce() -> String) -> Vec<u8> {
    let text = String::from_utf8_lossy(request);
    let first = text.lines().next().unwrap_or("");
    let mut parts = first.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let head = method == "HEAD";
    if method != "GET" && !head {
        return reply(
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            b"GET only\n",
            false,
        );
    }
    let path = target.split('?').next().unwrap_or("");
    match path {
        "/" | "/index.html" => {
            let body = page();
            reply("200 OK", "text/html; charset=utf-8", body.as_bytes(), head)
        }
        "/favicon.ico" => reply("204 No Content", "text/plain", b"", head),
        _ => reply(
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"not found\n",
            head,
        ),
    }
}

fn reply(status: &str, content_type: &str, body: &[u8], head_only: bool) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nServer: rustadsb\r\nContent-Type: {content_type}\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; img-src data:\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         Cache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    if !head_only {
        out.extend_from_slice(body);
    }
    out
}

// ---- the page ------------------------------------------------------------------------------------

struct PageData {
    version: &'static str,
    started: i64,
    local: bool,
    now: i64,
    live: Live,
    history: Vec<Point>,
    clients: Vec<ClientInfo>,
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

/// "3 d 4 h", "5 h 12 min", "12 min", "45 s".
fn duration(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (d, h, m) = (s / 86_400, s / 3600 % 24, s / 60 % 60);
    match () {
        _ if d > 0 => format!("{d} d {h} h"),
        _ if h > 0 => format!("{h} h {m} min"),
        _ if m > 0 => format!("{m} min"),
        _ => format!("{s} s"),
    }
}

/// A number with sensible precision for its size.
fn num(v: f64) -> String {
    if v.abs() >= 100.0 || v == v.trunc() {
        format!("{v:.0}")
    } else if v.abs() >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

fn opt(v: Option<f64>, unit: &str) -> String {
    v.map_or_else(|| "–".to_string(), |v| format!("{}{unit}", num(v)))
}

fn tile(label: &str, value: &str, note: &str) -> String {
    format!(
        "<div class=\"tile\"><div class=\"label\">{}</div><div class=\"value\">{}</div><div class=\"note\">{}</div></div>",
        esc(label),
        value,
        note
    )
}

/// One line of a graph.
struct Series<'a> {
    name: &'a str,
    /// CSS class giving its colour: `s1` or `s2`.
    class: &'a str,
    values: Vec<(i64, Option<f64>)>,
}

const W: f64 = 480.0;
const H: f64 = 150.0;
const LEFT: f64 = 44.0;
const BOTTOM: f64 = 24.0;
const TOP: f64 = 8.0;

/// A round step (1, 2, 2.5 or 5 times a power of ten) that splits `range` into about four parts.
fn nice_step(range: f64) -> f64 {
    let raw = (range / 4.0).max(1e-9);
    let mag = 10f64.powf(raw.log10().floor());
    let f = raw / mag;
    mag * match f {
        f if f <= 1.0 => 1.0,
        f if f <= 2.0 => 2.0,
        f if f <= 2.5 => 2.5,
        f if f <= 5.0 => 5.0,
        _ => 10.0,
    }
}

/// The time the graphs cover: the shortest of 1, 2, 4, 8, 12 and 24 hours that holds the whole
/// history, so they fill up right after a restart instead of showing a sliver at the right edge.
fn span_for(history_ms: i64) -> i64 {
    [1, 2, 4, 8, 12, 24]
        .into_iter()
        .map(|h| h * 3_600_000)
        .find(|&s| s >= history_ms)
        .unwrap_or(DAY_MS)
}

/// "1 hour", "8 hours".
fn span_words(span_ms: i64) -> String {
    match span_ms / 3_600_000 {
        1 => "hour".to_string(),
        h => format!("{h} hours"),
    }
}

/// An x-axis label `ms` before now: "now", "−45 min", "−6 h".
fn ago(ms: i64) -> String {
    let m = ms / MINUTE_MS;
    match m {
        0 => "now".to_string(),
        m if m % 60 == 0 => format!("−{} h", m / 60),
        m => format!("−{m} min"),
    }
}

/// An SVG line graph of the last `span` ms. A gap of more than three minutes breaks the line.
fn chart(
    title: &str,
    unit: &str,
    now: i64,
    span: i64,
    series: &[Series<'_>],
    from_zero: bool,
) -> String {
    let vals = || {
        series
            .iter()
            .flat_map(|s| s.values.iter().filter_map(|v| v.1))
    };
    let mut out = format!("<section class=\"chart\"><h3>{}", esc(title));
    for s in series {
        let last = s.values.iter().rev().find_map(|v| v.1);
        let _ = write!(
            out,
            " <span class=\"key {}\">{}: {}</span>",
            s.class,
            esc(s.name),
            opt(last, unit)
        );
    }
    out.push_str("</h3>");
    let (Some(lo), Some(hi)) = (vals().reduce(f64::min), vals().reduce(f64::max)) else {
        out.push_str("<p class=\"empty\">No data yet: the first point comes a minute after start.</p></section>");
        return out;
    };
    let (mut lo, mut hi) = (if from_zero { lo.min(0.0) } else { lo }, hi);
    if hi - lo < 1e-9 {
        // a flat line: give it some room
        hi += hi.abs().max(1.0) * 0.5;
        if !(from_zero && lo == 0.0) {
            lo -= lo.abs().max(1.0) * 0.5;
        }
    }
    // round grid values: the axis runs from one to another
    let step = nice_step(hi - lo);
    let (lo, hi) = ((lo / step).floor() * step, (hi / step).ceil() * step);
    let t0 = now - span;
    let x = |t: i64| LEFT + (t - t0) as f64 / span as f64 * (W - LEFT);
    let y = |v: f64| TOP + (hi - v) / (hi - lo) * (H - TOP - BOTTOM);
    let _ = write!(
        out,
        "<svg viewBox=\"0 0 {W} {H}\" role=\"img\" aria-label=\"{}, last {}\">",
        esc(title),
        span_words(span)
    );
    let ticks = ((hi - lo) / step).round() as i64;
    for i in 0..=ticks {
        let v = lo + i as f64 * step;
        let _ = write!(
            out,
            "<line class=\"grid\" x1=\"{LEFT}\" x2=\"{W}\" y1=\"{yy:.1}\" y2=\"{yy:.1}\"/><text class=\"axis\" x=\"{}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>",
            LEFT - 6.0,
            y(v) + 4.5,
            num(v),
            yy = y(v)
        );
    }
    for i in 0..=4 {
        let label = ago(span * (4 - i) / 4);
        let xx = x(t0 + span * i / 4);
        let anchor = match i {
            0 => "start",
            4 => "end",
            _ => "middle",
        };
        let _ = write!(
            out,
            "<text class=\"axis\" x=\"{xx:.1}\" y=\"{}\" text-anchor=\"{anchor}\">{label}</text>",
            H - 4.0
        );
    }
    for s in series {
        // runs of points without a gap; a run of one is drawn as a dot
        let mut runs: Vec<Vec<(f64, f64)>> = Vec::new();
        let mut prev: Option<i64> = None;
        for &(t, v) in &s.values {
            let Some(v) = v.filter(|_| t >= t0) else {
                prev = None;
                continue;
            };
            if !prev.is_some_and(|p| t - p <= 3 * MINUTE_MS) {
                runs.push(Vec::new());
            }
            if let Some(run) = runs.last_mut() {
                run.push((x(t), y(v)));
            }
            prev = Some(t);
        }
        let mut d = String::new();
        for run in &runs {
            if let [(px, py)] = run.as_slice() {
                let _ = write!(
                    out,
                    "<circle class=\"dot {}\" cx=\"{px:.1}\" cy=\"{py:.1}\" r=\"2.5\"/>",
                    s.class
                );
                continue;
            }
            for (i, (px, py)) in run.iter().enumerate() {
                let _ = write!(d, "{}{px:.1} {py:.1}", if i == 0 { 'M' } else { 'L' });
            }
        }
        if !d.is_empty() {
            let _ = write!(out, "<path class=\"line {}\" d=\"{d}\"/>", s.class);
        }
    }
    out.push_str("</svg></section>");
    out
}

const STYLE: &str = r#"
:root { --bg:#f7f7f5; --card:#fff; --ink:#1d1d1f; --muted:#6b6b70; --line:#e3e3e0; --s1:#2563eb; --s2:#d9480f; --ok:#2b8a3e; --warn:#c92a2a; }
@media (prefers-color-scheme: dark) { :root { --bg:#141416; --card:#1e1e21; --ink:#ececee; --muted:#9a9aa0; --line:#323236; --s1:#6ea8fe; --s2:#ff922b; --ok:#69db7c; --warn:#ff8787; } }
* { box-sizing:border-box; }
body { margin:0; background:var(--bg); color:var(--ink); font:15px/1.45 system-ui,-apple-system,"Segoe UI",Roboto,sans-serif; }
main { max-width:1100px; margin:0 auto; padding:20px 16px 40px; }
header { display:flex; flex-wrap:wrap; align-items:baseline; gap:6px 16px; margin-bottom:16px; }
h1 { font-size:22px; margin:0; } h2 { font-size:17px; margin:28px 0 10px; } h3 { font-size:15px; margin:0 0 6px; font-weight:600; }
.sub { color:var(--muted); font-size:13px; }
.tiles { display:grid; grid-template-columns:repeat(auto-fill,minmax(160px,1fr)); gap:10px; }
.tile, .chart, table { background:var(--card); border:1px solid var(--line); border-radius:10px; }
.tile { padding:12px 14px; } .label { color:var(--muted); font-size:13px; } .value { font-size:26px; font-weight:650; font-variant-numeric:tabular-nums; } .note { color:var(--muted); font-size:12px; }
.ok { color:var(--ok); } .bad { color:var(--warn); }
.charts { display:grid; grid-template-columns:repeat(auto-fit,minmax(420px,1fr)); gap:10px; }
@media (max-width:480px) { .charts { grid-template-columns:1fr; } }
.chart { padding:12px 14px 6px; } .chart svg { width:100%; height:auto; display:block; }
.key { font-weight:400; font-size:13px; margin-left:10px; white-space:nowrap; } .key::before { content:""; display:inline-block; width:10px; height:3px; margin-right:5px; vertical-align:middle; background:currentColor; }
.key.s1 { color:var(--s1); } .key.s2 { color:var(--s2); }
.line { fill:none; stroke-width:2; stroke-linejoin:round; } .line.s1 { stroke:var(--s1); } .line.s2 { stroke:var(--s2); }
.grid { stroke:var(--line); stroke-width:1; } .axis { fill:var(--muted); font-size:14px; }
.dot.s1 { fill:var(--s1); } .dot.s2 { fill:var(--s2); }
.empty { color:var(--muted); margin:30px 0; text-align:center; }
.scroll { overflow-x:auto; }
table { width:100%; border-collapse:separate; border-spacing:0; font-size:14px; }
th, td { text-align:left; padding:7px 12px; border-bottom:1px solid var(--line); white-space:nowrap; } th { color:var(--muted); font-weight:500; font-size:13px; } tr:last-child td { border-bottom:0; }
td.num { font-variant-numeric:tabular-nums; }
footer { margin-top:24px; color:var(--muted); font-size:12px; }
"#;

fn render(p: &PageData) -> String {
    let last = p.history.last();
    let day: Vec<&Point> = p.history.iter().filter(|x| x.t >= p.now - DAY_MS).collect();
    let max_range = day.iter().filter_map(|x| x.range_km).reduce(f64::max);
    let dropped_day: u64 = day.iter().map(|x| x.dropped).sum();
    let positions_day: u64 = day.iter().map(|x| x.positions).sum();
    // the history starts at each restart: say so until it covers a whole day
    let period = if p
        .history
        .first()
        .is_some_and(|f| p.now - f.t >= DAY_MS - MINUTE_MS)
    {
        "in 24 h"
    } else {
        "since start"
    };
    let mut h = String::with_capacity(64 * 1024);
    let _ = write!(
        h,
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta http-equiv=\"refresh\" content=\"{REFRESH_S}\"><title>rustadsb status</title>\
         <style>{STYLE}</style></head><body><main><header><h1>rustadsb</h1>\
         <span class=\"sub\">{} · up {} · {} messages since start · refreshes every {REFRESH_S} s</span></header>",
        esc(p.version),
        duration(p.now - p.started),
        p.live.messages_total
    );

    h.push_str("<div class=\"tiles\">");
    h.push_str(&tile(
        "Messages per second",
        &num(p.live.msg_rate),
        &last.map_or(String::new(), |l| {
            format!("{} over the last minute", num(l.msg_rate))
        }),
    ));
    h.push_str(&tile(
        "Aircraft",
        &p.live.aircraft.to_string(),
        &format!("{} with a position", p.live.with_pos),
    ));
    h.push_str(&tile(
        "Furthest aircraft",
        &opt(last.and_then(|l| l.range_km), " km"),
        &format!("last minute · {} {period}", opt(max_range, " km")),
    ));
    h.push_str(&tile(
        "Positions",
        &last.map_or("–".into(), |l| l.positions.to_string()),
        &format!("last minute · {positions_day} {period}"),
    ));
    if p.local {
        let snr = last.and_then(|l| Some(l.signal_db? - l.noise_db?));
        h.push_str(&tile(
            "Signal / noise",
            &format!(
                "{} / {}",
                opt(last.and_then(|l| l.signal_db), ""),
                opt(last.and_then(|l| l.noise_db), "")
            ),
            &format!("dBFS, last minute · {} dB apart", opt(snr, "")),
        ));
        let strong = last.map_or(0, |l| l.strong);
        let share = last
            .filter(|l| l.msg_rate > 0.0)
            .map(|l| strong as f64 * 100.0 / (l.msg_rate * 60.0));
        h.push_str(&tile(
            "Strong signals",
            &strong.to_string(),
            &format!("above −3 dBFS, last minute ({})", opt(share, " %")),
        ));
        let cls = if dropped_day == 0 { "ok" } else { "bad" };
        h.push_str(&tile(
            "Dropped samples",
            &format!("<span class=\"{cls}\">{dropped_day}</span>"),
            &format!("{period} (0 is right)"),
        ));
    }
    h.push_str(&tile(
        "This process",
        &opt(last.and_then(|l| l.cpu_pct), " %"),
        &format!(
            "of one core · {} memory",
            p.live
                .rss_kb
                .map_or("–".into(), |k| format!("{:.1} MB", k as f64 / 1024.0))
        ),
    ));
    let span = span_for(p.history.first().map_or(0, |f| p.now - f.t + MINUTE_MS));
    let _ = write!(
        h,
        "</div><h2>Last {}</h2><div class=\"charts\">",
        span_words(span)
    );

    let series = |f: fn(&Point) -> Option<f64>| -> Vec<(i64, Option<f64>)> {
        p.history.iter().map(|x| (x.t, f(x))).collect()
    };
    h.push_str(&chart(
        "Messages per second",
        "",
        p.now,
        span,
        &[Series {
            name: "messages/s",
            class: "s1",
            values: series(|x| Some(x.msg_rate)),
        }],
        true,
    ));
    h.push_str(&chart(
        "Aircraft",
        "",
        p.now,
        span,
        &[
            Series {
                name: "all",
                class: "s1",
                values: series(|x| Some(x.aircraft as f64)),
            },
            Series {
                name: "with position",
                class: "s2",
                values: series(|x| Some(x.with_pos as f64)),
            },
        ],
        true,
    ));
    h.push_str(&chart(
        "Furthest aircraft",
        " km",
        p.now,
        span,
        &[Series {
            name: "range",
            class: "s1",
            values: series(|x| x.range_km),
        }],
        true,
    ));
    if p.local {
        h.push_str(&chart(
            "Signal and noise",
            " dBFS",
            p.now,
            span,
            &[
                Series {
                    name: "signal",
                    class: "s1",
                    values: series(|x| x.signal_db),
                },
                Series {
                    name: "noise",
                    class: "s2",
                    values: series(|x| x.noise_db),
                },
            ],
            false,
        ));
    }
    h.push_str("</div>");

    let (incoming, outgoing): (Vec<&ClientInfo>, Vec<&ClientInfo>) =
        p.clients.iter().partition(|c| !c.outgoing);
    for (title, list, empty) in [
        (
            "Connected to this decoder",
            &incoming,
            "Nothing is connected to this decoder's ports.",
        ),
        ("Connections this process makes", &outgoing, ""),
    ] {
        if list.is_empty() && empty.is_empty() {
            continue;
        }
        let _ = write!(h, "<h2>{title} ({})</h2>", list.len());
        if list.is_empty() {
            let _ = write!(h, "<p class=\"sub\">{empty}</p>");
            continue;
        }
        let mut rows: Vec<&&ClientInfo> = list.iter().collect();
        rows.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.since_ms.cmp(&b.since_ms)));
        h.push_str("<div class=\"scroll\"><table><tr><th>Port / protocol</th><th>Other end</th><th>Connected for</th></tr>");
        for c in rows {
            let _ = write!(
                h,
                "<tr><td>{}</td><td>{}</td><td class=\"num\">{}</td></tr>",
                esc(&c.kind),
                esc(&c.peer),
                duration(p.now - c.since_ms)
            );
        }
        h.push_str("</table></div>");
    }
    let _ = write!(
        h,
        "<footer>Graphs: one point a minute, kept for 24 hours in memory (they start again when the \
         decoder restarts). Read-only page; turn it off by removing --dashboard-port.</footer></main></body></html>"
    );
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustadsb_core::demod::DemodStats;
    use rustadsb_core::track::TrackStats;

    fn bucket(start: i64, end: i64, messages: u64) -> StatsBlock {
        StatsBlock {
            start,
            end,
            samples_processed: 0,
            samples_lost: 0,
            samples_dropped: 0,
            demod: DemodStats {
                signal_power_sum: 0.1,
                signal_power_count: 10,
                noise_power_sum: 0.001,
                noise_power_count: 10,
                ..DemodStats::default()
            },
            track: TrackStats {
                messages_total: messages,
                distance_max: (start / 1000) as u64,
                ..TrackStats::default()
            },
        }
    }

    #[test]
    fn six_buckets_make_a_minute() {
        let mut m = Minute::default();
        for i in 0..6 {
            m.add(&bucket(i * 10_000, (i + 1) * 10_000, 600));
        }
        let p = m.point(3, 2, Some(40.0));
        assert_eq!(p.t, 60_000);
        assert!((p.msg_rate - 60.0).abs() < 1e-9, "3600 messages in 60 s");
        // 0.1 / 10 = 0.01 -> -20 dBFS; 0.001 / 10 -> -40 dBFS
        assert!((p.signal_db.unwrap() + 20.0).abs() < 1e-9);
        assert!((p.noise_db.unwrap() + 40.0).abs() < 1e-9);
        assert_eq!(
            p.range_km,
            Some(0.05),
            "the largest of the buckets' furthest positions"
        );
        assert_eq!((p.aircraft, p.with_pos, p.cpu_pct), (3, 2, Some(40.0)));
    }

    #[test]
    fn no_signal_without_a_local_receiver() {
        let mut m = Minute::default();
        let mut b = bucket(0, 60_000, 10);
        b.demod = DemodStats::default();
        b.track.distance_max = 0;
        m.add(&b);
        let p = m.point(0, 0, None);
        assert_eq!((p.signal_db, p.noise_db, p.range_km), (None, None, None));
    }

    #[test]
    fn a_gap_breaks_the_line() {
        let now = DAY_MS * 10;
        let s = Series {
            name: "x",
            class: "s1",
            values: vec![
                (now - 20 * MINUTE_MS, Some(1.0)),
                (now - 19 * MINUTE_MS, Some(2.0)),
                // ten minutes without data, then a lone point, then a missing value
                (now - 9 * MINUTE_MS, Some(3.0)),
                (now - 8 * MINUTE_MS, None),
                (now - 7 * MINUTE_MS, Some(4.0)),
                (now - 6 * MINUTE_MS, Some(4.0)),
            ],
        };
        let svg = chart("t", "", now, DAY_MS, &[s], true);
        let path = svg
            .split(" d=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert_eq!(path.matches('M').count(), 2, "{path}");
        assert_eq!(path.matches('L').count(), 2, "{path}");
        assert_eq!(
            svg.matches("<circle").count(),
            1,
            "the lone point is a dot: {svg}"
        );
    }

    #[test]
    fn grid_values_are_round() {
        assert_eq!(nice_step(270.0), 100.0);
        assert_eq!(nice_step(21.0), 10.0);
        assert_eq!(nice_step(9.0), 2.5);
        assert_eq!(nice_step(1.0), 0.25);
        let now = DAY_MS;
        let svg = chart(
            "t",
            "",
            now,
            DAY_MS,
            &[Series {
                name: "x",
                class: "s1",
                values: vec![(now, Some(243.0))],
            }],
            true,
        );
        for label in [">0<", ">100<", ">200<", ">300<"] {
            assert!(svg.contains(label), "{label} in {svg}");
        }
    }

    #[test]
    fn empty_and_flat_graphs_are_drawn() {
        let now = DAY_MS;
        let empty = chart(
            "t",
            "",
            now,
            DAY_MS,
            &[Series {
                name: "x",
                class: "s1",
                values: vec![],
            }],
            true,
        );
        assert!(empty.contains("No data yet"));
        let flat = chart(
            "t",
            "",
            now,
            DAY_MS,
            &[Series {
                name: "x",
                class: "s1",
                values: vec![(now, Some(5.0)), (now - MINUTE_MS, Some(5.0))],
            }],
            false,
        );
        assert!(flat.contains("<path") && !flat.contains("NaN"), "{flat}");
    }

    #[test]
    fn requests() {
        let get =
            |r: &str| String::from_utf8(respond(r.as_bytes(), || "<p>page</p>".into())).unwrap();
        let ok = get("GET / HTTP/1.1\r\nHost: pi\r\n\r\n");
        assert!(
            ok.starts_with("HTTP/1.1 200 OK\r\n") && ok.ends_with("<p>page</p>"),
            "{ok}"
        );
        assert!(ok.contains("Content-Security-Policy: default-src 'none'"));
        assert!(get("GET /index.html?x=1 HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200"));
        let head = get("HEAD / HTTP/1.1\r\n\r\n");
        assert!(
            head.starts_with("HTTP/1.1 200") && head.ends_with("\r\n\r\n"),
            "no body: {head}"
        );
        assert!(get("GET /etc/passwd HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 404"));
        assert!(get("POST / HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 405"));
    }

    #[test]
    fn the_page_escapes_what_it_shows() {
        let page = render(&PageData {
            version: "rustadsb <test>",
            started: 0,
            local: true,
            now: 120_000,
            live: Live::default(),
            history: vec![Point {
                t: 60_000,
                msg_rate: 12.0,
                ..Point::default()
            }],
            clients: vec![ClientInfo {
                kind: "beast out :30005".into(),
                peer: "<script>".into(),
                outgoing: false,
                since_ms: 0,
            }],
        });
        assert!(page.contains("&lt;script&gt;") && !page.contains("<script"));
        assert!(page.contains("rustadsb &lt;test&gt;"));
        assert!(page.contains("Connected to this decoder (1)"));
        assert!(page.contains("Signal and noise"));
    }

    #[test]
    fn the_time_axis_grows_with_the_history() {
        let h = 3_600_000;
        assert_eq!(span_for(0), h);
        assert_eq!(span_for(5 * MINUTE_MS), h);
        assert_eq!(span_for(h + 1), 2 * h);
        assert_eq!(span_for(9 * h), 12 * h);
        assert_eq!(span_for(30 * h), DAY_MS);
        assert_eq!(
            (0..=4).map(|i| ago(h * (4 - i) / 4)).collect::<Vec<_>>(),
            ["−1 h", "−45 min", "−30 min", "−15 min", "now"]
        );
        assert_eq!(ago(DAY_MS / 4 * 3), "−18 h");
        assert_eq!(span_words(h), "hour");
        assert_eq!(span_words(8 * h), "8 hours");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(45_000), "45 s");
        assert_eq!(duration(12 * 60_000), "12 min");
        assert_eq!(duration((5 * 60 + 12) * 60_000), "5 h 12 min");
        assert_eq!(duration((3 * 24 + 4) * 3_600_000), "3 d 4 h");
    }
}
