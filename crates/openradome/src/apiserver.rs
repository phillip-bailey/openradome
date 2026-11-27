//! `--net-api-port`: upstream's HTTP API (`/re-api?...`) for the aircraft list.
//!
//! The engine thread publishes a snapshot of the aircraft list once per `--write-json-every` interval;
//! tokio tasks answer requests from the latest snapshot, so a slow client never delays decoding. The
//! request rules follow `apiReadRequest` in upstream's `api.c`: only `GET`, only HTTP/1.0 and 1.1,
//! the whole request is lower-cased before parsing, and the path is ignored (only the query counts).

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use openradome_core::api::{self, LIST_MAX, Snapshot};
use openradome_core::track::Tracker;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Handle;

/// Upstream's limit on a request: `1024 + 13 * API_REQ_LIST_MAX + 32` bytes.
const REQUEST_MAX: usize = 1024 + 13 * LIST_MAX + 32;

struct Shared {
    snapshot: Mutex<Arc<Snapshot>>,
    shutting_down: AtomicBool,
    version: &'static str,
}

/// The publishing half, owned by the engine.
pub struct Api {
    shared: Arc<Shared>,
    interval_ms: i64,
    next: i64,
    rate_last: (i64, u64),
    message_rate: f64,
}

impl Api {
    /// Start listening on `bind:port`.
    pub fn start(
        rt: &Handle,
        bind: IpAddr,
        port: u16,
        interval_ms: i64,
        now: i64,
        version: &'static str,
    ) -> io::Result<Self> {
        let std_listener = std::net::TcpListener::bind(SocketAddr::new(bind, port))?;
        std_listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            snapshot: Mutex::new(Arc::new(Snapshot {
                now_ms: now,
                entries: Vec::new(),
                aircraft_with_position: 0,
                message_count: 0,
                message_rate: 0.0,
                flags: 1,
            })),
            shutting_down: AtomicBool::new(false),
            version,
        });
        let listener = {
            let _guard = rt.enter();
            TcpListener::from_std(std_listener)?
        };
        let s = shared.clone();
        let slots = crate::net::ClientLimit::new(MAX_CONNECTIONS, MAX_CONNECTIONS_PER_HOST);
        rt.spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((sock, peer)) => {
                        let Some(slot) = slots.take(peer.ip()) else {
                            continue; // too many open: drop this one
                        };
                        let s = s.clone();
                        tokio::spawn(async move {
                            serve(sock, s).await;
                            drop(slot);
                        });
                    }
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                }
            }
        });
        Ok(Self {
            shared,
            interval_ms,
            next: now,
            rate_last: (now, 0),
            message_rate: 0.0,
        })
    }

    /// Publish a fresh snapshot when the interval is due.
    pub fn tick(&mut self, now: i64, t: &Tracker) {
        if now < self.next && self.next - now <= self.interval_ms {
            return;
        }
        self.next = now + self.interval_ms;
        let (t0, m0) = self.rate_last;
        if now - t0 >= 1000 {
            let msgs = t.stats.messages_total;
            self.message_rate = msgs.saturating_sub(m0) as f64 * 1000.0 / (now - t0) as f64;
            self.rate_last = (now, msgs);
        }
        let snap = Arc::new(Snapshot::build(t, now, self.message_rate));
        *self
            .shared
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = snap;
    }

    /// `?status` answers 503 from now on, so a load balancer stops sending new clients.
    pub fn begin_shutdown(&self) {
        self.shared.shutting_down.store(true, Ordering::SeqCst);
    }
}

fn status_reply(status: &str, keepalive: bool) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nServer: openradome\r\nConnection: {}\r\nCache-control: no-store\r\nContent-length: 0\r\n\r\n",
        if keepalive { "keep-alive" } else { "close" }
    )
    .into_bytes()
}

/// What to do after a request.
struct Outcome {
    reply: Vec<u8>,
    keepalive: bool,
}

