//! TCP networking: output servers (raw, Beast, SBS), input servers (raw, Beast) and
//! outbound connectors (`--net-connector`). Everything here is async (tokio); the decoder
//! engine runs on its own thread and talks to this module through a [`Hub`] (outgoing) and a
//! channel of [`WireItem`]s (incoming).

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener as StdListener};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use openradome_core::beast::{BeastFrame, BeastParser, parse_raw_line};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Handle;
use tokio::sync::broadcast;

pub type Chunk = Arc<Vec<u8>>;

/// A connection that is open right now: a client on one of our ports, or one of our connectors.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientInfo {
    /// What it is, such as `beast out :30005` or `beast_reduce_plus_out`.
    pub kind: String,
    /// The other end (`address:port`, or `host:port` for a connector).
    pub peer: String,
    /// We connected out (a `--net-connector`), rather than it connecting to us.
    pub outgoing: bool,
    /// When it connected, in ms since the Unix epoch.
    pub since_ms: i64,
}

static CLIENTS: LazyLock<Mutex<BTreeMap<u64, ClientInfo>>> = LazyLock::new(Default::default);
static NEXT_CLIENT: AtomicU64 = AtomicU64::new(0);

/// Keeps a connection in [`clients`] for as long as it lives.
pub struct ClientGuard(u64);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        CLIENTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

fn register_client(kind: String, peer: String, outgoing: bool) -> ClientGuard {
    let id = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
    let info = ClientInfo {
        kind,
        peer,
        outgoing,
        since_ms: now_ms(),
    };
    CLIENTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, info);
    ClientGuard(id)
}

/// The connections open right now, oldest first.
pub fn clients() -> Vec<ClientInfo> {
    CLIENTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect()
}

/// At most this many clients on the data ports at once, all ports together. Each holds a file
/// descriptor, and running out (the usual limit is 1024 a process) would stop every listener and
/// every file the decoder writes.
const MAX_CLIENTS: usize = 512;
/// At most this many from one address other than this computer's own, so one host can't take every
/// slot. Feed and MLAT clients on this computer all come from 127.0.0.1, so loopback has no limit
/// of its own.
const MAX_CLIENTS_PER_HOST: usize = 32;
/// After a failed accept (out of file descriptors, say) wait this long: retrying at once would
/// spin a core and starve everything else on the runtime.
const ACCEPT_RETRY: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Slots {
    total: usize,
    by_host: HashMap<IpAddr, usize>,
}

/// A cap on open connections: so many in all, and so many from one address other than this
/// computer's own (loopback has only the overall cap: local feed and MLAT clients share it).
pub struct ClientLimit {
    max: usize,
    per_host: usize,
    slots: Mutex<Slots>,
}

impl ClientLimit {
    pub fn new(max: usize, per_host: usize) -> Arc<Self> {
        Arc::new(Self {
            max,
            per_host,
            slots: Mutex::default(),
        })
    }

    /// A place for a client from `ip`, if there is one; given back when the slot is dropped.
    pub fn take(self: &Arc<Self>, ip: IpAddr) -> Option<ClientSlot> {
        let ip = ip.to_canonical();
        let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let from_host = s.by_host.get(&ip).copied().unwrap_or(0);
        if s.total >= self.max || (!ip.is_loopback() && from_host >= self.per_host) {
            return None;
        }
        s.total += 1;
        *s.by_host.entry(ip).or_default() += 1;
        Some(ClientSlot {
            limit: self.clone(),
            ip,
        })
    }
}

pub struct ClientSlot {
    limit: Arc<ClientLimit>,
    ip: IpAddr,
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        let mut s = self.limit.slots.lock().unwrap_or_else(|e| e.into_inner());
        s.total = s.total.saturating_sub(1);
        if let Some(n) = s.by_host.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                s.by_host.remove(&self.ip);
            }
        }
    }
}

static DATA_CLIENTS: LazyLock<Arc<ClientLimit>> =
    LazyLock::new(|| ClientLimit::new(MAX_CLIENTS, MAX_CLIENTS_PER_HOST));
static NEXT_ACCEPT_LOG: AtomicI64 = AtomicI64::new(0);

/// Say so, but at most once a minute: a flood must not fill the log too.
fn log_accept_problem(msg: impl FnOnce() -> String) {
    let now = now_ms();
    let next = NEXT_ACCEPT_LOG.load(Ordering::Relaxed);
    if now >= next
        && NEXT_ACCEPT_LOG
            .compare_exchange(next, now + 60_000, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        eprintln!("{}", msg());
    }
}

/// The next client within the limits. Clients over them are closed at once.
async fn accept_client(listener: &TcpListener, port: u16) -> (TcpStream, SocketAddr, ClientSlot) {
    loop {
        match listener.accept().await {
            Ok((sock, peer)) => match DATA_CLIENTS.take(peer.ip()) {
                Some(slot) => return (sock, peer, slot),
                None => log_accept_problem(|| {
                    format!(
                        "net: port {port}: too many clients ({MAX_CLIENTS} in all, {MAX_CLIENTS_PER_HOST} from one host); refusing {peer} and others"
                    )
                }),
            },
            Err(e) => {
                log_accept_problem(|| format!("net: port {port}: accept failed: {e}"));
                tokio::time::sleep(ACCEPT_RETRY).await;
            }
        }
    }
}

pub const BEAST_HEARTBEAT: [u8; 11] = [0x1a, b'1', 0, 0, 0, 0, 0, 0, 0, 0, 0];
pub const RAW_HEARTBEAT: &[u8] = b"*0000;\n";
pub const SBS_HEARTBEAT: &[u8] = b"\r\n";

