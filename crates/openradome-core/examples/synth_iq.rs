//! Write a synthetic IQ capture of Mode S frames over a noise floor.
//!
//! usage: synth_iq <out.bin> [options] [< frames.txt]
//!
//!   --count N         bursts to write (default 50)
//!   --seed S          random seed (default 1)
//!   --noise L         noise floor, uniform 0..L on the magnitude (default 300)
//!   --format F        uc8 (default), sc16 or sc16q11
//!   --random N        also generate N random valid frames (all common downlink formats)
//!   --flip P          percent of bursts that get 1-2 bit errors on the air (default 0)
//!   --overlap P       percent of bursts that start on top of the previous one (default 0)
//!   --min-amp/--max-amp  pulse amplitude range, 0..65535 (default 4000..54000)
//!   --flights N       simulate N aircraft flying straight lines around (52.0, 4.0) instead of
//!                     random bursts: identification, even/odd position, velocity, DF11 and
//!                     DF4/DF5 replies on realistic schedules. Needs --seconds.
//!   --seconds S       length of the simulated capture (default 10)
//!   --glitch P        percent of airborne position frames that carry a random position (with a
//!                     valid CRC), to exercise the tracker's plausibility checks (default 0)
//!   --ground P        percent of flights that are vehicles on the airport surface (default 0)
//!
//! Optional hex frames (one per line) on stdin are included as-is. Bursts cycle through
//! all frames. The frames actually transmitted, in order, go to `<out.bin>.truth`.

use std::io::{BufRead, IsTerminal, Write};

use openradome_core::crc::checksum;
use openradome_core::synth::{modulate, to_uc8};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
    fn pct(&mut self, p: u64) -> bool {
        self.below(100) < p
    }
}

fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim().trim_start_matches('*').trim_end_matches(';');
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

/// Fill the last three bytes with parity; `addr` is XORed in for address/parity formats.
fn set_parity(frame: &mut [u8], addr: Option<u32>) {
    let n = frame.len();
    frame[n - 3..].fill(0);
    let p = checksum(frame) ^ addr.unwrap_or(0);
    frame[n - 3] = (p >> 16) as u8;
    frame[n - 2] = (p >> 8) as u8;
    frame[n - 1] = p as u8;
}

fn random_frame(rng: &mut Rng, pool: &[u32]) -> Vec<u8> {
    let addr = pool[rng.below(pool.len() as u64) as usize];
    let a = [(addr >> 16) as u8, (addr >> 8) as u8, addr as u8];
    match rng.below(10) {
        // DF17 extended squitter, weighted towards the interesting ME types
        0..=4 => {
            let mut f = vec![0x8D, a[0], a[1], a[2]];
            let mut me = rng.bytes(7);
            let metype = match rng.below(8) {
                0 => 1 + rng.below(4) as u8,      // identification
                1 | 2 => 9 + rng.below(10) as u8, // airborne position (baro)
                3 => 20 + rng.below(3) as u8,     // airborne position (geometric)
                4 => 19,                          // velocity
                5 => 5 + rng.below(4) as u8,      // surface position
                6 => [28, 29, 31][rng.below(3) as usize],
                _ => rng.below(32) as u8,
            };
            me[0] = (metype << 3) | (me[0] & 7);
            f.extend(me);
            f.extend([0, 0, 0]);
            set_parity(&mut f, None);
            f
        }
        // DF11 all-call reply, II = 0
        5 | 6 => {
            let mut f = vec![0x5D, a[0], a[1], a[2], 0, 0, 0];
            set_parity(&mut f, None);
            f
        }
        // short address/parity: DF0, DF4, DF5
        7 => {
            let df = [0u8, 4, 5][rng.below(3) as usize];
            let mut f = vec![(df << 3) | (rng.next() as u8 & 7)];
            f.extend(rng.bytes(3));
            f.extend([0, 0, 0]);
            set_parity(&mut f, Some(addr));
            f
        }
        // long address/parity: DF16, DF20, DF21
        8 => {
            let df = [16u8, 20, 21][rng.below(3) as usize];
            let mut f = vec![(df << 3) | (rng.next() as u8 & 7)];
            f.extend(rng.bytes(10));
            f.extend([0, 0, 0]);
            set_parity(&mut f, Some(addr));
            f
        }
        // DF18 (TIS-B / non-transponder), CF random
        _ => {
            let mut f = vec![0x90 | (rng.below(8) as u8), a[0], a[1], a[2]];
            f.extend(rng.bytes(7));
            f.extend([0, 0, 0]);
            set_parity(&mut f, None);
            f
        }
    }
}

