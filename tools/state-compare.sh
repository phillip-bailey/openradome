#!/bin/bash
# --write-state compatibility: upstream and openradome each take the MLAT replay and save
# their range outline at exit; then each starts (no input) from the OTHER's rangeDirs.gz.
# All four outline.json files should be identical.
set -u
cd "${CMP_DIR:-$HOME/cmp}" || exit 1
pkill -f 'net-bi-port 4[3-8]004' ; sleep 1
rm -rf st; mkdir -p st/up st/rs st/up-from-rs st/rs-from-up
OPTS="--net --net-only --quiet --lat ${LAT:?set LAT and LON to the receiver position} --lon ${LON:?} --max-range 450 --fix --write-json-every 1 --json-location-accuracy 2 --range-outline-hours 24 --net-bind-address 127.0.0.1 --net-heartbeat 60 --net-ro-port 0 --net-sbs-port 0 --net-bo-port 0 --net-ri-port 0"

# 1. both decoders take the replay and save state at exit
/usr/local/bin/readsb $OPTS --net-bi-port 43004,43104 --write-json st/up --write-state st/up/state 2>st/up.err &
P1=$!
./openradome-state $OPTS --net-bi-port 44004,44104 --write-json st/rs --write-state st/rs/state 2>st/rs.err &
P2=$!
sleep 3
python3 replay-pcap.py mlat.pcap 43104 43004 2>/dev/null & R1=$!
python3 replay-pcap.py mlat.pcap 44104 44004 2>/dev/null & R2=$!
wait $R1 $R2
sleep 17   # one more 15 s outline.json write
kill $P1 $P2; wait $P1 $P2 2>/dev/null
ls -la st/up/state/internal_state/rangeDirs.gz st/rs/state/internal_state/rangeDirs.gz

# 2. each starts from the other's file (only rangeDirs.gz: openradome writes no aircraft blobs)
mkdir -p st/up-from-rs/state/internal_state st/rs-from-up/state/internal_state
cp -p st/rs/state/internal_state/rangeDirs.gz st/up-from-rs/state/internal_state/
cp -p st/up/state/internal_state/rangeDirs.gz st/rs-from-up/state/internal_state/
/usr/local/bin/readsb $OPTS --net-bi-port 45004,45104 --write-json st/up-from-rs --write-state st/up-from-rs/state 2>st/up-from-rs.err &
P3=$!
./openradome-state $OPTS --net-bi-port 46004,46104 --write-json st/rs-from-up --write-state st/rs-from-up/state 2>st/rs-from-up.err &
P4=$!
sleep 20
kill $P3 $P4; wait $P3 $P4 2>/dev/null
grep -h "range outline\|state" st/*.err | sort | uniq -c | head
python3 - <<'PY'
import json
def pts(d): return json.load(open(f"st/{d}/outline.json"))["actualRange"]["last24h"]["points"]
up, rs, ufr, rfu = pts("up"), pts("rs"), pts("up-from-rs"), pts("rs-from-up")
print("points: upstream", len(up), "| openradome", len(rs), "| upstream from openradome's file", len(ufr), "| openradome from upstream's file", len(rfu))
print("upstream == openradome (live):           ", up == rs)
print("openradome loads upstream's file exactly:", rfu == up)
print("upstream loads openradome's file exactly:", ufr == rs)
PY