/// Outgoing data, one broadcast channel per output format.
#[derive(Clone)]
pub struct Hub {
    raw: broadcast::Sender<Chunk>,
    beast: broadcast::Sender<Chunk>,
    sbs: broadcast::Sender<Chunk>,
    /// Beast frames of messages the tracker flagged as worth forwarding (`beast_reduce`).
    beast_reduce: broadcast::Sender<Chunk>,
    /// Output waiting for the next `flush`, so clients get one write per batch rather than one
    /// per message (upstream flushes about every 50 ms, or when this many bytes are waiting).
    pending: Arc<Mutex<[Vec<u8>; 4]>>,
    flush_size: usize,
}

/// Output flush size (`--net-ro-size`): a message that would push the waiting data past this
/// many bytes is sent after what is waiting, and a single message this large goes out at once.
/// Upstream's default and its minimum; like upstream there is no maximum.
pub const FLUSH_SIZE: usize = 1280;
pub const MIN_FLUSH_SIZE: usize = 750;

/// Data we had to throw away: output skipped for clients that can't keep up (upstream drops
/// such data too), and input the decoder had no room for. Counted (see [`output_skipped`] and
/// [`input_dropped`]), and logged at most once a minute each, so that a loss is visible but
/// can't flood the log.
static OUTPUT_SKIPPED: AtomicU64 = AtomicU64::new(0);
static INPUT_DROPPED: AtomicU64 = AtomicU64::new(0);
/// Monotonic milliseconds at which each may next be logged.
static OUTPUT_NEXT_LOG: AtomicU64 = AtomicU64::new(0);
static INPUT_NEXT_LOG: AtomicU64 = AtomicU64::new(0);
static CLOCK: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Output chunks skipped so far for clients that could not keep up.
pub fn output_skipped() -> u64 {
    OUTPUT_SKIPPED.load(Ordering::Relaxed)
}

/// Network messages dropped so far because the decoder's input queue was full.
pub fn input_dropped() -> u64 {
    INPUT_DROPPED.load(Ordering::Relaxed)
}

/// Count a loss and, at most once a minute, log it. `what` is only built when it is logged.
fn note_loss(total: &AtomicU64, next_log: &AtomicU64, n: u64, what: impl FnOnce() -> String) {
    let sum = total.fetch_add(n, Ordering::Relaxed) + n;
    // Monotonic time: a stepped system clock can neither silence nor flood the log.
    let now = CLOCK.elapsed().as_millis() as u64;
    let due = next_log.load(Ordering::Relaxed);
    if now >= due
        && next_log
            .compare_exchange(due, now + 60_000, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        eprintln!("net: {} ({sum} in total)", what());
    }
}
const RAW: usize = 0;
const BEAST: usize = 1;
const SBS: usize = 2;
const BEAST_REDUCE: usize = 3;

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    pub fn new() -> Self {
        let mk = || broadcast::channel::<Chunk>(8192).0;
        Self {
            raw: mk(),
            beast: mk(),
            sbs: mk(),
            beast_reduce: mk(),
            pending: Arc::new(Mutex::new([Vec::new(), Vec::new(), Vec::new(), Vec::new()])),
            flush_size: FLUSH_SIZE,
        }
    }

    /// A hub that flushes once `size` bytes are waiting (`--net-ro-size`, at least 750).
    pub fn with_flush_size(size: usize) -> Self {
        Self {
            flush_size: size.max(MIN_FLUSH_SIZE),
            ..Self::new()
        }
    }

    fn channel(&self, which: usize) -> &broadcast::Sender<Chunk> {
        match which {
            RAW => &self.raw,
            BEAST => &self.beast,
            SBS => &self.sbs,
            _ => &self.beast_reduce,
        }
    }

    fn queue(&self, which: usize, data: &[u8]) {
        let full = {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let buf = &mut p[which];
            // send what is waiting first if this message would push it past the size limit
            let early = (!buf.is_empty() && buf.len() + data.len() > self.flush_size)
                .then(|| std::mem::take(buf));
            buf.extend_from_slice(data);
            // a message that is itself that large goes out at once (upstream does both)
            let big = (buf.len() >= self.flush_size).then(|| std::mem::take(buf));
            [early, big]
        };
        for chunk in full.into_iter().flatten() {
            let _ = self.channel(which).send(Arc::new(chunk));
        }
    }

    /// Send everything queued since the last call, one chunk per format.
    pub fn flush(&self) {
        let chunks: [Vec<u8>; 4] = {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            [
                std::mem::take(&mut p[RAW]),
                std::mem::take(&mut p[BEAST]),
                std::mem::take(&mut p[SBS]),
                std::mem::take(&mut p[BEAST_REDUCE]),
            ]
        };
        for (which, chunk) in chunks.into_iter().enumerate() {
            if !chunk.is_empty() {
                let _ = self.channel(which).send(Arc::new(chunk));
            }
        }
    }

    pub fn raw_clients(&self) -> usize {
        self.raw.receiver_count()
    }

    pub fn beast_clients(&self) -> usize {
        self.beast.receiver_count()
    }

    pub fn sbs_clients(&self) -> usize {
        self.sbs.receiver_count()
    }

    pub fn beast_reduce_clients(&self) -> usize {
        self.beast_reduce.receiver_count()
    }

    pub fn send_raw(&self, data: Vec<u8>) {
        self.queue(RAW, &data);
    }

    pub fn send_beast(&self, data: Vec<u8>) {
        self.queue(BEAST, &data);
    }

    pub fn send_sbs(&self, data: Vec<u8>) {
        self.queue(SBS, &data);
    }

    pub fn send_beast_reduce(&self, data: Vec<u8>) {
        self.queue(BEAST_REDUCE, &data);
    }
}