/// Pack bit fields MSB first.
struct Bits {
    bytes: Vec<u8>,
    n: usize,
}
impl Bits {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            n: 0,
        }
    }
    fn put(&mut self, value: u64, width: usize) {
        for i in (0..width).rev() {
            if self.n.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (value >> i) & 1 == 1 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 0x80 >> (self.n % 8);
            }
            self.n += 1;
        }
    }
}

fn df17(addr: u32, me: Vec<u8>) -> Vec<u8> {
    let mut f = vec![0x8D, (addr >> 16) as u8, (addr >> 8) as u8, addr as u8];
    f.extend(me);
    f.extend([0, 0, 0]);
    set_parity(&mut f, None);
    f
}

fn encode_ident(addr: u32, callsign: &str, category: u8) -> Vec<u8> {
    const SET: &str = "@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_ !\"#$%&'()*+,-./0123456789:;<=>?";
    let mut b = Bits::new();
    b.put(4, 5);
    b.put(category as u64, 3);
    for c in callsign.chars().chain(std::iter::repeat(' ')).take(8) {
        b.put(SET.find(c).unwrap_or(32) as u64, 6);
    }
    df17(addr, b.bytes)
}

fn cpr_encode(lat: f64, lon: f64, odd: bool) -> (u32, u32) {
    let i = odd as i32;
    let dlat = 360.0 / (60 - i) as f64;
    let yz = (131072.0 * (lat.rem_euclid(dlat) / dlat) + 0.5).floor();
    let rlat = dlat * (yz / 131072.0 + (lat / dlat).floor());
    let nl = (openradome_core::cpr::nl(rlat) - i).max(1);
    let dlon = 360.0 / nl as f64;
    let xz = (131072.0 * (lon.rem_euclid(dlon) / dlon) + 0.5).floor();
    ((yz as u32) & 0x1FFFF, (xz as u32) & 0x1FFFF)
}

fn encode_position(addr: u32, lat: f64, lon: f64, alt_ft: i32, odd: bool) -> Vec<u8> {
    let n = ((alt_ft + 1000) / 25) as u64;
    let alt12 = ((n & 0x7F0) << 1) | 0x10 | (n & 0x0F);
    let (la, lo) = cpr_encode(lat, lon, odd);
    let mut b = Bits::new();
    b.put(11, 5); // airborne position, barometric altitude
    b.put(0, 2); // surveillance status
    b.put(0, 1); // NIC supplement B
    b.put(alt12, 12);
    b.put(0, 1); // time
    b.put(odd as u64, 1);
    b.put(la as u64, 17);
    b.put(lo as u64, 17);
    df17(addr, b.bytes)
}

/// Surface CPR spans 90 degrees instead of 360.
fn cpr_encode_surface(lat: f64, lon: f64, odd: bool) -> (u32, u32) {
    let i = odd as i32;
    let dlat = 90.0 / (60 - i) as f64;
    let yz = (131072.0 * (lat.rem_euclid(dlat) / dlat) + 0.5).floor();
    let rlat = dlat * (yz / 131072.0 + (lat / dlat).floor());
    let nl = (openradome_core::cpr::nl(rlat) - i).max(1);
    let dlon = 90.0 / nl as f64;
    let xz = (131072.0 * (lon.rem_euclid(dlon) / dlon) + 0.5).floor();
    ((yz as u32) & 0x1FFFF, (xz as u32) & 0x1FFFF)
}

fn encode_surface(addr: u32, lat: f64, lon: f64, gs_kt: f64, track: f64, odd: bool) -> Vec<u8> {
    // movement: 15..70 kt in 1 kt steps from code 39; 2..15 kt in 0.5 kt steps from code 13
    let movement = if gs_kt >= 15.0 {
        39 + ((gs_kt - 15.0) as u64).min(54)
    } else {
        13 + (((gs_kt - 2.0).max(0.0)) * 2.0) as u64
    };
    let (la, lo) = cpr_encode_surface(lat, lon, odd);
    let mut b = Bits::new();
    b.put(6, 5); // surface position
    b.put(movement, 7);
    b.put(1, 1); // heading valid
    b.put(((track / 360.0 * 128.0) as u64) & 0x7F, 7);
    b.put(0, 1); // time
    b.put(odd as u64, 1);
    b.put(la as u64, 17);
    b.put(lo as u64, 17);
    df17(addr, b.bytes)
}

