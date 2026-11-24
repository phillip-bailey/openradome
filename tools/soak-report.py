#!/usr/bin/env python3
"""Summarise a soak.csv written by tools/soak.sh.

    tools/soak-report.py soak.csv
"""
import csv
import sys
from statistics import mean, median


def num(v):
    try:
        return float(v)
    except ValueError:
        return None


def col(rows, name):
    return [x for x in (num(r[name]) for r in rows) if x is not None]


def span(vals):
    return f"min {min(vals):.1f}  median {median(vals):.1f}  mean {mean(vals):.1f}  max {max(vals):.1f}"


rows = list(csv.DictReader(open(sys.argv[1])))
if not rows:
    sys.exit("no data")
hours = (len(rows) - 1) / 60
print(f"{len(rows)} samples, {hours:.1f} h, {rows[0]['time']} .. {rows[-1]['time']}")

inactive = [r["time"] for r in rows if r["active"] != "active"]
restarts = col(rows, "restarts")
print(f"service: {len(inactive)} samples not active" + (f" ({inactive[:5]})" if inactive else ""))
print(f"restarts: {int(restarts[0])} -> {int(restarts[-1])}" + (" (none during the soak)" if restarts[0] == restarts[-1] else ""))
pids = {r["pid"] for r in rows}
print(f"process ids seen: {len(pids)}")

cpu = col(rows, "cpu_pct")
if cpu:
    print(f"cpu % of one core: {span(cpu)}")
rss = col(rows, "rss_kb")
if rss:
    q = len(rss) // 4
    print(f"rss kB: first {rss[0]:.0f}  last {rss[-1]:.0f}  max {max(rss):.0f}  (first-quarter mean {mean(rss[:q] or rss):.0f}, last-quarter mean {mean(rss[-q:] or rss):.0f})")
thr = col(rows, "threads")
if thr:
    print(f"threads: {int(min(thr))}..{int(max(thr))}")
for name, label in [("load1", "pi load (1 min)"), ("temp_c", "pi temperature C")]:
    v = col(rows, name)
    if v:
        print(f"{label}: {span(v)}")

print()
for name, label in [
    ("msgs_1min", "messages / min"),
    ("accepted_1min", "accepted Mode S / min"),
    ("aircraft", "aircraft"),
    ("with_pos", "aircraft with position"),
    ("max_range_nm", "max range NM"),
    ("noise_db", "noise dBFS"),
    ("signal_db", "signal dBFS"),
]:
    v = col(rows, name)
    if v:
        print(f"{label:24s} {span(v)}")
dropped = col(rows, "dropped_1min")
if dropped:
    print(f"{'dropped samples':24s} total {sum(dropped):.0f} in {sum(1 for d in dropped if d > 0)} minutes")