/// A message received from the network, not yet decoded.
#[derive(Debug, Clone)]
pub struct WireItem {
    pub data: Vec<u8>,
    pub timestamp: i64,
    pub signal: Option<u8>,
    pub receiver_id: u64,
    /// Receive time, ms since the epoch.
    pub now: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    BeastIn,
    BeastOut,
    /// Beast frames of messages the tracker flagged as news (`--net-beast-reduce-interval`).
    BeastReduceOut,
    /// `BeastReduceOut` that first identifies the station by UUID and answers pings.
    BeastReducePlusOut,
    RawIn,
    RawOut,
    SbsOut,
    /// UAT (978 MHz) input: accepted for command-line compatibility, not supported.
    UatIn,
}

impl Proto {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "beast_in" => Self::BeastIn,
            "beast_out" => Self::BeastOut,
            "beast_reduce_out" => Self::BeastReduceOut,
            "beast_reduce_plus_out" => Self::BeastReducePlusOut,
            "raw_in" => Self::RawIn,
            "raw_out" => Self::RawOut,
            "sbs_out" => Self::SbsOut,
            "uat_in" => Self::UatIn,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::BeastIn => "beast_in",
            Self::BeastOut => "beast_out",
            Self::BeastReduceOut => "beast_reduce_out",
            Self::BeastReducePlusOut => "beast_reduce_plus_out",
            Self::RawIn => "raw_in",
            Self::RawOut => "raw_out",
            Self::SbsOut => "sbs_out",
            Self::UatIn => "uat_in",
        }
    }
}

/// A `--net-connector IP,PORT,PROTOCOL[,IP2,PORT2][,silent_fail][,uuid=UUID]` entry, as
/// upstream parses it: positional host, port, protocol and an optional fallback host and port;
/// the named options may appear anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connector {
    pub host: String,
    pub port: u16,
    pub proto: Proto,
    /// Tried alternately with the primary when connecting fails.
    pub fallback: Option<(String, u16)>,
    /// Don't log connection failures.
    pub silent_fail: bool,
    /// UUID for this connection only (`beast_reduce_plus_out`), overriding `--uuid`.
    pub uuid: Option<String>,
}

impl Connector {
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut positional: Vec<&str> = Vec::new();
        let mut silent_fail = false;
        let mut uuid = None;
        for token in s.split(',').map(str::trim) {
            if token == "silent_fail" {
                silent_fail = true;
            } else if let Some(u) = token.strip_prefix("uuid=") {
                uuid = Some(u.to_string());
            } else {
                positional.push(token);
            }
        }
        if positional.len() < 3 {
            return Err(format!("net-connector needs IP,PORT,PROTOCOL, got '{s}'"));
        }
        let port = |p: &str| {
            p.parse::<u16>()
                .map_err(|_| format!("bad port '{p}' in net-connector '{s}'"))
        };
        let proto = Proto::parse(positional[2]).ok_or_else(|| {
            format!(
                "unsupported net-connector protocol '{}' (supported: beast_in, beast_out, beast_reduce_out, beast_reduce_plus_out, raw_in, raw_out, sbs_out)",
                positional[2]
            )
        })?;
        let fallback = match positional.get(3..5) {
            Some([h, p]) => Some(((*h).to_string(), port(p)?)),
            _ => None,
        };
        Ok(Self {
            host: positional[0].to_string(),
            port: port(positional[1])?,
            proto,
            fallback,
            silent_fail,
            uuid,
        })
    }
}

/// The frames a `beast_reduce_plus_out` connection starts with: the station UUID
/// (`0x1a 0xe4` and 36 characters, padded with `f`) and the request for high-resolution
/// pings (`0x1a W O`). `None` if the UUID is too short to be one.
pub fn uuid_preamble(uuid: &str) -> Option<Vec<u8>> {
    let uuid = uuid.trim();
    if uuid.len() < 28 {
        return None;
    }
    let mut f = vec![0x1a, 0xe4];
    let mut id: Vec<u8> = uuid.bytes().take(36).collect();
    id.resize(36, b'f');
    f.extend_from_slice(&id);
    f.extend_from_slice(&[0x1a, b'W', b'O']);
    Some(f)
}

/// Parse a port list such as `30005` or `30005,30105`; `0` means off.
pub fn parse_ports(s: &str) -> Result<Vec<u16>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "0")
        .map(|p| p.parse::<u16>().map_err(|_| format!("bad port '{p}'")))
        .collect()
}

#[derive(Debug, Clone)]
pub struct NetConfig {
    pub bind: IpAddr,
    pub raw_out: Vec<u16>,
    pub beast_out: Vec<u16>,
    pub sbs_out: Vec<u16>,
    pub raw_in: Vec<u16>,
    pub beast_in: Vec<u16>,
    pub beast_reduce_out: Vec<u16>,
    pub connectors: Vec<Connector>,
    /// Station UUID for `beast_reduce_plus_out` (`--uuid` / `--uuid-file`).
    pub uuid: Option<String>,
    /// Heartbeat interval; zero disables.
    pub heartbeat: Duration,
    pub connector_delay: Duration,
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let std = StdListener::bind(addr)?;
    std.set_nonblocking(true)?;
    TcpListener::from_std(std)
}

