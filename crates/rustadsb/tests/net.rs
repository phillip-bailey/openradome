//! End-to-end network test: run the binary with no SDR, push raw messages into its
//! input port, and read them back from the Beast, raw and SBS output ports.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The tests in this file run one at a time: a port picked by one test is free only until the server
/// binds it, and a parallel test's server or client socket (macOS hands out client ports from the same
/// range) could take it in between.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// `N` distinct free ports, held together until all are picked so the system cannot hand one out twice.
fn free_ports<const N: usize>() -> [u16; N] {
    let held: Vec<TcpListener> = (0..N)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    std::array::from_fn(|i| held[i].local_addr().unwrap().port())
}

fn connect(port: u16) -> TcpStream {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => {
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                return s;
            }
            Err(e) if Instant::now() > end => panic!("can't connect to {port}: {e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn read_until(s: &mut TcpStream, want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while got.len() < want {
        let n = s.read(&mut buf).expect("read");
        assert!(n > 0, "connection closed after {} bytes", got.len());
        got.extend_from_slice(&buf[..n]);
    }
    got
}

const IDENT: &str = "8D4840D6202CC371C32CE0576098";

#[test]
fn raw_in_to_all_outputs_and_json() {
    let _serial = serial();
    let dir = std::env::temp_dir().join(format!("rustadsb-net-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let [ri, bo, ro, sbs] = free_ports();
    let child = Command::new(env!("CARGO_BIN_EXE_rustadsb"))
        .args([
            "--net-bind-address",
            "127.0.0.1",
            "--net-ri-port",
            &ri.to_string(),
            "--net-bo-port",
            &bo.to_string(),
            "--net-ro-port",
            &ro.to_string(),
            "--net-sbs-port",
            &sbs.to_string(),
            "--net-heartbeat",
            "0",
            "--quiet",
            "--write-json",
            dir.to_str().unwrap(),
            "--write-json-every",
            "0.1",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn");
    let _server = Server(child);

    let mut beast = connect(bo);
    let mut raw = connect(ro);
    let mut sbs_c = connect(sbs);
    // Beast clients first get five heartbeats
    let hb = read_until(&mut beast, 55);
    assert_eq!(&hb[..11], &[0x1a, b'1', 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    let mut input = connect(ri);
    // let the server register the output clients before sending
    std::thread::sleep(Duration::from_millis(300));
    input
        .write_all(format!("*{IDENT};\n*{};\n", IDENT.to_lowercase()).as_bytes())
        .unwrap();

    let line = format!("*{IDENT};\n");
    let got = read_until(&mut raw, line.len() * 2);
    assert_eq!(&got[..line.len() * 2], format!("{line}{line}").as_bytes());

    // Beast: 0x1a '3' + 6 byte timestamp + signal + 14 bytes
    let frame = read_until(&mut beast, 2 + 6 + 1 + 14);
    assert_eq!(&frame[..2], &[0x1a, b'3']);
    assert_eq!(&frame[9..23], &hex(IDENT)[..]);

    let sbs_line = String::from_utf8(read_until(&mut sbs_c, 20)).unwrap();
    assert!(sbs_line.starts_with("MSG,1,1,1,4840D6,1,"), "{sbs_line}");

    // the aircraft shows up in aircraft.json (two messages are enough to be listed)
    let end = Instant::now() + Duration::from_secs(10);
    let json = loop {
        if let Ok(j) = std::fs::read_to_string(dir.join("aircraft.json"))
            && j.contains("4840d6")
        {
            break j;
        }
        assert!(
            Instant::now() < end,
            "aircraft never appeared in aircraft.json"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(json.contains("\"flight\":\"KLM1023 \""), "{json}");
    assert!(dir.join("receiver.json").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

#[test]
fn beast_in_is_decoded_and_forwarded() {
    let _serial = serial();
    let [bi, ro] = free_ports();
    let child = Command::new(env!("CARGO_BIN_EXE_rustadsb"))
        .args([
            "--net-bind-address",
            "127.0.0.1",
            "--net-bi-port",
            &bi.to_string(),
            "--net-ro-port",
            &ro.to_string(),
            "--quiet",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn");
    let _server = Server(child);
    let mut raw = connect(ro);
    let mut input = connect(bi);
    std::thread::sleep(Duration::from_millis(300));
    // a Beast long frame: ts 1, signal 100, message
    let mut frame = vec![0x1a, b'3', 0, 0, 0, 0, 0, 1, 100];
    frame.extend(hex(IDENT));
    input.write_all(&frame).unwrap();
    // a corrupted copy is not forwarded; the intact one after it is
    let mut bad = frame.clone();
    bad[20] ^= 0xff;
    input.write_all(&bad).unwrap();
    input.write_all(&frame).unwrap();
    let line = format!("*{IDENT};\n");
    let got = read_until(&mut raw, line.len() * 2);
    assert_eq!(&got[..line.len() * 2], format!("{line}{line}").as_bytes());
}

#[test]
fn listen_port_in_use_is_an_error() {
    let _serial = serial();
    let l = TcpListener::bind("0.0.0.0:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let out = Command::new(env!("CARGO_BIN_EXE_rustadsb"))
        .args(["--net-ro-port", &port.to_string()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("can't listen"));
}

#[test]
fn bad_connector_is_an_error() {
    let _serial = serial();
    let out = Command::new(env!("CARGO_BIN_EXE_rustadsb"))
        .args(["--net-connector", "127.0.0.1,1,telnet"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unsupported net-connector protocol"));
}

/// `GET` a page from `port` and return the whole reply.
fn http_get(port: u16, path: &str) -> String {
    let mut s = connect(port);
    write!(s, "GET {path} HTTP/1.1\r\nHost: test\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).expect("read reply");
    out
}

#[test]
fn dashboard_shows_the_open_connections() {
    let _serial = serial();
    let [bo, dash] = free_ports();
    let child = Command::new(env!("CARGO_BIN_EXE_rustadsb"))
        .args([
            "--net-bind-address",
            "127.0.0.1",
            "--net-bo-port",
            &bo.to_string(),
            "--net-heartbeat",
            "0",
            "--quiet",
            "--dashboard-port",
            &dash.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn");
    let _server = Server(child);
    let _client = connect(bo);
    // the connection is registered by the accepting task, a moment after connect returns
    let end = Instant::now() + Duration::from_secs(5);
    let page = loop {
        let page = http_get(dash, "/");
        if page.contains("Connected to this decoder (1)") || Instant::now() > end {
            break page;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(page.starts_with("HTTP/1.1 200 OK\r\n"), "{page}");
    assert!(page.contains("Connected to this decoder (1)"), "{page}");
    assert!(page.contains(&format!("beast out :{bo}")), "{page}");
    assert!(
        !page.contains("Signal and noise"),
        "no local receiver, so no signal graph"
    );
    assert!(http_get(dash, "/nothing").starts_with("HTTP/1.1 404"));
}
