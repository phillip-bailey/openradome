#!/usr/bin/env python3
"""Replay TCP flows from a tcpdump capture into a decoder, at the original pace.

    tools/replay-pcap.py capture.pcap MLAT_PORT BEAST_PORT [--mlat-from 30104] [--beast-from 30005]
                                                          [--speed 1.0]

Every flow that went *to* port MLAT-FROM (mlat-client results) is replayed over its own
connection to localhost:MLAT_PORT, so Beast framing is kept per connection. The largest flow
that came *from* port BEAST-FROM (one client's copy of the Beast output) is replayed to
localhost:BEAST_PORT. Segments are sent at their capture-time offsets, so the two streams keep
their relative timing.

Record with, for example:
    sudo tcpdump -i lo -s 0 -w capture.pcap 'tcp dst port 30104 or tcp src port 30005'

Reads classic pcap (micro- or nanosecond), link types Ethernet, Linux SLL/SLL2 and NULL.
"""
import argparse
import socket
import struct
import sys
import time
from collections import defaultdict


def parse_pcap(path):
    """Yield (time_seconds, src, sport, dst, dport, seq, payload) for each TCP segment."""
    with open(path, "rb") as f:
        data = f.read()
    magic = data[:4]
    if magic in (b"\xd4\xc3\xb2\xa1", b"\x4d\x3c\xb2\xa1"):
        endian, nanos = "<", magic == b"\x4d\x3c\xb2\xa1"
    elif magic in (b"\xa1\xb2\xc3\xd4", b"\xa1\xb2\x3c\x4d"):
        endian, nanos = ">", magic == b"\xa1\xb2\x3c\x4d"
    else:
        sys.exit("not a classic pcap file (pcapng is not supported; use tcpdump -w)")
    linktype = struct.unpack(endian + "I", data[20:24])[0]
    pos = 24
    while pos + 16 <= len(data):
        ts_sec, ts_frac, caplen, _ = struct.unpack(endian + "IIII", data[pos : pos + 16])
        pos += 16
        pkt = data[pos : pos + caplen]
        pos += caplen
        t = ts_sec + ts_frac / (1e9 if nanos else 1e6)
        # link layer -> IPv4
        if linktype == 1:  # Ethernet
            if len(pkt) < 14 or pkt[12:14] != b"\x08\x00":
                continue
            ip = pkt[14:]
        elif linktype == 113:  # Linux cooked
            if len(pkt) < 16 or pkt[14:16] != b"\x08\x00":
                continue
            ip = pkt[16:]
        elif linktype == 276:  # Linux cooked v2
            if len(pkt) < 20 or pkt[0:2] != b"\x08\x00":
                continue
            ip = pkt[20:]
        elif linktype == 0:  # BSD loopback
            ip = pkt[4:]
        else:
            sys.exit(f"unsupported link type {linktype}")
        if len(ip) < 20 or ip[0] >> 4 != 4 or ip[9] != 6:
            continue
        ihl = (ip[0] & 0xF) * 4
        total = struct.unpack(">H", ip[2:4])[0]
        tcp = ip[ihl:total]
        if len(tcp) < 20:
            continue
        sport, dport, seq = struct.unpack(">HHI", tcp[:8])
        doff = (tcp[12] >> 4) * 4
        payload = tcp[doff:]
        if payload:
            yield t, ip[12:16], sport, ip[16:20], dport, seq, payload


def collect_flows(path):
    """Group segments by flow, dropping retransmitted bytes. Returns {flow: [(t, payload)]}."""
    flows = defaultdict(list)
    expected = {}
    for t, src, sport, dst, dport, seq, payload in parse_pcap(path):
        key = (src, sport, dst, dport)
        nxt = expected.get(key)
        if nxt is not None:
            if seq + len(payload) <= nxt:
                continue  # pure retransmission
            if seq < nxt:
                payload = payload[nxt - seq :]
                seq = nxt
        expected[key] = seq + len(payload)
        flows[key].append((t, payload))
    return flows


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("pcap")
    ap.add_argument("mlat_port", type=int)
    ap.add_argument("beast_port", type=int)
    ap.add_argument("--mlat-from", type=int, default=30104)
    ap.add_argument("--beast-from", type=int, default=30005)
    ap.add_argument("--speed", type=float, default=1.0)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument(
        "--no-beast",
        action="store_true",
        help="replay the MLAT flows only, so MLAT is the decoder's sole source",
    )
    args = ap.parse_args()

    flows = collect_flows(args.pcap)
    mlat = {k: v for k, v in flows.items() if k[3] == args.mlat_from}
    if not mlat:
        sys.exit(f"no flow to port {args.mlat_from} in the capture")
    targets = {k: args.mlat_port for k in mlat}
    beast_bytes = 0
    if not args.no_beast:
        beast = {k: v for k, v in flows.items() if k[1] == args.beast_from}
        if not beast:
            sys.exit(f"no flow from port {args.beast_from} in the capture")
        best = max(beast, key=lambda k: sum(len(p) for _, p in beast[k]))
        beast_bytes = sum(len(p) for _, p in beast[best])
        targets[best] = args.beast_port

    sockets = {}
    for k in targets:
        s = socket.create_connection((args.host, targets[k]))
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        sockets[k] = s
    events = sorted((t, k, p) for k in targets for t, p in flows[k])
    beast_note = "no Beast flow" if args.no_beast else f"1 Beast flow ({beast_bytes} bytes) -> :{args.beast_port}"
    print(
        f"{len(mlat)} MLAT flows -> :{args.mlat_port}, {beast_note},"
        f" {events[-1][0] - events[0][0]:.1f} s of capture",
        file=sys.stderr,
    )
    t0, start = events[0][0], time.monotonic()
    sent = defaultdict(int)
    for t, k, p in events:
        due = start + (t - t0) / args.speed
        wait = due - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        try:
            sockets[k].sendall(p)
            sent[k] += len(p)
        except OSError as e:
            print(f"flow {k[1]}->{k[3]}: {e}", file=sys.stderr)
            targets.pop(k, None)
    time.sleep(0.5)
    for s in sockets.values():
        s.close()
    print(f"sent {sum(sent.values())} bytes over {len(sent)} connections", file=sys.stderr)


if __name__ == "__main__":
    main()
