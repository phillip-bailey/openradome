#!/usr/bin/env bash
# Run one IQ capture through upstream (Docker image openradome-upstream) and through
# this implementation, and diff the decoded messages and the receiver statistics.
#
#   tools/compare.sh capture.bin [iformat]       # iformat: UC8 (default), SC16, SC16Q11
#   EXTRA='--aggressive --modeac' tools/compare.sh ...   # flags given to both programs
#
# Build the reference first:  docker build -t openradome-upstream tools/upstream
set -euo pipefail

cap=${1:?usage: tools/compare.sh capture.bin [iformat]}
fmt=${2:-UC8}
extra=${EXTRA:-}
here=$(cd "$(dirname "$0")/.." && pwd)
bin=${OPENRADOME:-$here/target/release/openradome}
dir=$(cd "$(dirname "$cap")" && pwd)
name=$(basename "$cap")
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# upstream exits non-zero at end of file ("Abnormal exit"), which is expected here
up() { docker run --rm -v "$dir":/data openradome-upstream --device-type ifile --ifile "/data/$name" --iformat "$fmt" $extra "$@" 2>/dev/null || true; }

up --raw >"$tmp/up.raw"
"$bin" --ifile "$cap" --iformat "$fmt" $extra --raw >"$tmp/rs.raw"

# Statistics: from "Local receiver:" to the aircraft-track counts (CPU timings excluded).
stats() { sed -n '/^Local receiver:/,/aircraft tracks where only one message was seen/p' | awk 'NF' | grep -v '^ *Phase stats'; }
up --stats >"$tmp/up.stats.full"
"$bin" --ifile "$cap" --iformat "$fmt" $extra --stats >"$tmp/rs.stats.full"
stats <"$tmp/up.stats.full" >"$tmp/up.stats"
stats <"$tmp/rs.stats.full" >"$tmp/rs.stats"

status=0
echo "messages: upstream $(wc -l <"$tmp/up.raw" | tr -d ' ')  rust $(wc -l <"$tmp/rs.raw" | tr -d ' ')"
if diff -q "$tmp/up.raw" "$tmp/rs.raw" >/dev/null; then
  echo "decoded messages: IDENTICAL"
else
  echo "decoded messages: DIFFER"; diff "$tmp/up.raw" "$tmp/rs.raw" | head -20; status=1
fi
if diff -q "$tmp/up.stats" "$tmp/rs.stats" >/dev/null; then
  echo "receiver stats:   IDENTICAL"
else
  echo "receiver stats:   DIFFER"; diff "$tmp/up.stats" "$tmp/rs.stats" | head -30; status=1
fi
exit $status