fn handle(raw: &[u8], shared: &Shared) -> Outcome {
    let received = Instant::now();
    let text = String::from_utf8_lossy(raw).into_owned();
    let line_end = text.find('\n').unwrap_or(text.len());
    let first = text[..line_end].trim_end_matches('\r');
    let mut parts = first.split(' ');
    let (method, target, protocol) = (parts.next(), parts.next(), parts.next());
    let minor = match protocol {
        Some("HTTP/1.1") => 1,
        Some("HTTP/1.0") => 0,
        _ => {
            return Outcome {
                reply: status_reply("505 HTTP Version Not Supported", false),
                keepalive: false,
            };
        }
    };
    // headers (lower-cased, as upstream does for the whole request)
    let mut keepalive = minor == 1;
    let mut accept_zstd = false;
    for h in text[line_end..].lines().map(str::to_ascii_lowercase) {
        if h.starts_with("accept-encoding") && h.contains("zstd") {
            accept_zstd = true;
        }
        if h.starts_with("connection") {
            if h.contains("close") {
                keepalive = false;
            } else if keepalive || h.contains("keep-alive") {
                keepalive = true;
            }
        }
    }
    if method != Some("GET") {
        return Outcome {
            reply: status_reply("405 Method Not Allowed", keepalive),
            keepalive,
        };
    }
    let target = target.unwrap_or("").to_ascii_lowercase();
    if target.ends_with("?status") {
        let status = if shared.shutting_down.load(Ordering::SeqCst) {
            "503 Service Unavailable"
        } else {
            "200 OK"
        };
        return Outcome {
            reply: status_reply(status, keepalive),
            keepalive,
        };
    }
    let Some(query) = target.split_once('?').map(|(_, q)| q) else {
        return Outcome {
            reply: status_reply("400 Bad Request", keepalive),
            keepalive,
        };
    };
    let Some(q) = api::parse_query(query) else {
        return Outcome {
            reply: status_reply("400 Bad Request", keepalive),
            keepalive,
        };
    };
    let snap = shared
        .snapshot
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut body = api::reply(&snap, &q, received.elapsed().as_secs_f64() * 1000.0);
    let content_type = if q.zstd {
        "application/zstd"
    } else if q.bincraft {
        "application/octet-stream"
    } else {
        "application/json"
    };
    // `zstd` in the query compresses the body itself; the header asks for transport compression
    let encode = (q.zstd || accept_zstd) && !body.is_empty();
    if encode {
        body = crate::jsonout::zstd(&body);
    }
    let content_encoding = if accept_zstd && !q.zstd {
        "Content-Encoding: zstd\r\n"
    } else {
        ""
    };
    let version = if q.include_version {
        format!("readsb_version: {}\r\n", shared.version)
    } else {
        String::new()
    };
    let mut reply = format!(
        "HTTP/1.1 200 OK\r\nServer: openradome\r\n{version}Content-Type: {content_type}\r\nConnection: {}\r\nCache-Control: no-store\r\n{content_encoding}Content-Length: {}\r\n\r\n",
        if keepalive { "keep-alive" } else { "close" },
        body.len()
    )
    .into_bytes();
    reply.extend_from_slice(&body);
    Outcome { reply, keepalive }
}