/// Bind all listeners (failing early if a port is taken) and start serving.
pub fn spawn(rt: &Handle, cfg: &NetConfig, hub: &Hub, tx: &SyncSender<WireItem>) -> io::Result<()> {
    let _guard = rt.enter();
    let outputs = [
        (
            "raw out",
            &cfg.raw_out,
            hub.raw.clone(),
            RAW_HEARTBEAT.to_vec(),
            false,
        ),
        (
            "beast out",
            &cfg.beast_out,
            hub.beast.clone(),
            BEAST_HEARTBEAT.to_vec(),
            true,
        ),
        (
            "sbs out",
            &cfg.sbs_out,
            hub.sbs.clone(),
            SBS_HEARTBEAT.to_vec(),
            false,
        ),
        (
            "beast_reduce out",
            &cfg.beast_reduce_out,
            hub.beast_reduce.clone(),
            BEAST_HEARTBEAT.to_vec(),
            true,
        ),
    ];
    for (name, ports, sender, heartbeat, beast) in outputs {
        for &port in ports {
            let listener = bind(SocketAddr::new(cfg.bind, port)).map_err(|e| {
                io::Error::new(e.kind(), format!("can't listen on port {port}: {e}"))
            })?;
            let (sender, heartbeat, interval) = (sender.clone(), heartbeat.clone(), cfg.heartbeat);
            tokio::spawn(async move {
                loop {
                    let (sock, peer, slot) = accept_client(&listener, port).await;
                    set_sockopts(&sock);
                    let guard = register_client(format!("{name} :{port}"), peer.to_string(), false);
                    let served = serve_output(
                        sock,
                        sender.subscribe(),
                        heartbeat.clone(),
                        interval,
                        beast,
                        None,
                        peer.to_string(),
                    );
                    tokio::spawn(async move {
                        let _guard = (guard, slot);
                        served.await
                    });
                }
            });
        }
    }
    let inputs = [
        ("raw in", &cfg.raw_in, false),
        ("beast in", &cfg.beast_in, true),
    ];
    for (name, ports, beast) in inputs {
        for &port in ports {
            let listener = bind(SocketAddr::new(cfg.bind, port)).map_err(|e| {
                io::Error::new(e.kind(), format!("can't listen on port {port}: {e}"))
            })?;
            let tx = tx.clone();
            tokio::spawn(async move {
                loop {
                    let (sock, peer, slot) = accept_client(&listener, port).await;
                    set_sockopts(&sock);
                    let guard = register_client(format!("{name} :{port}"), peer.to_string(), false);
                    let served = serve_input(sock, beast, tx.clone(), None);
                    tokio::spawn(async move {
                        let _guard = (guard, slot);
                        served.await
                    });
                }
            });
        }
    }
    for c in &cfg.connectors {
        if c.proto == Proto::UatIn {
            // feeder installers add one for a local dump978; UAT is a US-only system
            eprintln!(
                "net: {}:{}: uat_in (978 MHz UAT) is not supported; connector ignored",
                c.host, c.port
            );
            continue;
        }
        let (c, hub, tx, interval, delay) = (
            c.clone(),
            hub.clone(),
            tx.clone(),
            cfg.heartbeat,
            cfg.connector_delay,
        );
        let preamble = matches!(c.proto, Proto::BeastReducePlusOut).then(|| {
            let id = c.uuid.as_deref().or(cfg.uuid.as_deref()).unwrap_or("");
            uuid_preamble(id).unwrap_or_else(|| {
                eprintln!(
                    "net: {}:{}: no valid UUID for beast_reduce_plus_out (give --uuid-file or uuid=); sending none",
                    c.host, c.port
                );
                Vec::new()
            })
        });
        tokio::spawn(async move {
            let mut use_fallback = false;
            let mut failures: u32 = 0;
            let mut short_lived: u32 = 0;
            loop {
                let (host, port) = match (&c.fallback, use_fallback) {
                    (Some((h, p)), true) => (h.as_str(), *p),
                    _ => (c.host.as_str(), c.port),
                };
                let connected = connect_with_timeout(host, port, connect_timeout(delay)).await;
                if c.fallback.is_some() {
                    use_fallback = !use_fallback;
                }
                let sock = match connected {
                    Ok(s) => s,
                    Err(e) => {
                        failures += 1;
                        // like upstream: say so the first few times, then only now and then
                        if !c.silent_fail && (failures <= 3 || failures.is_multiple_of(200)) {
                            eprintln!("net: {host}:{port}: connection failed ({failures}x): {e}");
                        }
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                };
                if failures > 0 && !c.silent_fail {
                    eprintln!("net: {host}:{port}: connected");
                }
                failures = 0;
                let label = format!("{host}:{port}");
                // upstream: a server that sends neither data nor heartbeats for two heartbeat
                // intervals is reconnected to (none with --net-heartbeat 0)
                let idle = (!interval.is_zero()).then(|| Idle {
                    limit: 2 * interval,
                    label: label.clone(),
                });
                let connected_at = Instant::now();
                set_sockopts(&sock);
                let guard = register_client(c.proto.name().to_string(), label.clone(), true);
                match c.proto {
                    Proto::BeastIn => serve_input(sock, true, tx.clone(), idle).await,
                    Proto::RawIn => serve_input(sock, false, tx.clone(), idle).await,
                    Proto::BeastOut => {
                        serve_output(
                            sock,
                            hub.beast.subscribe(),
                            BEAST_HEARTBEAT.to_vec(),
                            interval,
                            true,
                            None,
                            label.clone(),
                        )
                        .await
                    }
                    Proto::BeastReduceOut | Proto::BeastReducePlusOut => {
                        serve_output(
                            sock,
                            hub.beast_reduce.subscribe(),
                            BEAST_HEARTBEAT.to_vec(),
                            interval,
                            true,
                            preamble.clone(),
                            label.clone(),
                        )
                        .await
                    }
                    Proto::RawOut => {
                        serve_output(
                            sock,
                            hub.raw.subscribe(),
                            RAW_HEARTBEAT.to_vec(),
                            interval,
                            false,
                            None,
                            label.clone(),
                        )
                        .await
                    }
                    Proto::SbsOut => {
                        serve_output(
                            sock,
                            hub.sbs.subscribe(),
                            SBS_HEARTBEAT.to_vec(),
                            interval,
                            false,
                            None,
                            label.clone(),
                        )
                        .await
                    }
                    Proto::UatIn => unreachable!("uat_in connectors are skipped above"),
                }
                drop(guard);
                // Say so when the other side hangs up: a network that keeps refusing the station
                // (ADSBHub does until it is registered) should be visible. Short-lived
                // connections in a row are logged the first few times, then only now and then.
                if connected_at.elapsed() < Duration::from_secs(60) {
                    short_lived += 1;
                } else {
                    short_lived = 1;
                }
                if !c.silent_fail && (short_lived <= 3 || short_lived.is_multiple_of(200)) {
                    eprintln!(
                        "net: {host}:{port}: disconnected after {:.0} s",
                        connected_at.elapsed().as_secs_f64()
                    );
                }
                tokio::time::sleep(delay).await;
            }
        });
    }
    Ok(())
}

/// How long one connection attempt may take, as upstream: the reconnect delay, but at most 5 s
/// ("if your connection won't establish after 5 seconds ... tough luck"). Without it a host
/// that never answers (a dead fallback port) holds the connector for the kernel's timeout,
/// about two minutes, instead of moving on to the other address.
fn connect_timeout(delay: Duration) -> Duration {
    delay.min(Duration::from_secs(5))
}

async fn connect_with_timeout(host: &str, port: u16, limit: Duration) -> io::Result<TcpStream> {
    match tokio::time::timeout(limit, TcpStream::connect((host, port))).await {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("no answer within {} s", limit.as_secs_f64()),
        )),
    }
}