fn encode_velocity(addr: u32, gs_kt: f64, track_deg: f64, vrate_fpm: i32) -> Vec<u8> {
    let ew = gs_kt * track_deg.to_radians().sin();
    let ns = gs_kt * track_deg.to_radians().cos();
    let mut b = Bits::new();
    b.put(19, 5);
    b.put(1, 3); // subtype 1: ground speed, subsonic
    b.put(0, 1); // intent change
    b.put(0, 1); // reserved
    b.put(2, 3); // NAC velocity
    b.put((ew < 0.0) as u64, 1);
    b.put(ew.abs().round() as u64 + 1, 10);
    b.put((ns < 0.0) as u64, 1);
    b.put(ns.abs().round() as u64 + 1, 10);
    b.put(1, 1); // vertical rate source: barometric
    b.put((vrate_fpm < 0) as u64, 1);
    b.put((vrate_fpm.unsigned_abs() / 64 + 1) as u64, 9);
    b.put(0, 2);
    b.put(0, 1); // geometric - baro sign
    b.put(10, 7); // difference 225 ft
    df17(addr, b.bytes)
}

fn encode_df11(addr: u32) -> Vec<u8> {
    let mut f = vec![
        0x5D,
        (addr >> 16) as u8,
        (addr >> 8) as u8,
        addr as u8,
        0,
        0,
        0,
    ];
    set_parity(&mut f, None);
    f
}

/// DF4 (altitude reply) with address/parity.
fn encode_df4(addr: u32, alt_ft: i32) -> Vec<u8> {
    let n = ((alt_ft + 1000) / 25) as u32;
    let ac13 = ((n & 0x7E0) << 2) | ((n & 0x10) << 1) | 0x10 | (n & 0x0F);
    let mut b = Bits::new();
    b.put(4, 5);
    b.put(0, 3); // FS
    b.put(0, 5); // DR
    b.put(0, 6); // UM
    b.put(ac13 as u64, 13);
    let mut f = b.bytes;
    f.extend([0, 0, 0]);
    set_parity(&mut f, Some(addr));
    f
}

struct Flight {
    addr: u32,
    callsign: String,
    lat: f64,
    lon: f64,
    gs: f64,
    track: f64,
    alt: f64,
    vrate: i32,
    ground: bool,
    // schedule offsets (ms)
    ident_off: i64,
    pos_off: i64,
    vel_off: i64,
    df11_off: i64,
    df4_off: i64,
}

impl Flight {
    fn position_at(&self, t_ms: i64) -> (f64, f64, i32) {
        let secs = t_ms as f64 / 1000.0;
        let dist_deg = self.gs * 1852.0 / 3600.0 * secs / 111_320.0;
        let lat = self.lat + dist_deg * self.track.to_radians().cos();
        let lon = self.lon + dist_deg * self.track.to_radians().sin() / self.lat.to_radians().cos();
        let alt = self.alt + self.vrate as f64 * secs / 60.0;
        (lat, lon, alt as i32)
    }
}

