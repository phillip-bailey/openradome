#!/usr/bin/env python3
"""Compare two aircraft.binCraft files (upstream's and rustadsb's) aircraft by aircraft, field by field.

Usage: bincraft-compare.py UPSTREAM_FILE RUSTADSB_FILE
Time-dependent fields (seen, seen_pos, messages, signal) are compared with a tolerance, since the two files are
written at slightly different moments.
"""
import sys
sys.path.insert(0, __file__.rsplit('/', 1)[0])
import bincraft
from collections import Counter

hu, au = bincraft.parse(bincraft.read(sys.argv[1]))
hr, ar = bincraft.parse(bincraft.read(sys.argv[2]))
print('header: element_size %d/%d version %d/%d aircraft %d/%d' % (
    hu['element_size'], hr['element_size'], hu['version'], hr['version'], len(au), len(ar)))
u = {a['hex']: a for a in au}
r = {a['hex']: a for a in ar}
print('only upstream:', sorted('%06x' % h for h in set(u) - set(r)), 'only rust:', sorted('%06x' % h for h in set(r) - set(u)))
loose = {'seen': 30, 'seen_pos': 30, 'messages': 400, 'signal': 6, 'lat': 2000, 'lon': 2000}
diff = Counter(); ex = {}; n = 0
for h in sorted(set(u) & set(r)):
    for k in u[h]:
        n += 1
        a, b = u[h][k], r[h][k]
        if a != b and not (k in loose and abs(a - b) <= loose[k]):
            diff[k] += 1; ex.setdefault(k, []).append(('%06x' % h, a, b))
print('fields compared: %d | differing: %d' % (n, sum(diff.values())))
for k, c in diff.most_common():
    print('  %-22s %3d  e.g. %s' % (k, c, ex[k][:3]))
