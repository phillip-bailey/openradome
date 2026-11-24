#!/usr/bin/env python3
"""Decode an aircraft.binCraft (or globe_*.binCraft) file: header and aircraft, as JSON on stdout.

Usage: bincraft.py FILE[.gz|.zst]     (the layout is upstream's struct binCraft, version 20250403)
"""
import gzip, json, struct, sys

def read(path):
    raw = open(path, 'rb').read()
    if raw[:2] == b'\x1f\x8b':                     # gzip (upstream's aircraft.binCraft is gzipped)
        raw = gzip.decompress(raw)
    elif raw[:4] == b'\x28\xb5\x2f\xfd':           # zstd frame
        try:
            from compression import zstd           # Python 3.14+
            raw = zstd.decompress(raw)
        except ImportError:
            import subprocess
            raw = subprocess.run(['zstd', '-dc'], input=raw, capture_output=True, check=True).stdout
    return raw

BITS = ['category:8', 'pos_nic:8', 'nav_modes:8', 'emergency:4', 'addrtype:4', 'airground:4', 'nav_altitude_src:4',
        'sil_type:4', 'adsb_version:4', 'adsr_version:4', 'tisb_version:4', 'nac_p:4', 'nac_v:4', 'sil:2', 'gva:2',
        'sda:2', 'nic_a:1', 'nic_c:1'] + [n + ':1' for n in (
        'nic_baro alert spi callsign_valid baro_alt_valid geom_alt_valid position_valid gs_valid ias_valid tas_valid '
        'mach_valid track_valid track_rate_valid roll_valid mag_heading_valid true_heading_valid baro_rate_valid '
        'geom_rate_valid nic_a_valid nic_c_valid nic_baro_valid nac_p_valid nac_v_valid sil_valid gva_valid sda_valid '
        'squawk_valid emergency_valid spi_valid nav_qnh_valid nav_altitude_mcp_valid nav_altitude_fms_valid '
        'nav_altitude_src_valid nav_heading_valid nav_modes_valid alert_valid wind_valid temp_valid unused_1 unused_2'
        ).split()]

def decode_header(b):
    now, size, npos, idx, s, w, n, e, msgs, rlat, rlon, ver, rate, flags = struct.unpack('<qIIIhhhhIiiIII', b[:52])
    return dict(now=now / 1000, element_size=size, aircraft_with_pos=npos, index=idx, south=s, west=w, north=n,
                east=e, messages=msgs, receiver_lat=rlat / 1e6, receiver_lon=rlon / 1e6, version=ver,
                message_rate=rate / 10, flags=flags)

def decode(b):
    f = struct.unpack('<Iiii' 'hhhh' 'HHhh' 'Hhhh' 'hhhh' 'hhhh' 'HHHH', b[:64])
    names = ('hex seen lon lat baro_rate geom_rate baro_alt geom_alt nav_altitude_mcp nav_altitude_fms nav_qnh '
             'nav_heading squawk gs mach roll track track_rate mag_heading true_heading wind_direction wind_speed '
             'oat tat tas ias pos_rc messages').split()
    d = dict(zip(names, f))
    bits = int.from_bytes(b[64:78], 'little')
    pos = 0
    for spec in BITS:
        n, w = spec.split(':'); w = int(w)
        d[n] = (bits >> pos) & ((1 << w) - 1)
        pos += w
    d['callsign'] = b[78:86].split(b'\0')[0].decode('latin1')
    d['dbFlags'], = struct.unpack('<H', b[86:88])
    d['typeCode'] = b[88:92].split(b'\0')[0].decode()
    d['registration'] = b[92:104].split(b'\0')[0].decode()
    d['receiverCount'], d['signal'], d['extraFlags'] = b[104], b[105], b[106]
    d['seen_pos'], = struct.unpack('<i', b[108:112])
    return d

def parse(raw):
    h = decode_header(raw)
    size = h['element_size']
    return h, [decode(raw[i:i + size]) for i in range(size, len(raw) - size + 1, size)]

if __name__ == '__main__':
    h, ac = parse(read(sys.argv[1]))
    json.dump({'header': h, 'aircraft': ac}, sys.stdout, indent=1)