fn simulate(
    rng: &mut Rng,
    flights: usize,
    seconds: i64,
    ground_pct: u64,
    glitch_pct: u64,
) -> Vec<(i64, Vec<u8>)> {
    let mut fl: Vec<Flight> = Vec::new();
    for i in 0..flights {
        let addr = 0x400000 + (rng.below(0xFFFF) as u32) * 16 + i as u32;
        let ground = rng.pct(ground_pct);
        fl.push(Flight {
            addr,
            callsign: format!("TST{:04}", rng.below(10000)),
            lat: if ground {
                52.0 + (rng.below(400) as f64 - 200.0) / 1000.0
            } else {
                52.0 + (rng.below(3000) as f64 - 1500.0) / 1000.0
            },
            lon: if ground {
                4.0 + (rng.below(600) as f64 - 300.0) / 1000.0
            } else {
                4.0 + (rng.below(5000) as f64 - 2500.0) / 1000.0
            },
            gs: if ground {
                5.0 + rng.below(30) as f64
            } else {
                100.0 + rng.below(450) as f64
            },
            track: rng.below(360) as f64,
            alt: if ground {
                0.0
            } else {
                2000.0 + rng.below(38000) as f64
            },
            vrate: if ground {
                0
            } else {
                (rng.below(3000) as i32 - 1500) / 64 * 64
            },
            ground,
            ident_off: rng.below(5000) as i64,
            pos_off: rng.below(500) as i64,
            vel_off: rng.below(500) as i64,
            df11_off: rng.below(1000) as i64,
            df4_off: rng.below(1500) as i64,
        });
    }
    let mut events: Vec<(i64, Vec<u8>)> = Vec::new();
    for f in &fl {
        let mut t = f.ident_off;
        while t < seconds * 1000 {
            events.push((t, encode_ident(f.addr, &f.callsign, 0)));
            t += 5000;
        }
        let mut t = f.pos_off;
        let mut odd = rng.below(2) == 1;
        while t < seconds * 1000 {
            let (la, lo, alt) = f.position_at(t);
            if f.ground {
                events.push((t, encode_surface(f.addr, la, lo, f.gs, f.track, odd)));
            } else if rng.pct(glitch_pct) {
                // a plausible-looking but wrong position somewhere else
                let (gl, go) = (
                    la + (rng.below(8000) as f64 - 4000.0) / 1000.0,
                    lo + (rng.below(8000) as f64 - 4000.0) / 1000.0,
                );
                events.push((t, encode_position(f.addr, gl, go, alt, odd)));
            } else {
                events.push((t, encode_position(f.addr, la, lo, alt, odd)));
            }
            odd = !odd;
            t += 500;
        }
        let mut t = f.vel_off;
        while t < seconds * 1000 && !f.ground {
            events.push((t, encode_velocity(f.addr, f.gs, f.track, f.vrate)));
            t += 500;
        }
        let mut t = f.df11_off;
        while t < seconds * 1000 {
            events.push((t, encode_df11(f.addr)));
            t += 1000;
        }
        let mut t = f.df4_off;
        while t < seconds * 1000 && !f.ground {
            let (_, _, alt) = f.position_at(t);
            events.push((t, encode_df4(f.addr, alt)));
            t += 1500;
        }
    }
    events.sort_by_key(|e| e.0);
    events
}

