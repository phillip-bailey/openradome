#!/usr/bin/env python3
"""replay.py capture port : send a Beast capture to localhost:port at its original pace
(paced by the 12 MHz message timestamps)."""
import socket, sys, time
cap, port = sys.argv[1], int(sys.argv[2])
d = open(cap, "rb").read()
frames, i = [], 0
LEN = {0x31: 9, 0x32: 14, 0x33: 21, 0xe3: 8}
while i < len(d):
    if d[i] != 0x1a or i + 1 >= len(d) or d[i + 1] not in LEN:
        i += 1; continue
    n, j, got = LEN[d[i + 1]], i + 2, 0
    while got < n and j < len(d):
        j += 2 if d[j] == 0x1a else 1
        got += 1
    if got < n: break
    body = d[i + 2:j]
    ts = None
    if d[i + 1] != 0xe3:
        raw, k = [], 0
        while len(raw) < 6:
            raw.append(body[k]); k += 2 if body[k] == 0x1a else 1
        ts = int.from_bytes(bytes(raw), "big")
    frames.append((ts, d[i:j])); i = j
t0 = next(ts for ts, _ in frames if ts)  # skip zero-timestamp heartbeats
for _ in range(100):
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=2); break
    except OSError:
        time.sleep(0.1)
else:
    sys.exit("no connect")
start = time.time()
sent = 0
for ts, raw in frames:
    if ts:
        wait = (ts - t0) / 12e6 - (time.time() - start)
        if wait > 0: time.sleep(wait)
    s.sendall(raw); sent += 1
time.sleep(1)
print(f"replayed {sent} frames in {time.time()-start:.1f}s to port {port}")
