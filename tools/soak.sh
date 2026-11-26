#!/bin/bash
# Soak-test logger for the openradome service: one CSV line a minute.
#
#   tools/soak.sh [DIR] [JSON_DIR]      (defaults: ~/soak, /run/openradome)
#
# Run it detached and time-bounded, e.g. on the Pi:
#   setsid nohup timeout 25h bash ~/soak/soak.sh ~/soak >~/soak/soak.log 2>&1 </dev/null &
# Read it out with tools/soak-report.py DIR/soak.csv.
set -u
DIR=${1:-$HOME/soak}
JSON=${2:-/run/openradome}
mkdir -p "$DIR"
OUT=$DIR/soak.csv
[ -s "$OUT" ] || echo "time,active,restarts,pid,cpu_pct,rss_kb,threads,load1,temp_c,msgs_1min,modes_1min,accepted_1min,dropped_1min,aircraft,with_pos,max_range_nm,noise_db,signal_db" >"$OUT"

CLK=$(getconf CLK_TCK)
prev_pid=0
prev_ticks=0
prev_t=0

while true; do
  now=$(date +%s)
  active=$(systemctl is-active openradome 2>/dev/null)
  restarts=$(systemctl show openradome -p NRestarts --value 2>/dev/null)
  pid=$(systemctl show openradome -p MainPID --value 2>/dev/null)
  cpu=""
  rss=""
  thr=""
  if [ "${pid:-0}" != 0 ] && [ -r "/proc/$pid/stat" ]; then
    # fields after "pid (comm)": index 11 utime, 12 stime, 17 threads
    read -r -a f < <(sed 's/^[0-9]* ([^)]*) //' "/proc/$pid/stat")
    ticks=$((f[11] + f[12]))
    thr=${f[17]}
    rss=$(awk '/VmRSS/ {print $2}' "/proc/$pid/status")
    if [ "$pid" = "$prev_pid" ] && [ "$prev_t" != 0 ]; then
      cpu=$(awk -v d=$((ticks - prev_ticks)) -v c="$CLK" -v s=$((now - prev_t)) 'BEGIN { printf "%.1f", 100 * d / c / s }')
    fi
    prev_pid=$pid
    prev_ticks=$ticks
    prev_t=$now
  else
    prev_pid=0
  fi
  load=$(cut -d' ' -f1 /proc/loadavg)
  temp=$(vcgencmd measure_temp 2>/dev/null | tr -dc '0-9.')
  json=$(python3 - "$JSON" <<'EOF'
import json, sys
d = sys.argv[1]
try:
    s = json.load(open(d + "/stats.json"))["last1min"]
    l = s.get("local", {})
    a = json.load(open(d + "/aircraft.json"))["aircraft"]
    print(",".join(str(v) for v in [
        s.get("messages", ""), l.get("modes", ""), sum(l.get("accepted", [0])), l.get("samples_dropped", ""),
        len(a), sum("lat" in x for x in a), round(max([x.get("r_dst", 0) for x in a] or [0]), 1),
        l.get("noise", ""), l.get("signal", "")]))
except Exception:
    print(",,,,,,,,")
EOF
)
  echo "$(date -u -d @"$now" +%FT%TZ),$active,$restarts,$pid,$cpu,$rss,$thr,$load,$temp,$json" >>"$OUT"
  sleep $((60 - $(date +%s) % 60))
done