fn to_format(mags: &[u16], format: &str) -> Vec<u8> {
    match format {
        "uc8" => to_uc8(mags),
        "sc16" | "sc16q11" => {
            let full = if format == "sc16" { 32767.0 } else { 2047.0 };
            let mut out = Vec::with_capacity(mags.len() * 4);
            for &m in mags {
                let i = (m as f64 / 65535.0 * full).round() as i16;
                out.extend_from_slice(&i.to_le_bytes());
                out.extend_from_slice(&0i16.to_le_bytes());
            }
            out
        }
        other => panic!("unknown format {other}"),
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out_path = args.next().expect("usage: synth_iq <out.bin> [options]");
    let (mut count, mut seed, mut noise, mut random) = (50usize, 1u64, 300u64, 0usize);
    let (mut flip, mut overlap) = (0u64, 0u64);
    let (mut min_amp, mut max_amp) = (4000u64, 54000u64);
    let mut format = "uc8".to_string();
    let (mut flights, mut seconds, mut ground, mut glitch) = (0usize, 10i64, 0u64, 0u64);
    while let Some(a) = args.next() {
        let v = args.next().expect("missing value");
        match a.as_str() {
            "--count" => count = v.parse().unwrap(),
            "--seed" => seed = v.parse().unwrap(),
            "--noise" => noise = v.parse().unwrap(),
            "--random" => random = v.parse().unwrap(),
            "--flip" => flip = v.parse().unwrap(),
            "--overlap" => overlap = v.parse().unwrap(),
            "--min-amp" => min_amp = v.parse().unwrap(),
            "--max-amp" => max_amp = v.parse().unwrap(),
            "--format" => format = v.to_ascii_lowercase(),
            "--flights" => flights = v.parse().unwrap(),
            "--seconds" => seconds = v.parse().unwrap(),
            "--ground" => ground = v.parse().unwrap(),
            "--glitch" => glitch = v.parse().unwrap(),
            _ => panic!("unknown option {a}"),
        }
    }
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);

    if flights > 0 {
        let events = simulate(&mut rng, flights, seconds, ground, glitch);
        let total =
            (seconds as usize + 1) * 2_400_000 + openradome_core::demod::TRAILING_SAMPLES + 4000;
        let mut mags: Vec<u16> = vec![0; total];
        let mut truth = String::new();
        for (t_ms, frame) in &events {
            let mut frame = frame.clone();
            if rng.pct(flip) {
                for _ in 0..1 + rng.below(2) {
                    let bit = 5 + rng.below(frame.len() as u64 * 8 - 5) as usize;
                    frame[bit / 8] ^= 0x80 >> (bit % 8);
                }
            }
            let frame = &frame;
            let start = 2000 + (*t_ms as usize) * 2400 + rng.below(2400) as usize / 8;
            let amp = (min_amp + rng.below(max_amp - min_amp + 1)) as f64;
            let burst = modulate(frame, 0, rng.below(5) as usize, amp);
            let keep = burst.len() - 50 - openradome_core::demod::TRAILING_SAMPLES;
            for (d, s) in mags[start..start + keep].iter_mut().zip(&burst[..keep]) {
                *d = d.saturating_add(*s);
            }
            truth.push_str(&format!("{t_ms} "));
            for b in frame {
                truth.push_str(&format!("{b:02X}"));
            }
            truth.push('\n');
        }
        if noise > 0 {
            for m in &mut mags {
                *m = m.saturating_add(rng.below(noise) as u16);
            }
        }
        std::fs::File::create(&out_path)
            .unwrap()
            .write_all(&to_format(&mags, &format))
            .unwrap();
        std::fs::write(format!("{out_path}.truth"), truth).unwrap();
        eprintln!(
            "wrote {out_path}: {} samples, {} frames from {flights} flights over {seconds} s ({format})",
            mags.len(),
            events.len()
        );
        return;
    }

    let mut frames: Vec<Vec<u8>> = Vec::new();
    if !std::io::stdin().is_terminal() {
        frames.extend(
            std::io::stdin()
                .lock()
                .lines()
                .map_while(Result::ok)
                .filter_map(|l| parse_hex(&l)),
        );
    }
    // a pool of aircraft so addresses repeat and the ICAO filter has something to learn
    let pool: Vec<u32> = (0..40).map(|_| (rng.next() & 0xFF_FFFF) as u32).collect();
    for _ in 0..random {
        frames.push(random_frame(&mut rng, &pool));
    }
    assert!(
        !frames.is_empty(),
        "no frames: give hex on stdin or --random N"
    );

    let mut mags: Vec<u16> = vec![0; 2000];
    let mut truth = String::new();
    let mut last_start = 0usize;
    for i in 0..count {
        // random order after the first pass so formats and aircraft interleave
        let mut f = if i < frames.len() {
            frames[i].clone()
        } else {
            frames[rng.below(frames.len() as u64) as usize].clone()
        };
        if rng.pct(flip) {
            for _ in 0..1 + rng.below(2) {
                let bit = 5 + rng.below(f.len() as u64 * 8 - 5) as usize;
                f[bit / 8] ^= 0x80 >> (bit % 8);
            }
        }
        let offset = rng.below(5) as usize;
        let amp = (min_amp + rng.below(max_amp - min_amp + 1)) as f64;
        let burst = modulate(&f, 0, offset, amp);
        let keep = burst.len() - 50 - openradome_core::demod::TRAILING_SAMPLES;

        let start = if i > 0 && rng.pct(overlap) {
            last_start + rng.below(300) as usize
        } else {
            mags.len() + 200 + rng.below(3000) as usize
        };
        if mags.len() < start + keep {
            mags.resize(start + keep, 0);
        }
        for (d, s) in mags[start..start + keep].iter_mut().zip(&burst[..keep]) {
            *d = d.saturating_add(*s);
        }
        last_start = start;
        for b in &f {
            truth.push_str(&format!("{b:02X}"));
        }
        truth.push('\n');
    }
    mags.extend(std::iter::repeat_n(
        0u16,
        2000 + openradome_core::demod::TRAILING_SAMPLES,
    ));
    if noise > 0 {
        for m in &mut mags {
            *m = m.saturating_add(rng.below(noise) as u16);
        }
    }
    std::fs::File::create(&out_path)
        .unwrap()
        .write_all(&to_format(&mags, &format))
        .unwrap();
    std::fs::write(format!("{out_path}.truth"), truth).unwrap();
    eprintln!(
        "wrote {out_path}: {} samples, {count} bursts ({format})",
        mags.len()
    );
}
