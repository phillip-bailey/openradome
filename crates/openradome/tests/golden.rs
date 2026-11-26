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

const CAPTURES: [Capture; 2] = [
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
