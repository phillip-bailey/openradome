//! Fuzz tests: random and mutated input through every decoder that takes it from outside (radio
//! frames, IQ samples, the Beast and raw network formats, API queries), then through the tracker
//! and every output format. Passing means no panic, valid JSON out, and memory that stays bounded.
//!
//! Plain tests on the stable toolchain, with a built-in generator, so they run in CI on every
//! change. A seed and an iteration count can be given for a longer hunt:
//!
//! ```sh
//! FUZZ_ITERS=5000000 FUZZ_SEED=42 cargo test --release -p openradome-core --test fuzz
//! ```
//!
//! Random frames almost never pass the CRC, so the generator mostly builds frames with a valid
//! CRC (anyone with a transmitter can do the same) or flips a few bits of real ones: that is what
//! reaches the decoding behind the CRC check.

use openradome_core::api;
use openradome_core::beast::{BeastFrame, BeastParser, message_from_wire, parse_raw_line};
use openradome_core::bincraft;
use openradome_core::convert::InputFormat;
use openradome_core::crc::checksum;
use openradome_core::demod::Demodulator;
use openradome_core::json;
use openradome_core::mode_s::{Decoder, ModesMessage};
use openradome_core::output::{beast_frame, raw_line};
use openradome_core::pipeline::IqPipeline;
use openradome_core::sbs::{SbsOptions, sbs_line};
use openradome_core::synth;
use openradome_core::track::{MAX_AIRCRAFT, RangeOutline, Tracker, TrackerConfig};

/// xorshift64*: small, fast and the same everywhere, so a failing seed can be replayed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    fn fill(&mut self, b: &mut [u8]) {
        for x in b {
            *x = self.byte();
        }
    }
}

