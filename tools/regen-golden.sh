#!/usr/bin/env bash
# Regenerate the golden files in testdata/ from upstream (Docker image
# rustadsb-upstream, built from tools/upstream). Only needed when a capture or the
# pinned upstream commit changes.
#
# Captures, and the flags both programs get for them (crates/rustadsb/tests/golden.rs uses
# the same list):
#   synth_uc8.bin  synthetic frames from examples/synth_iq.rs; default flags
#   flights_uc8.bin  3 s of simulated traffic from examples/synth_iq.rs:
#                  --flights 12 --seconds 3 --ground 20 --glitch 5 --flip 5 --overlap 5 --seed 7
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here/testdata"

# upstream exits non-zero at end of file ("Abnormal exit"), which is expected here
up() { docker run --rm -v "$PWD":/data rustadsb-upstream --device-type ifile "$@" 2>/dev/null || true; }

# Statistics: from "Local receiver:" to the aircraft-track counts (CPU timings excluded),
# the same range tools/compare.sh diffs.
stats() {
  sed -n '/^Local receiver:/,/aircraft tracks where only one message was seen/p' | awk 'NF' | grep -v '^ *Phase stats'
}

gen() {
  local name=$1
  shift
  up --ifile "/data/$name.bin" "$@" --raw >"$name.upstream.raw"
  up --ifile "/data/$name.bin" "$@" --stats | stats >"$name.upstream.stats"
  wc -l "$name.upstream.raw" "$name.upstream.stats"
}

gen synth_uc8
gen flights_uc8 --fix --lat 52.0 --lon 4.0 --max-range 450