/// Connections served at once (each holds a file descriptor); more are closed straight away.
const MAX_CONNECTIONS: usize = 128;
/// ... and from one address other than this computer's own (a proxy in front comes from there).
const MAX_CONNECTIONS_PER_HOST: usize = 16;
/// A kept-alive connection with no new request for this long is closed (nginx's own keep-alive
/// is 60 s), and a reply must be taken within `WRITE_TIMEOUT`.
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(75);
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn serve(mut sock: TcpStream, shared: Arc<Shared>) {
    let _ = sock.set_nodelay(true);
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        // read until a complete request (a blank line) is there
        let read = async {
            loop {
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    return Some(pos + 4);
                }
                if buf.len() > REQUEST_MAX {
                    return None;
                }
                match sock.read(&mut chunk).await {
                    Ok(0) | Err(_) => return Some(0),
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
        };
        let end = match tokio::time::timeout(IDLE_TIMEOUT, read).await {
            Err(_) | Ok(Some(0)) => return,
            Ok(Some(end)) => end,
            Ok(None) => {
                let _ = tokio::time::timeout(
                    WRITE_TIMEOUT,
                    sock.write_all(&status_reply("400 Bad Request", false)),
                )
                .await;
                return;
            }
        };
        let request: Vec<u8> = buf.drain(..end).collect();
        let out = handle(&request, &shared);
        let written = tokio::time::timeout(WRITE_TIMEOUT, sock.write_all(&out.reply)).await;
        if !matches!(written, Ok(Ok(()))) || !out.keepalive {
            let _ = sock.shutdown().await;
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        Shared {
            snapshot: Mutex::new(Arc::new(Snapshot {
                now_ms: 1_700_000_000_000,
                entries: Vec::new(),
                aircraft_with_position: 0,
                message_count: 0,
                message_rate: 0.0,
                flags: 1,
            })),
            shutting_down: AtomicBool::new(false),
            version: "openradome test",
        }
    }

    fn run(req: &str) -> (String, bool) {
        let o = handle(req.as_bytes(), &shared());
        (String::from_utf8_lossy(&o.reply).into_owned(), o.keepalive)
    }

    #[test]
    fn status_and_errors() {
        let (r, ka) = run("GET /?status HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(
            r.starts_with("HTTP/1.1 200 OK\r\n") && r.contains("Content-length: 0"),
            "{r}"
        );
        assert!(ka, "HTTP/1.1 keeps the connection by default");
        assert!(
            !run("GET /?all HTTP/1.0\r\n\r\n").1,
            "HTTP/1.0 closes by default"
        );
        assert!(
            run("POST /?all HTTP/1.1\r\n\r\n")
                .0
                .starts_with("HTTP/1.1 405")
        );
        assert!(
            run("GET /?all HTTP/2.0\r\n\r\n")
                .0
                .starts_with("HTTP/1.1 505")
        );
        assert!(
            run("GET /nothing HTTP/1.1\r\n\r\n")
                .0
                .starts_with("HTTP/1.1 400")
        );
        assert!(
            run("GET /?bogus HTTP/1.1\r\n\r\n")
                .0
                .starts_with("HTTP/1.1 400")
        );
        let (r, ka) = run("GET /?status HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(r.contains("Connection: close") && !ka);
    }

    #[test]
    fn a_query_gets_json_with_headers() {
        let (r, _) = run("GET /re-api/?All&Include_Version HTTP/1.1\r\n\r\n");
        assert!(
            r.starts_with(
                "HTTP/1.1 200 OK\r\nServer: openradome\r\nreadsb_version: openradome test\r\n"
            ),
            "{r}"
        );
        assert!(r.contains("Content-Type: application/json\r\n"));
        assert!(r.contains("Cache-Control: no-store\r\n"));
        assert!(
            r.contains("\"resultCount\": 0"),
            "the query is case-insensitive: {r}"
        );
    }

    #[test]
    fn shutdown_turns_status_into_503() {
        let s = shared();
        s.shutting_down.store(true, Ordering::SeqCst);
        let o = handle(b"GET /?status HTTP/1.1\r\n\r\n", &s);
        assert!(String::from_utf8_lossy(&o.reply).starts_with("HTTP/1.1 503"));
    }

    #[tokio::test]
    async fn serves_over_a_socket_with_keepalive() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let api = Api::start(
            &Handle::current(),
            "127.0.0.1".parse().unwrap(),
            0,
            1000,
            0,
            "t",
        );
        // port 0 can't be read back from Api; bind our own listener to prove the loop works
        drop(api);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sh = Arc::new(shared());
        tokio::spawn({
            let sh = sh.clone();
            async move {
                let (s, _) = listener.accept().await.unwrap();
                serve(s, sh).await;
            }
        });
        let mut c = TcpStream::connect(addr).await.unwrap();
        for _ in 0..2 {
            c.write_all(b"GET /?all HTTP/1.1\r\n\r\n").await.unwrap();
            let mut got = vec![0u8; 4096];
            let n = c.read(&mut got).await.unwrap();
            assert!(String::from_utf8_lossy(&got[..n]).contains("\"resultCount\": 0"));
        }
    }
}