fn iterations(default: u64) -> u64 {
    std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn seed() -> u64 {
    std::env::var("FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0x0A0D_5B00)
}

/// Real frames to mutate (DF17 identification and airborne positions, from testdata/frames.txt).
const REAL: [&str; 4] = [
    "8D4840D6202CC371C32CE0576098",
    "8D40621D58C382D690C8AC2863A7",
    "8D40621D58C386435CC412692AD6",
    "8D485020994409940838175B284F",
];

/// Real Comm-B replies (DF20/21), two of each format the decoder tells apart, from a receiver's
/// feed: mutating them reaches the register decoders' plausibility scoring, which random payloads
/// almost never pass.
const COMMB: [&str; 20] = [
    "A0000C3620304631C838201D29FD", // 2,0 aircraft identification
    "A0001838200834F2E76C20E552AC",
    "A800199C955A5B28A2C45C660F51", // ambiguous
    "A0000CB7955A5B28A2CC5E4DCE41",
    "A0000C3B10030A80F500006F7DF2", // 1,0 data link capability
    "A800083410030A80B50000056FAA",
    "A800139C00000000000000282272", // empty
    "A0001838000000000000000758FE",
    "A00017B0FE81C300000000DAF1A5", // 1,7 GICB capability
    "A0000DB0FA8DE300000000529E3B",
    "A8000834E1FA452C3E1FC74C6C6B", // 6,0 heading and speed
    "A0001291E2DA332D7E0FC495C718",
    "A000183858C385F3A453F5DE1474", // 4,4 meteorological routine
    "A800121558BF05EB2DA47E8BF4BE",
    "A0001718FFFDF934E004F097990E", // 5,0 track and turn
    "A80005B081BC3F316024E030F567",
    "A8000834800000003FFCD475447A", // unknown
    "A000103F1E810300000000043F55",
    "A00010939F400030AA00006EC3E6", // 4,0 vertical intention
    "A0001838CA380030A80000DDA9EC",
];

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// Make the parity field right: zero for DF11/17/18 (address in the payload), the address XORed
/// in for the address/parity formats.
fn set_parity(frame: &mut [u8], xor: u32) {
    let n = frame.len();
    frame[n - 3..].fill(0);
    let p = checksum(frame) ^ xor;
    frame[n - 3] = (p >> 16) as u8;
    frame[n - 2] = (p >> 8) as u8;
    frame[n - 1] = p as u8;
}

/// One frame as an attacker or a noisy channel might send it.
fn frame(rng: &mut Rng, pool: &[u32]) -> Vec<u8> {
    let addr = if rng.chance(40) {
        rng.next() as u32 & 0xFF_FFFF // a new address: grows the table
    } else {
        pool[rng.below(pool.len() as u64) as usize]
    };
    match rng.below(10) {
        // extended squitter with a valid CRC and any type code: what reaches the most decoding
        0..=3 => {
            let mut f = vec![0u8; 14];
            rng.fill(&mut f);
            let df = if rng.chance(4) { 18 } else { 17 };
            f[0] = (df << 3) | (rng.byte() & 7);
            f[1..4].copy_from_slice(&addr.to_be_bytes()[1..]);
            if rng.chance(3) {
                // a valid-looking airborne position or velocity more often than chance would
                f[4] = [0x58, 0x60, 0x99, 0x20, 0xE1, 0xEA, 0xF8][rng.below(7) as usize]
                    | (rng.byte() & 7);
            }
            set_parity(&mut f, 0);
            f
        }
        // address/parity replies (surveillance, Comm-B, ACAS) for an address heard before
        4..=5 => {
            let df = [0u8, 4, 5, 16, 20, 21][rng.below(6) as usize];
            let mut f = vec![0u8; if df >= 16 { 14 } else { 7 }];
            rng.fill(&mut f);
            f[0] = (df << 3) | (rng.byte() & 7);
            set_parity(&mut f, addr);
            f
        }
        // all-call reply
        6 => {
            let mut f = vec![0u8; 7];
            rng.fill(&mut f);
            f[0] = (11 << 3) | (rng.byte() & 7);
            f[1..4].copy_from_slice(&addr.to_be_bytes()[1..]);
            set_parity(
                &mut f,
                if rng.chance(2) {
                    0
                } else {
                    rng.below(80) as u32
                },
            );
            f
        }
        // a real frame with a few bits flipped, and its CRC fixed half the time
        7..=8 => {
            let mut f = hex(REAL[rng.below(REAL.len() as u64) as usize]);
            for _ in 0..=rng.below(3) {
                let bit = rng.below(f.len() as u64 * 8) as usize;
                f[bit / 8] ^= 0x80 >> (bit % 8);
            }
            if rng.chance(2) {
                set_parity(&mut f, 0);
            }
            f
        }
        // noise
        _ => {
            let mut f = vec![0u8; [2, 7, 14][rng.below(3) as usize]];
            rng.fill(&mut f);
            f
        }
    }
}

fn tracker(traces: bool) -> Tracker {
    let mut cfg = TrackerConfig::new(Some(52.0), Some(4.0), Some(300.0));
    cfg.write_traces = traces;
    cfg.globe_index = traces;
    let mut t = Tracker::new(cfg);
    t.outline = Some(RangeOutline::new(24 * 3_600_000));
    t
}

/// The outputs built from a tracker hold their shape whatever went in.
fn check_outputs(t: &Tracker, now: i64, rng: &mut Rng) {
    let doc = json::aircraft_json(t, now);
    let v: serde_json::Value = serde_json::from_str(&doc).unwrap_or_else(|e| {
        panic!(
            "aircraft.json is not JSON ({e}): {}",
            &doc[..doc.len().min(2000)]
        )
    });
    assert!(v["aircraft"].is_array());
    if let Some(o) = &t.outline {
        let out = json::outline_json(o);
        serde_json::from_str::<serde_json::Value>(&out).expect("outline.json is JSON");
    }
    let bin = bincraft::aircraft_bin(t, now, 123.4, false);
    assert_eq!(bin.len() % bincraft::RECORD_SIZE, 0);
    for a in t.active() {
        let _ = json::aircraft_object(t, a, now);
        let _ = json::trace_details(&t.cfg, a, now);
    }
    // the API on the same table, with a query of any shape
    let snap = api::Snapshot::build(t, now, 12.5);
    for _ in 0..4 {
        let q = random_query(rng);
        if let Some(parsed) = api::parse_query(&q) {
            let body = api::reply(&snap, &parsed, 0.1);
            serde_json::from_slice::<serde_json::Value>(&body)
                .unwrap_or_else(|e| panic!("API reply to {q:?} is not JSON: {e}"));
        }
    }
}

fn random_query(rng: &mut Rng) -> String {
    const KEYS: [&str; 16] = [
        "box",
        "circle",
        "closest",
        "find_hex",
        "find_callsign",
        "find_reg",
        "find_type",
        "find_squawk",
        "all",
        "all_with_pos",
        "filter_with_pos",
        "above_alt_baro",
        "below_alt_baro",
        "filter_callsign_exact",
        "filter_squawk",
        "jv2",
    ];
    const VALUES: [&str; 14] = [
        "52,4,10",
        "-91,91,-181,181",
        "nan,1,2",
        "inf",
        "1e308,1e308,1e308",
        "-0",
        "",
        "4840d6,40621d,~abcdef",
        "KLM1023",
        ",,,,",
        "99999999999999999999",
        "7700",
        "a,b",
        "52.0,4.0,-5",
    ];
    let mut q = String::new();
    for i in 0..=rng.below(4) {
        if i > 0 {
            q.push('&');
        }
        q.push_str(KEYS[rng.below(KEYS.len() as u64) as usize]);
        if rng.below(5) != 0 {
            q.push('=');
            if rng.chance(4) {
                for _ in 0..rng.below(24) {
                    q.push(char::from(
                        b" ,.-+e0123456789abcdefnai~&=%"[rng.below(29) as usize],
                    ));
                }
            } else {
                q.push_str(VALUES[rng.below(VALUES.len() as u64) as usize]);
            }
        }
    }
    q
}

/// Frames as they arrive over the network: decoded, tracked and written out in every format.
fn fuzz_frames(traces: bool, iters: u64, seed: u64) {
    let mut rng = Rng::new(seed);
    let mut decoder = Decoder::new(1);
    let mut t = tracker(traces);
    let pool: Vec<u32> = (0..64).map(|_| rng.next() as u32 & 0xFF_FFFF).collect();
    let mut now: i64 = 1_700_000_000_000;
    let opts = SbsOptions {
        use_gnss: false,
        extra_fields: true,
    };
    for i in 0..iters {
        // time mostly runs forward; now and then the clock steps back a little
        now += rng.below(20) as i64;
        if rng.chance(50_000) {
            now -= 5_000;
        }
        let data = frame(&mut rng, &pool);
        let timestamp = match rng.below(8) {
            0 => 0xFF00_4D4C_4154, // the MLAT magic timestamp
            1 => rng.next() as i64 & 0xFFFF_FFFF_FFFF,
            _ => (now * 12_000) & 0xFFFF_FFFF_FFFF,
        };
        let Some(mut mm) = message_from_wire(
            &mut decoder,
            &data,
            timestamp,
            Some(rng.byte()),
            now,
            rng.below(4),
            true,
        ) else {
            continue;
        };
        t.update_from_message(&mut mm);
        let _ = raw_line(&mm, true, rng.chance(2));
        let _ = beast_frame(&mm, rng.chance(2));
        if let Some(line) = sbs_line(&mm, mm.addr, Some(rng.below(400) as i32 - 200), now, opts) {
            assert!(
                line.ends_with("\r\n") && line.matches("\r\n").count() == 1,
                "{line:?}"
            );
        }
        if i % 2_000 == 0 {
            t.periodic(now);
            decoder.icao.expire();
        }
        if i % 20_000 == 0 {
            check_outputs(&t, now, &mut rng);
        }
        assert!(t.len() <= MAX_AIRCRAFT);
    }
    t.periodic(now);
    check_outputs(&t, now, &mut rng);
}

#[test]
fn frames_decode_track_and_print_without_a_panic() {
    fuzz_frames(false, iterations(150_000), seed());
}

#[test]
fn frames_with_traces_and_the_globe_index() {
    fuzz_frames(true, iterations(150_000) / 2, seed() ^ 0x7ACE);
}

/// A flood of new addresses stops at the table's limit instead of eating memory.
#[test]
fn the_aircraft_table_has_a_limit() {
    let mut decoder = Decoder::new(1);
    let mut t = tracker(false);
    let mut rng = Rng::new(seed() ^ 0xF100D);
    let now = 1_700_000_000_000;
    for i in 0..(MAX_AIRCRAFT as u32 + 5_000) {
        let mut f = vec![0u8; 14];
        rng.fill(&mut f);
        f[0] = 17 << 3 | 5;
        f[1..4].copy_from_slice(&i.to_be_bytes()[1..]);
        f[4] = 0x20; // identification
        set_parity(&mut f, 0);
        if let Some(mut mm) = message_from_wire(&mut decoder, &f, 0, Some(100), now, 0, false) {
            t.update_from_message(&mut mm);
        }
    }
    assert_eq!(t.len(), MAX_AIRCRAFT);
    assert_eq!(t.refused_table_full, 5_000);
}

/// The Beast stream parser on any bytes, in any chunks: frames come out whole, and what is held
/// over between chunks stays small.
#[test]
fn beast_streams_of_any_bytes() {
    let mut rng = Rng::new(seed() ^ 0xBEA5);
    let mut decoder = Decoder::new(1);
    let mut t = tracker(false);
    let mut p = BeastParser::new();
    let real: Vec<Vec<u8>> = REAL.iter().map(|s| hex(s)).collect();
    let mut stream = Vec::new();
    for round in 0..iterations(150_000) / 10 {
        stream.clear();
        for _ in 0..rng.below(64) {
            match rng.below(4) {
                // a well-formed frame (escaping any 0x1a), sometimes cut short or corrupted
                0 | 1 => {
                    let msg = &real[rng.below(real.len() as u64) as usize];
                    let kind = [b'1', b'2', b'3', b'4', 0xe3][rng.below(5) as usize];
                    let mut body = vec![0u8; 7];
                    rng.fill(&mut body);
                    body.extend_from_slice(msg);
                    stream.push(0x1a);
                    stream.push(kind);
                    for &b in &body {
                        stream.push(b);
                        if b == 0x1a {
                            stream.push(0x1a);
                        }
                    }
                    if rng.chance(5) {
                        let cut = rng.below(stream.len() as u64) as usize;
                        stream.truncate(cut);
                    }
                }
                // escapes and markers in odd places
                2 => {
                    for _ in 0..rng.below(12) {
                        stream.push(
                            [0x1a, b'1', b'2', b'3', b'4', 0xe3, 0x00][rng.below(7) as usize],
                        );
                    }
                }
                _ => {
                    for _ in 0..rng.below(40) {
                        stream.push(rng.byte());
                    }
                }
            }
        }
        // in random chunks, as TCP delivers it
        let mut at = 0;
        while at < stream.len() {
            let n = (1 + rng.below(64) as usize).min(stream.len() - at);
            for f in p.feed(&stream[at..at + n]) {
                if let BeastFrame::Message {
                    data,
                    timestamp,
                    signal,
                } = f
                {
                    assert!(matches!(data.len(), 2 | 7 | 14), "{} bytes", data.len());
                    if let Some(mut mm) = message_from_wire(
                        &mut decoder,
                        &data,
                        timestamp,
                        Some(signal),
                        round as i64,
                        0,
                        true,
                    ) {
                        t.update_from_message(&mut mm);
                    }
                }
            }
            at += n;
        }
    }
    // a valid frame still comes through after all that
    let mut good = vec![0x1a, b'3', 0, 0, 0, 0, 0, 1, 50];
    good.extend_from_slice(&real[0]);
    let frames = p.feed(&good);
    assert!(
        frames
            .iter()
            .any(|f| matches!(f, BeastFrame::Message { data, .. } if *data == real[0])),
        "the parser recovers"
    );
}

/// The raw text format: anything that is not exactly a frame is refused, without a panic.
#[test]
fn raw_lines_of_any_text() {
    let mut rng = Rng::new(seed() ^ 0x0AA);
    const PIECES: [&str; 12] = [
        "*",
        "@",
        ";",
        "8D4840D6202CC371C32CE0576098",
        "0123456789AB",
        "\u{FFFD}",
        "é",
        "+",
        "-",
        " ",
        "\t",
        "ZZ",
    ];
    for _ in 0..iterations(150_000) {
        let mut line = String::new();
        for _ in 0..rng.below(8) {
            if rng.chance(3) {
                line.push(char::from_u32(rng.below(0x11_0000) as u32).unwrap_or('?'));
            } else {
                line.push_str(PIECES[rng.below(PIECES.len() as u64) as usize]);
            }
        }
        if let Some(r) = parse_raw_line(&line) {
            assert!(matches!(r.data.len(), 2 | 7 | 14), "{line:?}");
            let t = line.trim();
            assert!(t.is_ascii() && t.ends_with(';'), "accepted {line:?}");
        }
    }
    assert!(parse_raw_line("*8D4840D6202CC371C32CE0576098;").is_some());
}

/// IQ samples from the radio: noise, clipping, and real frames buried in it.
#[test]
fn iq_samples_of_any_shape() {
    let mut rng = Rng::new(seed() ^ 0x1F0);
    let mut pipe = IqPipeline::new(
        Demodulator::new(Decoder::new(1)),
        InputFormat::Uc8,
        16 * 1024,
    );
    let mut decoded = 0usize;
    let real: Vec<Vec<u8>> = REAL.iter().map(|s| hex(s)).collect();
    let blocks = (iterations(150_000) / 1_500).max(20);
    for _ in 0..blocks {
        let mut buf = vec![0u8; 8 * 1024 + rng.below(8 * 1024) as usize];
        match rng.below(3) {
            0 => rng.fill(&mut buf),
            1 => {
                let level = rng.byte();
                buf.iter_mut()
                    .for_each(|b| *b = if rng.chance(2) { level } else { 255 - level });
            }
            _ => {
                // a real frame in noise, at any amplitude and offset
                let msg = &real[rng.below(real.len() as u64) as usize];
                let amp = 2_000.0 + rng.below(63_000) as f64; // pulse height, full scale 65535
                let mags = synth::modulate(
                    msg,
                    50 + rng.below(200) as usize,
                    rng.below(5) as usize,
                    amp,
                );
                let iq = synth::to_uc8(&mags);
                rng.fill(&mut buf);
                for b in buf.iter_mut() {
                    *b = 127u8.wrapping_add(*b >> 5).wrapping_sub(4);
                }
                // an even offset: the bytes are I/Q pairs
                let at =
                    rng.below((buf.len().saturating_sub(iq.len()) as u64 / 2).max(1)) as usize * 2;
                for (i, &v) in iq.iter().enumerate() {
                    if let Some(b) = buf.get_mut(at + i) {
                        *b = v;
                    }
                }
            }
        }
        // in uneven pieces, as the USB transfers come
        let mut at = 0;
        while at < buf.len() {
            let n = (1 + rng.below(4096) as usize).min(buf.len() - at);
            pipe.push(&buf[at..at + n], &mut |msgs: Vec<ModesMessage>| {
                decoded += msgs.len()
            });
            at += n;
        }
    }
    pipe.finish(&mut |msgs: Vec<ModesMessage>| decoded += msgs.len());
    assert!(decoded > 0, "frames in the noise are still found");
}

/// Comm-B replies from aircraft that are being tracked, mutated a few bits at a time with their
/// parity kept right: every register decoder and what the tracker does with its answer (heading,
/// speeds, selected altitude, wind and temperature).
#[test]
fn comm_b_registers_of_any_content() {
    let mut rng = Rng::new(seed() ^ 0xC0BB);
    let mut decoder = Decoder::new(1);
    let mut t = tracker(false);
    let seeds: Vec<(Vec<u8>, u32)> = COMMB
        .iter()
        .map(|s| {
            let f = hex(s);
            let addr = checksum(&f); // address/parity: the remainder is the address
            (f, addr)
        })
        .collect();
    let mut now: i64 = 1_700_000_000_000;
    // each aircraft announces itself with a squitter first, as on the air
    for (_, addr) in &seeds {
        for _ in 0..2 {
            let mut f = vec![
                0x8D, 0, 0, 0, 0x20, 0x2C, 0xC3, 0x71, 0xC3, 0x2C, 0xE0, 0, 0, 0,
            ];
            f[1..4].copy_from_slice(&addr.to_be_bytes()[1..]);
            set_parity(&mut f, 0);
            now += 10;
            if let Some(mut mm) = message_from_wire(&mut decoder, &f, 0, Some(100), now, 0, false) {
                t.update_from_message(&mut mm);
            }
        }
    }
    for i in 0..iterations(150_000) {
        now += rng.below(30) as i64;
        let (seed_frame, own) = &seeds[rng.below(seeds.len() as u64) as usize];
        let mut f = seed_frame.clone();
        // the payload of one register, sometimes sent by another aircraft
        let addr = if rng.chance(4) {
            seeds[rng.below(seeds.len() as u64) as usize].1
        } else {
            *own
        };
        for _ in 0..rng.below(5) {
            let bit = 32 + rng.below(56) as usize; // inside MB
            f[bit / 8] ^= 0x80 >> (bit % 8);
        }
        if rng.chance(20) {
            f[0] ^= 1 << rng.below(3); // the DR/UM bits that make the decoder distrust a reply
        }
        set_parity(&mut f, addr);
        if let Some(mut mm) =
            message_from_wire(&mut decoder, &f, 0, Some(rng.byte()), now, 0, false)
        {
            t.update_from_message(&mut mm);
        }
        if i % 25_000 == 0 {
            t.periodic(now);
            check_outputs(&t, now, &mut rng);
        }
    }
    check_outputs(&t, now, &mut rng);
}
