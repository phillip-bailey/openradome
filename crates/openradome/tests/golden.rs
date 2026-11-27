//! Golden test: decode a recorded IQ capture and compare with what upstream
//! (at the commit pinned in tools/upstream/Dockerfile) produced for the
//! same file. Regenerate the golden files with tools/regen-golden.sh.

use std::path::PathBuf;
use std::process::Command;

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

fn run(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_openradome"))
        .args(args)
        .output()
        .expect("run openradome");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The recorded captures and the flags both programs get for them. Keep in step with
/// tools/regen-golden.sh.
struct Capture {
    name: &'static str,
    flags: &'static [&'static str],
}

const CAPTURES: [Capture; 3] = [
    // synthetic frames from examples/synth_iq.rs
    Capture {
        name: "synth_uc8",
        flags: &[],
    },
    // 3 s of simulated traffic: 12 flights around (52.0, 4.0), some on the ground, with bit errors,
    // overlapping replies and implausible positions (examples/synth_iq.rs --flights)
    Capture {
        name: "flights_uc8",
        flags: &[
            "--fix",
            "--lat",
            "52.0",
            "--lon",
            "4.0",
            "--max-range",
            "450",
        ],
    },
    // 3 s of 10 flights that also answer identity requests: DF5 squawks, emergency codes among
    // them (examples/synth_iq.rs --squawks)
    Capture {
        name: "squawk_uc8",
        flags: &[
            "--fix",
            "--lat",
            "52.0",
            "--lon",
            "4.0",
            "--max-range",
            "450",
        ],
    },
];

fn run_capture(cap: &Capture, extra: &[&str]) -> String {
    let path = testdata(&format!("{}.bin", cap.name));
    let mut args = vec!["--ifile", path.to_str().unwrap()];
    args.extend_from_slice(cap.flags);
    args.extend_from_slice(extra);
    run(&args)
}

#[test]
fn raw_output_matches_upstream() {
    for cap in &CAPTURES {
        let got = run_capture(cap, &["--raw"]);
        let want =
            std::fs::read_to_string(testdata(&format!("{}.upstream.raw", cap.name))).unwrap();
        assert!(
            got == want,
            "{}: decoded messages differ from upstream",
            cap.name
        );
    }
}

/// The statistics block from "Local receiver:" to the aircraft-track counts, without blank
/// lines and the "Phase stats" heading: the range tools/compare.sh and regen-golden.sh use.
fn stats_block(out: &str) -> Vec<String> {
    let mut got = Vec::new();
    let mut in_block = false;
    for line in out.lines() {
        if line.starts_with("Local receiver:") {
            in_block = true;
        }
        if in_block && !line.trim().is_empty() && line.trim() != "Phase stats" {
            got.push(line.to_string());
        }
        if line.contains("aircraft tracks where only one message was seen") {
            break;
        }
    }
    got
}

#[test]
fn receiver_stats_match_upstream() {
    for cap in &CAPTURES {
        let got = stats_block(&run_capture(cap, &["--stats"]));
        let want: Vec<String> =
            std::fs::read_to_string(testdata(&format!("{}.upstream.stats", cap.name)))
                .unwrap()
                .lines()
                .map(String::from)
                .collect();
        assert_eq!(got, want, "{}: receiver statistics differ", cap.name);
    }
}

#[test]
fn flags_change_the_result_like_upstream() {
    let cap = testdata("synth_uc8.bin");
    let fixed = run(&["--ifile", cap.to_str().unwrap(), "--raw"])
        .lines()
        .count();
    let nofix = run(&["--ifile", cap.to_str().unwrap(), "--raw", "--no-fix"])
        .lines()
        .count();
    assert!(
        nofix < fixed,
        "--no-fix should accept fewer messages ({nofix} vs {fixed})"
    );
}

#[test]
fn missing_input_is_an_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_openradome"))
        .args(["--ifile", "/nonexistent/capture.bin", "--raw"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("could not open"));
}

/// One aircraft record's fields that depend on when the file was written, not on what was decoded:
/// upstream read the capture in real time (`--throttle`), openradome at full speed.
/// `seen` (bytes 4..8) and `seen_pos` (108..112), in tenths of a second before "now".
const BINCRAFT_TIMED: [std::ops::Range<usize>; 2] = [4..8, 108..112];

/// The header's timed fields: `now` (0..8), aircraft with a position (12..16; upstream counts them
/// every few seconds, so after a 4 s run it still says 0) and the message rate (44..48).
const BINCRAFT_HEADER_TIMED: [std::ops::Range<usize>; 3] = [0..8, 12..16, 44..48];

#[test]
fn aircraft_bincraft_matches_upstream_field_by_field() {
    for cap in &CAPTURES[1..] {
        bincraft_matches(cap);
    }
}

fn bincraft_matches(cap: &Capture) {
    const SIZE: usize = 112;
    let dir = std::env::temp_dir().join(format!(
        "openradome-golden-bincraft-{}-{}",
        cap.name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    run_capture(cap, &["--write-json", dir.to_str().unwrap(), "--quiet"]);
    let packed = std::fs::read(dir.join("aircraft.binCraft.zst")).unwrap();
    let mut got = Vec::new();
    std::io::Read::read_to_end(
        &mut ruzstd::decoding::StreamingDecoder::new(packed.as_slice()).unwrap(),
        &mut got,
    )
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let want = std::fs::read(testdata(&format!("{}.upstream.binCraft", cap.name))).unwrap();

    let masked = |rec: &[u8], timed: &[std::ops::Range<usize>]| {
        let mut r = rec.to_vec();
        for t in timed {
            r[t.clone()].fill(0);
        }
        r
    };
    assert_eq!(
        masked(&got[..SIZE], &BINCRAFT_HEADER_TIMED),
        masked(&want[..SIZE], &BINCRAFT_HEADER_TIMED),
        "header (element size, tile, message count, receiver position, version, flags)"
    );
    let by_hex = |file: &[u8]| -> std::collections::BTreeMap<u32, Vec<u8>> {
        file[SIZE..]
            .as_chunks::<SIZE>()
            .0
            .iter()
            .map(|r| (u32::from_le_bytes(r[..4].try_into().unwrap()), r.to_vec()))
            .collect()
    };
    let (got, want) = (by_hex(&got), by_hex(&want));
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>(),
        "the same aircraft"
    );
    assert!(want.len() >= 8, "{}: the capture's flights", cap.name);
    for (hex, w) in &want {
        let (g, w) = (
            masked(&got[hex], &BINCRAFT_TIMED),
            masked(w, &BINCRAFT_TIMED),
        );
        let differ: Vec<usize> = (0..SIZE).filter(|&i| g[i] != w[i]).collect();
        assert!(
            differ.is_empty(),
            "{}: {hex:06x}: bytes {differ:?} differ (see tools/bincraft.py for the layout)",
            cap.name
        );
    }
}