/// Upstream's socket options for every connection, in and out: no Nagle delay, and TCP
/// keepalive (probe after 20 s idle, every 2 s, give up after 3), so a peer that vanished
/// without closing is noticed in about 26 s rather than never (a reader) or hours.
fn set_sockopts(sock: &TcpStream) {
    let _ = sock.set_nodelay(true);
    let ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(20))
        .with_interval(Duration::from_secs(2))
        .with_retries(3);
    if let Err(e) = socket2::SockRef::from(sock).set_tcp_keepalive(&ka) {
        eprintln!("net: unable to set keepalive: {e}");
    }
}

/// As upstream: a connection to which nothing could be written for 5 s is dropped (a server
/// that stopped reading or acknowledging), but not in its first 15 s.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const SEND_GRACE: Duration = Duration::from_secs(15);

/// Write all of `buf`, failing with `TimedOut` once no byte could be written for `timeout`
/// (or until `grace` after `connected`, whichever is later). Without this a stalled peer
/// holds the connection until the kernel gives up on it, about 15 minutes, and gets nothing
/// meanwhile; upstream reconnects, and the server usually takes the new connection at once.
async fn send(
    sock: &mut TcpStream,
    mut buf: &[u8],
    connected: Instant,
    timeout: Duration,
    grace: Duration,
) -> io::Result<()> {
    while !buf.is_empty() {
        let limit = timeout.max(grace.saturating_sub(connected.elapsed()));
        match tokio::time::timeout(limit, sock.write(buf)).await {
            Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(Ok(n)) => buf = &buf[n..],
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "couldn't send any data for {:.0} s (insufficient bandwidth?): disconnecting",
                        limit.as_secs_f64()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// One output connection: every write goes through `send`, so none can skip the stall rule.
struct Out {
    sock: TcpStream,
    connected: Instant,
    /// How the log names the peer: the configured `host:port` of a connector (as in its
    /// other lines), the address of a client that connected to us.
    label: String,
}

impl Out {
    /// Write `buf`; false when the connection is done. A stall is reported (it is the reason
    /// for the reconnect that follows), as upstream does whatever `silent_fail` says; other
    /// errors are the peer going away, which the connector reports.
    async fn write(&mut self, buf: &[u8]) -> bool {
        match send(
            &mut self.sock,
            buf,
            self.connected,
            SEND_TIMEOUT,
            SEND_GRACE,
        )
        .await
        {
            Ok(()) => true,
            Err(e) => {
                if e.kind() == io::ErrorKind::TimedOut {
                    eprintln!("net: {}: {e}", self.label);
                }
                false
            }
        }
    }
}

/// Pings an aggregator sends on a `beast_reduce_plus` connection (`0x1a P` and a 24-bit
/// value, `0x1a` doubled); we answer each write with the value plus the time it waited.
#[derive(Default)]
struct Pinger {
    buf: Vec<u8>,
    ping: Option<(u32, Instant)>,
}

impl Pinger {
    fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        let mut i = 0;
        let mut consumed = 0;
        while i + 1 < self.buf.len() {
            if self.buf[i] != 0x1a {
                i += 1;
                consumed = i;
                continue;
            }
            if self.buf[i + 1] != b'P' {
                // some other frame (or an escaped 0x1a): skip it
                i += 2;
                consumed = i;
                continue;
            }
            // three payload bytes, each 0x1a doubled
            let mut v = 0u32;
            let mut j = i + 2;
            let mut ok = true;
            for _ in 0..3 {
                let Some(&b) = self.buf.get(j) else {
                    ok = false;
                    break;
                };
                v = (v << 8) | u32::from(b);
                j += if b == 0x1a && self.buf.get(j + 1) == Some(&0x1a) {
                    2
                } else {
                    1
                };
            }
            if !ok {
                break; // wait for the rest
            }
            self.ping = Some((v & 0xff_ffff, Instant::now()));
            i = j;
            consumed = i;
        }
        self.buf.drain(..consumed);
        if self.buf.len() > 64 {
            self.buf.clear();
        }
    }

    /// The pong to send with the next write, if a ping was received.
    fn pong(&self) -> Option<Vec<u8>> {
        let (ping, at) = self.ping?;
        let v = ping.wrapping_add(at.elapsed().as_millis() as u32) & 0xff_ffff;
        let mut f = vec![0x1a, b'P'];
        for b in [(v >> 16) as u8, (v >> 8) as u8, v as u8] {
            f.push(b);
            if b == 0x1a {
                f.push(0x1a);
            }
        }
        Some(f)
    }
}

/// Stream broadcast chunks to one client until it goes away. `preamble` (the UUID frames of
/// `beast_reduce_plus`) is sent first, and then the connection answers the aggregator's pings.
async fn serve_output(
    sock: TcpStream,
    mut rx: broadcast::Receiver<Chunk>,
    heartbeat: Vec<u8>,
    interval: Duration,
    beast: bool,
    preamble: Option<Vec<u8>>,
    label: String,
) {
    let mut out = Out {
        sock,
        connected: Instant::now(),
        label,
    };
    let mut pinger = preamble.as_ref().map(|_| Pinger::default());
    if let Some(p) = &preamble
        && !p.is_empty()
        && !out.write(p).await
    {
        return;
    }
    // Beast clients get a few heartbeats up front, to tell them we are a client that
    // can take feedback.
    if beast {
        for _ in 0..5 {
            if !out.write(&heartbeat).await {
                return;
            }
        }
    }
    let period = if interval.is_zero() {
        Duration::from_secs(3600)
    } else {
        interval
    };
    let mut timer = tokio::time::interval(period);
    timer.tick().await;
    let mut last_write = Instant::now();
    let mut scratch = [0u8; 512];
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(chunk) => {
                    if let Some(pong) = pinger.as_ref().and_then(Pinger::pong)
                        && !out.write(&pong).await
                    {
                        return;
                    }
                    if !out.write(&chunk).await {
                        return;
                    }
                    last_write = Instant::now();
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    note_loss(&OUTPUT_SKIPPED, &OUTPUT_NEXT_LOG, n, || {
                        format!("output client {} too slow, skipped {n} chunks", out.label)
                    });
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            // what clients send is ignored (Beast option commands), except pings on a
            // beast_reduce_plus connection; a close is noticed
            r = out.sock.read(&mut scratch) => match r {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if let Some(p) = pinger.as_mut() {
                        p.feed(&scratch[..n]);
                    }
                }
            },
            _ = timer.tick() => {
                if !interval.is_zero() && last_write.elapsed() >= interval {
                    if !out.write(&heartbeat).await {
                        return;
                    }
                    last_write = Instant::now();
                }
            }
        }
    }
}

/// Queue a received message for the decoder, counting it if there is no room.
fn offer(tx: &SyncSender<WireItem>, item: WireItem) {
    if let Err(std::sync::mpsc::TrySendError::Full(_)) = tx.try_send(item) {
        note_loss(&INPUT_DROPPED, &INPUT_NEXT_LOG, 1, || {
            "decoder input queue full, dropped network messages".into()
        });
    }
}

/// For an input connector: how long the server may send nothing, and how to name it.
struct Idle {
    limit: Duration,
    label: String,
}

/// Read Beast or raw messages from a client or server and queue them for the engine. With
/// `idle`, a server that sends nothing (not even a heartbeat) for that long is given up on,
/// so the connector reconnects (or tries its fallback), as upstream.
async fn serve_input(
    mut sock: TcpStream,
    beast: bool,
    tx: SyncSender<WireItem>,
    idle: Option<Idle>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    let mut parser = BeastParser::new();
    let mut lines: Vec<u8> = Vec::new();
    let mut receiver_id = 0u64;
    loop {
        let read = match &idle {
            None => sock.read(&mut buf).await,
            Some(i) => match tokio::time::timeout(i.limit, sock.read(&mut buf)).await {
                Ok(r) => r,
                Err(_) => {
                    eprintln!(
                        "net: {}: no data or heartbeat received for {:.0} s, reconnecting",
                        i.label,
                        i.limit.as_secs_f64()
                    );
                    return;
                }
            },
        };
        let Ok(n) = read else {
            return;
        };
        if n == 0 {
            return;
        }
        let now = now_ms();
        if beast {
            for frame in parser.feed(&buf[..n]) {
                match frame {
                    BeastFrame::ReceiverId(id) => receiver_id = id,
                    BeastFrame::Message {
                        timestamp,
                        signal,
                        data,
                    } => {
                        offer(
                            &tx,
                            WireItem {
                                data,
                                timestamp,
                                signal: Some(signal),
                                receiver_id,
                                now,
                            },
                        );
                    }
                }
            }
        } else {
            lines.extend_from_slice(&buf[..n]);
            // walk the lines, then drop them all at once (one drain per line would move the rest
            // of the buffer each time: a read of nothing but newlines would cost its square)
            let mut start = 0;
            while let Some(len) = lines[start..].iter().position(|&b| b == b'\n') {
                let line = &lines[start..start + len];
                start += len + 1;
                if let Some(raw) = std::str::from_utf8(line).ok().and_then(parse_raw_line) {
                    offer(
                        &tx,
                        WireItem {
                            data: raw.data,
                            timestamp: raw.timestamp.unwrap_or(0),
                            signal: None,
                            receiver_id: 0,
                            now,
                        },
                    );
                }
            }
            lines.drain(..start);
            // a client that never sends newlines must not grow the buffer without bound
            if lines.len() > 4096 {
                lines.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_host_cannot_take_every_client_slot() {
        // TEST-NET addresses, used by no other test
        let limit = ClientLimit::new(5, 2);
        let host: IpAddr = "192.0.2.77".parse().unwrap();
        let a = limit.take(host).expect("within the limit");
        let _b = limit.take(host).expect("within the limit");
        assert!(
            limit.take(host).is_none(),
            "one over the limit for one host"
        );
        let mapped: IpAddr = "::ffff:192.0.2.77".parse().unwrap();
        assert!(limit.take(mapped).is_none(), "the same host over IPv6");
        let _c = limit
            .take("192.0.2.78".parse().unwrap())
            .expect("another host");
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        let _d = limit.take(local).expect("loopback: no per-host limit");
        let _e = limit.take(local).expect("loopback: no per-host limit");
        assert!(limit.take(local).is_none(), "the overall limit");
        drop(a);
        assert!(limit.take(host).is_some(), "slots come back");
    }

    #[test]
    fn connector_parsing() {
        let c = Connector::parse("127.0.0.1,30005,beast_in").unwrap();
        assert_eq!(
            c,
            Connector {
                host: "127.0.0.1".into(),
                port: 30005,
                proto: Proto::BeastIn,
                fallback: None,
                silent_fail: false,
                uuid: None,
            }
        );
        assert_eq!(
            Connector::parse("h,1,sbs_out,extra").unwrap().proto,
            Proto::SbsOut
        );
        assert!(Connector::parse("127.0.0.1,30005").is_err());
        assert!(Connector::parse("127.0.0.1,x,beast_in").is_err());
        assert!(
            Connector::parse("127.0.0.1,1,asterix_out")
                .unwrap_err()
                .contains("unsupported")
        );
    }

    #[test]
    fn port_lists() {
        assert_eq!(parse_ports("0").unwrap(), Vec::<u16>::new());
        assert_eq!(parse_ports("30005,30105").unwrap(), vec![30005, 30105]);
        assert_eq!(parse_ports("").unwrap(), Vec::<u16>::new());
        assert!(parse_ports("abc").is_err());
        assert!(parse_ports("70000").is_err());
    }

    #[test]
    fn connect_attempts_are_capped_like_upstream() {
        assert_eq!(
            connect_timeout(Duration::from_secs(15)),
            Duration::from_secs(5)
        );
        assert_eq!(
            connect_timeout(Duration::from_secs(2)),
            Duration::from_secs(2)
        );
    }

    #[tokio::test]
    async fn a_host_that_never_answers_gives_up_within_the_limit() {
        // TEST-NET-1 is never routed: the attempt either fails at once or hangs until the limit
        let t = Instant::now();
        let r = connect_with_timeout("192.0.2.1", 30004, Duration::from_millis(300)).await;
        assert!(r.is_err());
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }

    /// A connected pair whose far end never reads, with both kernels' buffers already full,
    /// so that no further byte can be written.
    async fn stalled_pair() -> (TcpStream, TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (sock, accepted) = tokio::join!(
            async { TcpStream::connect(addr).await.unwrap() },
            listener.accept()
        );
        let chunk = vec![0u8; 64 * 1024];
        let mut idle_rounds = 0;
        // loopback moves data to the reader's buffer a moment after a write: fill until a
        // short wait frees no room either
        while idle_rounds < 3 {
            match sock.try_write(&chunk) {
                Ok(_) => idle_rounds = 0,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    idle_rounds += 1;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("{e}"),
            }
        }
        (sock, accepted.unwrap().0)
    }

    #[tokio::test]
    async fn a_peer_that_stops_reading_is_dropped() {
        let (mut sock, _peer) = stalled_pair().await;
        set_sockopts(&sock);
        let t = Instant::now();
        let connected = Instant::now() - Duration::from_secs(60); // past the grace
        let err = send(
            &mut sock,
            &[0u8; 1024],
            connected,
            Duration::from_millis(300),
            SEND_GRACE,
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(err.to_string().contains("couldn't send any data"));
        let took = t.elapsed();
        assert!(
            took >= Duration::from_millis(250) && took < Duration::from_secs(3),
            "{took:?}"
        );
    }

    #[tokio::test]
    async fn no_stall_is_declared_in_the_grace_period() {
        let (mut sock, _peer) = stalled_pair().await;
        // connected just now: a stall is only declared once the 1 s grace is over, although
        // the per-stall limit is 100 ms
        let connected = Instant::now();
        let err = send(
            &mut sock,
            &[0u8; 1024],
            connected,
            Duration::from_millis(100),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(
            connected.elapsed() >= Duration::from_millis(950),
            "{:?}",
            connected.elapsed()
        );
    }

    #[tokio::test]
    async fn an_input_connector_whose_server_goes_quiet_is_given_up() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (sock, accepted) = tokio::join!(
            async { TcpStream::connect(addr).await.unwrap() },
            listener.accept()
        );
        let (mut server, _) = accepted.unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(16);
        let idle = Idle {
            limit: Duration::from_millis(400),
            label: "test:1".into(),
        };
        let t = Instant::now();
        let reader = tokio::spawn(serve_input(sock, false, tx, Some(idle)));
        // data keeps it alive past the limit...
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            server
                .write_all(b"*8d4840d6202cc371c32ce0576098;\n")
                .await
                .unwrap();
        }
        assert!(
            !reader.is_finished(),
            "fed every 250 ms, a 400 ms limit must not fire"
        );
        // ...then silence ends it, with the server still connected
        tokio::time::timeout(Duration::from_secs(3), reader)
            .await
            .unwrap()
            .unwrap();
        assert!(
            t.elapsed() >= Duration::from_millis(1100),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(rx.try_iter().count(), 3);
    }

    #[test]
    fn connector_parsing_matches_upstream() {
        let c = Connector::parse("feed.adsb.lol,30004,beast_reduce_plus_out,feed2.adsb.lol,1337")
            .unwrap();
        assert_eq!(c.proto, Proto::BeastReducePlusOut);
        assert_eq!(c.fallback, Some(("feed2.adsb.lol".into(), 1337)));
        assert!(!c.silent_fail && c.uuid.is_none());
        let c = Connector::parse("127.0.0.1,30005,beast_in,silent_fail").unwrap();
        assert!(c.silent_fail && c.fallback.is_none());
        let c = Connector::parse("h,1,beast_reduce_out,uuid=0123456789abcdef0123456789abcdef0123")
            .unwrap();
        assert_eq!(
            c.uuid.as_deref(),
            Some("0123456789abcdef0123456789abcdef0123")
        );
        assert!(Connector::parse("h,1").is_err());
        assert!(Connector::parse("h,1,kinetic_out").is_err());
        // feeder installers add a UAT connector; it parses (and is skipped at startup)
        let c = Connector::parse("127.0.0.1,30978,uat_in,silent_fail").unwrap();
        assert_eq!(c.proto, Proto::UatIn);
    }

    #[test]
    fn uuid_preamble_and_pong_frames() {
        let p = uuid_preamble("0123456789abcdef0123456789abcdef\n").unwrap();
        assert_eq!(&p[..2], &[0x1a, 0xe4]);
        assert_eq!(&p[2..38], b"0123456789abcdef0123456789abcdefffff");
        assert_eq!(&p[38..], &[0x1a, b'W', b'O']);
        assert!(uuid_preamble("short").is_none());
        let mut pg = Pinger::default();
        pg.feed(&[0x1a, b'P', 0x01, 0x1a, 0x1a, 0x03]); // ping 0x011a03, escaped
        assert_eq!(pg.ping.map(|p| p.0), Some(0x01_1a03));
        let pong = pg.pong().unwrap();
        assert_eq!(&pong[..2], &[0x1a, b'P']);
        assert!(pong.len() >= 5);
    }

    #[test]
    fn output_is_batched_until_flush_or_size_limit() {
        let hub = Hub::new();
        let mut rx = hub.raw.subscribe();
        hub.send_raw(b"one\n".to_vec());
        hub.send_raw(b"two\n".to_vec());
        assert!(rx.try_recv().is_err(), "nothing is sent before a flush");
        hub.flush();
        assert_eq!(rx.try_recv().unwrap().as_slice(), b"one\ntwo\n");
        assert!(rx.try_recv().is_err());
        hub.flush(); // nothing queued: nothing sent
        assert!(rx.try_recv().is_err());
        // a message that would overflow the limit pushes out what is waiting first
        hub.send_raw(vec![b'a'; 1000]);
        // (the default limit is 1280 bytes)
        hub.send_raw(vec![b'b'; 1000]);
        assert_eq!(rx.try_recv().unwrap().len(), 1000);
        hub.flush();
        assert_eq!(rx.try_recv().unwrap().as_slice(), &[b'b'; 1000][..]);
    }

    #[test]
    fn flush_size_follows_net_ro_size_with_upstream_minimum() {
        let hub = Hub::with_flush_size(2000);
        let mut rx = hub.raw.subscribe();
        hub.send_raw(vec![b'a'; 1000]);
        hub.send_raw(vec![b'b'; 900]);
        assert!(rx.try_recv().is_err(), "1900 bytes fit in 2000");
        hub.send_raw(vec![b'c'; 200]);
        assert_eq!(rx.try_recv().unwrap().len(), 1900);
        assert_eq!(Hub::with_flush_size(10).flush_size, MIN_FLUSH_SIZE);
        // a single message at or over the size goes out at once
        let hub = Hub::with_flush_size(800);
        let mut rx = hub.raw.subscribe();
        hub.send_raw(vec![b'x'; 800]);
        assert_eq!(rx.try_recv().unwrap().len(), 800);
    }

    #[tokio::test]
    async fn output_and_input_round_trip() {
        let hub = Hub::new();
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        // find free ports
        let free = || {
            let l = StdListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let (out_port, in_port) = (free(), free());
        let cfg = NetConfig {
            bind: "127.0.0.1".parse().unwrap(),
            raw_out: vec![out_port],
            beast_out: vec![],
            sbs_out: vec![],
            raw_in: vec![in_port],
            beast_in: vec![],
            beast_reduce_out: vec![],
            connectors: vec![],
            uuid: None,
            heartbeat: Duration::from_millis(50),
            connector_delay: Duration::from_secs(1),
        };
        spawn(&Handle::current(), &cfg, &hub, &tx).unwrap();

        let mut client = TcpStream::connect(("127.0.0.1", out_port)).await.unwrap();
        while hub.raw_clients() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        hub.send_raw(b"*8D4840D6202CC371C32CE0576098;\n".to_vec());
        hub.flush();
        let mut got = vec![0u8; 31];
        client.read_exact(&mut got).await.unwrap();
        assert_eq!(got, b"*8D4840D6202CC371C32CE0576098;\n");
        // an idle connection gets heartbeats
        let mut hb = vec![0u8; RAW_HEARTBEAT.len()];
        client.read_exact(&mut hb).await.unwrap();
        assert_eq!(hb, RAW_HEARTBEAT);

        let mut sender = TcpStream::connect(("127.0.0.1", in_port)).await.unwrap();
        sender
            .write_all(b"*8d4840d6202cc371c32ce0576098;\r\nnonsense\n*5D4840D6000000;\n")
            .await
            .unwrap();
        let first = tokio::task::spawn_blocking(move || {
            (
                rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            )
        })
        .await
        .unwrap();
        assert_eq!(first.0.data.len(), 14);
        assert_eq!(first.1.data.len(), 7);
        assert!(first.0.signal.is_none());
    }

    #[tokio::test]
    async fn port_in_use_is_reported() {
        let l = StdListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let cfg = NetConfig {
            bind: "127.0.0.1".parse().unwrap(),
            raw_out: vec![port],
            beast_out: vec![],
            sbs_out: vec![],
            raw_in: vec![],
            beast_in: vec![],
            beast_reduce_out: vec![],
            connectors: vec![],
            uuid: None,
            heartbeat: Duration::ZERO,
            connector_delay: Duration::from_secs(1),
        };
        let err = spawn(&Handle::current(), &cfg, &Hub::new(), &tx).unwrap_err();
        assert!(err.to_string().contains("can't listen"), "{err}");
    }
}
