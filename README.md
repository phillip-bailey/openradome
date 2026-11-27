# OpenRadome

**Website: [phillip-bailey.github.io/openradome](https://phillip-bailey.github.io/openradome/)** · [Releases](https://github.com/phillip-bailey/openradome/releases) · [Changelog](CHANGELOG.md) · [Security](SECURITY.md)

## What is ADS-B?

**ADS-B** (Automatic Dependent Surveillance–Broadcast) is how aircraft tell the world where they are. An equipped
aircraft works out its own position by satellite navigation (GPS) and broadcasts it by radio on 1090 MHz, about
twice a second, together with its identity, callsign, altitude, speed and heading. Nothing has to ask for it: the
report is *automatic*, it *depends* on the aircraft's own navigation rather than on a radar measuring it, and it is
*broadcast* to anyone listening.

### Why it is used

- **Air traffic control sees more, more precisely.** A radar sweeps every 4 to 12 seconds and grows less accurate
  with distance; ADS-B gives a GPS-accurate position twice a second. A receiving station costs a small fraction of a
  radar, so coverage reaches places radar does not: remote regions, mountains, and (through satellite receivers)
  the oceans.
- **Aircraft see each other.** Other aircraft receive the same broadcasts, so pilots and on-board traffic displays
  know what is nearby, not only what the ground tells them.
- **It is required.** Since 2020, most aircraft in United States controlled airspace and larger aircraft in Europe
  must broadcast ADS-B, and many other countries have similar rules. Nearly every airliner transmits it.
- **Anyone can receive it.** The signal is not encrypted. A USB radio costing about $50 and an antenna on a
  Raspberry Pi pick up aircraft 200 to 400 km away. Tens of thousands of volunteer stations built this way feed
  flight-tracking services (Flightradar24, FlightAware, ADS-B Exchange, adsb.lol, adsb.fi, airplanes.live and
  others), whose maps are used by the public, journalists, researchers studying emissions and noise, airports and
  accident investigators.

## What OpenRadome does

OpenRadome is the software at the heart of such a receiving station. It turns the raw radio signal from the USB radio
into a live list of aircraft with their positions, feeds the station's own map, and forwards the data to
flight-tracking networks such as Flightradar24, FlightAware, ADS-B Exchange and adsb.lol. It is a memory-safe
rewrite, in Rust, of readsb, the decoder most stations run today. It takes the same options and produces the same
outputs, so it can replace readsb without changing the rest of a station. It runs in service on a Raspberry Pi, and
its output is checked against readsb's on recorded and live data.

## Overview

An ADS-B and Mode S decoder and aggregator feed client written in Rust: a from-scratch reimplementation of
[wiedehopf/readsb](https://github.com/wiedehopf/readsb) (v3.16.x).

It reads an RTL-SDR dongle (or an IQ file, or Beast/raw data over TCP), decodes Mode S, ADS-B, Comm-B and Mode A/C,
tracks aircraft, and serves the same outputs upstream does: Beast, raw and SBS over TCP, `beast_reduce` feeds for the
community aggregators, and the JSON files tar1090 and other tools read. Command-line flags, wire formats and JSON
layouts follow upstream, so it drops into an existing setup unchanged.

It is not a fork: no shared history, its own crate layout and design. Upstream is the reference for behaviour
and output, and the decoder logic is ported from its GPLv3 code, hence the licence (see [Licence](#licence)).

**Status:** in service on a Raspberry Pi 2 as the decoder and as the network feed clients. On recorded and live
data it produces the same output as upstream (see [Verification](#verification-against-upstream)). It implements
the parts of upstream a feeder station uses, not the whole of it (see [Not implemented](#not-implemented)).

## The problem it solves

Aircraft constantly announce themselves by radio on 1090 MHz. Their transponders answer radar interrogations
(Mode S, Mode A/C), and most airliners also broadcast their own GPS position, altitude, speed, heading and callsign
twice a second without being asked (ADS-B). Anyone can receive this: a $50 RTL-SDR USB dongle and an antenna on a
Raspberry Pi will hear aircraft 200-400 km away. Thousands of people run such stations, for their own map and to
feed community flight-tracking networks (adsb.lol, adsb.fi, ADS-B Exchange, airplanes.live, Flightradar24,
FlightAware and others), which together build the worldwide live picture of air traffic, including the
military and private aircraft that commercial services hide.

The dongle only delivers raw radio samples: 2.4 million per second, almost all of them noise. Getting from there to
"this aircraft is here, at this altitude, going this way" takes a chain of work, and that chain is what this
program does:

1. **Find the messages in the noise.** Detect the 8 µs preamble of each reply in the sample stream, recover the
   56 or 112 bits behind it at the right sub-sample timing, and check them against their CRC, repairing a damaged
   bit where it is safe.
2. **Decode them.** Turn the bits into address, altitude, squawk, callsign, velocity, the compact position encoding
   (CPR), and the Comm-B replies that carry airspeed, Mach, roll, selected altitude, wind and temperature.
3. **Track the aircraft.** One message rarely says everything. Combine many over time per aircraft: pair two
   position halves into a position, reject impossible jumps and corrupt data, decide which source to trust, expire
   aircraft that have gone quiet.
4. **Share it.** Serve the results to the station's own map (tar1090), to monitoring, and to each network in the
   form it expects: full or paced Beast streams with the receiver's 12 MHz timestamps, the station's identity, and
   the timing data multilateration (MLAT) needs to locate aircraft that don't broadcast their position. Positions
   computed by MLAT come back in and are merged with everything else.

Upstream is the standard program for this chain and the one most feeder setups run, in C. OpenRadome does
the same job, with the same flags and outputs, in safe Rust. It runs as both halves of a typical station: the
decoder attached to the dongle, and the small per-network feed clients that forward its output.

## Why a Rust version

- **Memory-safe where it faces the outside world.** It parses Beast and raw frames from network clients, aggregator
  pings and USB data all day. The whole program is safe Rust (`unsafe_code = "forbid"`).
- **No C dependencies on the radio side.** The dongle is driven by a pure-Rust USB driver
  ([rtlsdr-nusb](https://crates.io/crates/rtlsdr-nusb)): no librtlsdr, no libusb. Cross-compile one binary, copy it.
- **Observable and self-healing.** A silent-dongle watchdog, bounded shutdown on a wedged USB device, and counted,
  logged losses (dropped USB blocks, slow output clients, full input queues) instead of silent ones.
- **Pinned by tests.** Golden tests decode synthetic RF captures and must match upstream byte for byte; real recordings were checked
  the same way (see [Verification](#verification-against-upstream)).

## Features

**Signal and decoding**
- 2.4 MS/s demodulator (upstream's `demod_2400`): preamble detection, five sub-sample phases, scoring, 12 MHz
  timestamps, signal and noise power.
- IQ formats UC8, SC16 and SC16Q11.
- CRC-24 with 1-bit (`--fix`, default) or 2-bit (`--aggressive`) repair, and the recently-seen ICAO filter.
- Mode S DF0-24 and all ADS-B message types in use: identification, airborne and surface position, velocity,
  status, target state, operational status.
- Comm-B (DF20/21) BDS 1,0 1,7 2,0 3,0 4,0 4,4 5,0 6,0 with upstream's register scoring: IAS, Mach, TAS, roll, track
  rate, magnetic heading, selected altitude and QNH, wind and temperature reports.
- Mode A/C (`--modeac`).

**Tracking**
- Per-field validity and source priority (ADS-B > ADS-R > TIS-B > MLAT > Mode S), as upstream.
- CPR global, aircraft-relative and receiver-relative decoding, speed and range checks, position reliability,
  altitude plausibility, NIC/Rc, expiry.
- True heading from magnetic heading (WMM2025), wind and temperature estimates.
- MLAT results from mlat-client: `mlat` type and field flags, upstream's acceptance rules, altitude ignored except for
  anonymous addresses.

**Inputs**
- RTL-SDR (`--device-type rtlsdr`; R820T/R820T2/R860 and R828D tuners), IQ file (`--ifile`, `-` for stdin),
  Beast and raw TCP (listen ports and `--net-connector`).

**Outputs**
- Beast, raw and SBS (BaseStation) TCP servers and connectors.
- `beast_reduce_out` / `beast_reduce_plus_out` for aggregators: messages paced per field exactly as upstream
  (`--net-beast-reduce-interval`), station UUID (`--uuid`, `--uuid-file`, `uuid=`), ping answers.
- Connector fallback hosts (`host,port,proto,host2,port2`) and `silent_fail`.
- Upstream's connection hygiene: a connection attempt gives up after 5 s; a peer to which nothing could be
  written for 5 s (after the first 15 s) is dropped and the connector reconnects; an input connector whose
  server sends neither data nor a heartbeat for two `--net-heartbeat` intervals reconnects (or tries its
  fallback); TCP keepalive (20 s idle, 3 probes 2 s apart) on every connection.
- `--write-json`: `aircraft.json`, `receiver.json`, `stats.json` (1, 5 and 15 minute and total windows) and
  `outline.json` (range outline per direction over `--range-outline-hours`).
- `--write-state <dir>`: the range outline survives a restart. It is saved every 5 minutes and at exit (with
  `--write-state-only-on-exit`, every 6 hours and at exit) in `<dir>/internal_state/rangeDirs.gz`, in upstream's
  own format, so OpenRadome and upstream each read the other's file. Time slices that passed while nothing ran are
  cleared on load, so an old outline does not come back.
- Upstream's command file: `echo resetRangeOutline > <json dir>/setGain` clears the range outline (after moving the
  antenna, say). The decoder looks for the file every second, deletes it, rewrites `outline.json` and saves the
  empty state at once. Upstream's other commands in that file (gain, `setLatLon`) are logged and ignored. An empty
  file is left for the next look (the writer may not have written yet); anything but a regular file (a FIFO, a
  directory) is reported once and never opened; a file the decoder cannot delete is reported and not run, so a
  command never runs twice. The decoder's timers (this one, the JSON files, the 10 s `stats.json` buckets, saved
  state) fire at once if the clock is set back, rather than waiting out the step. Like upstream, `stats.json`
  starts afresh at each restart: its counters are not saved with the state.
- `aircraft.binCraft.zst` (always; `.binCraft` gzipped with `--write-binCraft-old`): upstream's compact binary
  aircraft list, version 20250403. `--write-json-gzip` also writes `aircraft.json.gz`;
  `--write-json-binCraft-only 2` skips `aircraft.json`. `receiver.json` advertises `binCraft` and `zstd`.
- **API server** (`--net-api-port <port>`, `--api-shutdown-delay`, `--tar1090-use-api`): upstream's HTTP query API,
  refreshed every `--write-json-every`. `box`, `circle`, `closest`, `find_hex`/`find_reg`/`find_callsign`/
  `find_squawk`/`find_type`, `filter_*`, `above_alt_baro`/`below_alt_baro`, `all`, `all_with_pos`; replies as JSON,
  `jv2`, `bincraft` or `zstd`; `?status` answers 503 during the shutdown delay. On a live capture 14 query types
  returned the same aircraft in the same order as upstream.
- **Globe index** (`--write-json-globe-index`): `globe_XXXX.binCraft.zst` per tile (upstream's 60 special tiles and
  3 degree grid, 192 files), `receiver.json` with `globeIndexGrid`, per-aircraft message rates, and
  `--json-reliable` (default 2 with the globe index).
- **Traces** (with the globe index, or `--write-globe-history <dir>`): `traces/XX/trace_recent_<hex>.json` and
  `trace_full_<hex>.json` (gzipped), recorded with upstream's rules (altitude, speed, vertical rate, turn and
  ground-state changes, `--json-trace-interval`, default 20 s); `--write-globe-history` adds one file per
  aircraft and UTC day; `--json-trace-hist-only`, `--full-trace-dir`.
- **Interactive mode** (`--interactive`, `--interactive-ttl`, `--no-interactive`; run as `viewadsb` it connects to
  `127.0.0.1:30005`): the aircraft table on the terminal, upstream's columns, units (`--metric`, `--gnss`) and
  sort order, drawn with plain ANSI codes.
- **Dashboard** (`--dashboard-port <port>`, OpenRadome only, off unless given): a status web page. See
  [Dashboard](#dashboard).
- **MCP server** (`--mcp-port <port>`, OpenRadome only, off unless given): read-only tools an AI assistant can
  ask about the station, over HTTP or HTTPS, behind a token. See [MCP server](#mcp-server).
- **Configuration file** (`--config <file.toml>`, `--print-config`; OpenRadome only): every option from a TOML
  file. See [Configuration file](#configuration-file).
- `--stats`, `--raw`, `--onlyaddr`, `--show-only`, `--filter-DF` for the terminal.

## Not implemented

- Other SDRs (bladeRF, HackRF, SoapySDR, Pluto, Beast serial), E4000/FC00xx tuners, `--adaptive-*` gain.
  `--gain` follows the driver's model of the R820T stages; 49.6 dB or more is fully open, as with librtlsdr's maximum.
- Aircraft state (upstream's `blob_*.zstl` under `--write-state`, dumps of its C structures for its traces): the
  aircraft table is rebuilt from live messages within seconds. `--write-state-every` is accepted without effect.
- Receiver position inference, JAERO and SBS input (and with it the SBS-only MLAT extras), UAT (`uat_in`
  connectors are accepted and skipped), VRS, JSON-position, ASTERIX and Kinetic outputs, `--stats-every`, the
  `remote` block of `stats.json`, the aircraft database (`--db-file`: registration, type and military flags are
  empty, so the `filter_mil`/`find_reg`/`find_type` API queries find nothing), heatmaps.
- Traces: leg markers (`mark_legs`), the extra points before landing (`traceLast`), state files and loading
  traces after a restart (a restart starts the traces afresh), and upstream's compressed in-memory chunks (points
  are kept uncompressed, at most 40,000 per aircraft). Trace decisions follow upstream's rules but have only been
  checked on a synthetic flight capture, not against upstream's own trace files.
- Globe files: the per-tile `globe_XXXX.json` and `globeMil_*` files are not written (tar1090 uses the binCraft
  tiles). Interactive mode has no Mode A/C rows.
- About 50 of upstream's roughly 150 flags are accepted. `--net-ro-interval`, `--net-ro-interval-beast-reduce` and
  `--json-location-accuracy` values other than 2 are accepted without effect.

## Quick start

Requirements: Rust stable (edition 2024).

```sh
cargo build --release
./target/release/openradome --help
```

Decode an IQ capture and print the messages:

```sh
openradome --ifile capture.bin --iformat UC8 --raw
openradome --ifile capture.bin --stats            # receiver statistics, as upstream prints them
```

Run a receiver with an RTL-SDR dongle, network outputs and JSON for tar1090 (the usual ports):

```sh
openradome --device-type rtlsdr --gain 49.6 --fix --lat 51.5 --lon -0.1 \
    --net --net-ro-port 30002 --net-sbs-port 30003 --net-bi-port 30004,30104 --net-bo-port 30005 \
    --write-json /run/openradome
```

Feed an aggregator from a running decoder (what each network's feed client does):

```sh
openradome --net --net-only --quiet --lat 51.5 --lon -0.1 \
    --net-connector 127.0.0.1,30005,beast_in,silent_fail \
    --net-connector feed.adsb.lol,30004,beast_reduce_plus_out,feed2.adsb.lol,1337 \
    --net-beast-reduce-interval 0.5 --uuid-file /usr/local/share/adsblol/adsblol-uuid \
    --write-json /run/adsblol-feed
```

As with upstream, a single-value option given twice takes the last value, which some feeder installers rely on.

## Files and ports

| | Usual setting | Contents |
|---|---|---|
| Beast output | `--net-bo-port 30005` | every accepted message with 12 MHz timestamp and signal level |
| Beast input | `--net-bi-port 30004,30104` | other decoders, MLAT results (magic MLAT timestamp) |
| raw output / input | `--net-ro-port 30002` / `--net-ri-port 30001` | `*hex;` lines (never MLAT) |
| SBS output | `--net-sbs-port 30003` | BaseStation CSV lines |
| BeastReduce output | `--net-beast-reduce-out-port` or connector | paced messages for aggregators |
| `aircraft.json` | every `--write-json-every` seconds | aircraft and their fields, as upstream |
| `receiver.json` | at start | location, version, `outlineJson` |
| `stats.json` | every 10 s | receiver and tracker statistics |
| `outline.json` | every 15 s | farthest position per direction |
| `<--write-state>/internal_state/rangeDirs.gz` | every 5 min and at exit | the range outline, in upstream's format |
| MCP server | `--mcp-port 8091` (off unless given) | `/mcp`: Model Context Protocol tools for AI assistants, token required |
| Dashboard | `--dashboard-port 8090` (off unless given) | status web page: live figures, system figures, 24 h graphs, open connections |

JSON files are written to a temporary file and renamed, so readers never see a partial file.

## Dashboard

An optional status page, served by OpenRadome itself. It is off unless you give a port. With `install.sh`, add
it to `/etc/openradome/openradome.toml` (or install with `--dashboard-port 8090`), then restart:

```toml
# /etc/openradome/openradome.toml
dashboard-port = 8090
dashboard-bind-address = "0.0.0.0"   # only needed when net-bind-address keeps the data ports on 127.0.0.1
```

```sh
sudo systemctl restart openradome
```

Then open `http://<the Pi's address>:8090/`. The page shows:

- **Now:** messages per second, aircraft (and how many have a position), the furthest aircraft, positions
  decoded, signal and noise, strong signals (too many means the gain is too high), dropped samples (should be 0),
  and the decoder's own CPU and memory use.
- **System** (Linux): the whole machine's CPU and load, the CPU of all openradome processes together (decoder and
  feed clients), memory and swap, temperature and, on a Raspberry Pi, the under-voltage warning (a weak power
  supply), disk space, Wi-Fi signal and uptime. Read from `/proc` and `/sys` every 10 s, by a background task.
- **The last 24 hours:** graphs of the message rate, aircraft, range, signal and noise, CPU (whole machine and
  OpenRadome) and temperature, one point a minute. After a restart the time axis starts at an hour and grows.
- **Connections:** everything connected to the decoder's ports right now (feed clients, MLAT results, tar1090,
  viewers) and for how long, and the connections it makes itself (`--net-connector`).

Signal, noise, strong signals and dropped samples appear only when there is a local receiver; a `--net-only`
process (a feed client) can have a dashboard too, without them.

How it works: the decoder adds one point a minute to an in-memory history of 24 hours (1440 points, tens of kB).
The history starts again when the decoder restarts. The page is drawn only when someone opens it: HTML with SVG
graphs, no JavaScript, nothing loaded from the internet, and it reloads itself every 10 s. It is read-only:
it answers `GET` for `/` and nothing else changes anything. It listens on `--dashboard-bind-address` if given, else on
`--net-bind-address` like the other ports. So a station whose data ports are kept local
(`--net-bind-address 127.0.0.1`) can still show the page on the LAN with `--dashboard-bind-address 0.0.0.0` (or the
Pi's LAN address). There is no password, so do not expose the port to the internet.

It answers requests for an IP address, `localhost`, a name without a dot (`raspberrypi`) or a name under `.local`,
`.lan`, `.home.arpa` or `.internal`. Any other name gets 403 Forbidden, so a web page on someone else's domain
can't read the dashboard by pointing that domain at your LAN (DNS rebinding). If you reach it by another name,
such as through a reverse proxy, add it with `--dashboard-host adsb.example.org` (repeatable; in the TOML file
`dashboard-host = ["adsb.example.org"]`).

## MCP server

An optional [Model Context Protocol](https://modelcontextprotocol.io) server, so an AI assistant (Claude Code
or any other MCP client) can ask the station how it is doing, in plain language: "how is reception tonight?",
"when did the message rate drop?", "what is the low aircraft over the house?". It is off unless you give it a
port, and read-only: no tool can change anything.

### Turning it on

```toml
# /etc/openradome/openradome.toml
mcp-port = 8091
mcp-token-file = "/etc/openradome/mcp-token"        # one token of at least 32 characters
# mcp-bind-address = "0.0.0.0"                    # when net-bind-address keeps the data ports on 127.0.0.1
# mcp-tls-cert = "/etc/openradome/tls/fullchain.pem"  # HTTPS: both lines, or neither (see HTTPS below)
# mcp-tls-key = "/etc/openradome/tls/privkey.pem"
```

`sudo ./install.sh --mcp-port 8091` makes a random token in `/etc/openradome/mcp-token` (readable by root and the
`openradome` service only) and, on a new install, writes these lines; with an existing file it says what to add.
Then `sudo systemctl restart openradome`. The server is on while `mcp-port` is set: put a `#` in front of it and
restart to switch it off.

### Adding it to Claude Code

From the computer you run Claude Code on (the `$( )` fetches the token over SSH, so it never shows on screen):

```sh
claude mcp add --transport http --scope user openradome https://<pi>:8091/mcp \
  --header "Authorization: Bearer $(ssh <pi> sudo cat /etc/openradome/mcp-token)"
```

- `--transport http`: MCP's Streamable HTTP transport, which OpenRadome speaks. Use `http://` in the address if
  the server runs without TLS, `https://` with it.
- `--scope user`: available in all your projects; `--scope local` for the current folder only, `--scope project`
  to share it through the project's `.mcp.json` (the token would then be in that file: avoid).
- With a certificate from a public authority (Let's Encrypt, for example) Claude Code trusts it with no extra
  setup. A self-signed certificate needs `NODE_EXTRA_CA_CERTS=/path/to/cert.pem` in Claude Code's environment.

Check it:

```sh
claude mcp list          # openradome: https://<pi>:8091/mcp (HTTP) - ✓ Connected
```

Then start a **new** Claude Code session (servers are loaded when a session starts). `/mcp` there lists
OpenRadome and its six tools. Ask in plain language (examples below); Claude Code asks your permission the first
time it uses each tool, and "always allow" is safe for these, as none changes anything. Its answers show
"Called OpenRadome" when it used the station's tools.

| Problem | Cause |
|---|---|
| `✗ Failed to connect`, or 401 in the decoder's log | wrong or missing token: `ssh <pi> sudo cat /etc/openradome/mcp-token` must print one token of 64 characters |
| name not found, or no connection | the name or port is not reachable from here (another network, firewall, `mcp-bind-address` still 127.0.0.1) |
| certificate error | the certificate is not trusted (self-signed: `NODE_EXTRA_CA_CERTS`), or the name in the address is not in the certificate |
| tools missing after `claude mcp add` | the session was started before: start a new one |

`claude mcp remove openradome` removes it, stored token included. After a token change (see Token below), remove it
and add it again.

**Other MCP clients** work the same way if they speak Streamable HTTP and can send an `Authorization` header.
A quick test without any client:

```sh
curl -s https://<pi>:8091/mcp -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"station_status"}}'
```

### What you can ask

Six tools. You do not call them by name: ask the question, and the assistant picks the tools (often several
for one question) and explains the answer.

| Tool | Options | What it returns |
|---|---|---|
| `station_status` | none | messages per second (last 10 s and last minute), aircraft and how many have a position, the furthest aircraft and positions decoded in the last minute, signal and noise (dBFS), strong signals, dropped samples, the decoder's CPU and memory, uptime; and the maxima over the kept history |
| `history` | `minutes` (1 to 1440, default 60) | one entry a minute, oldest first: message rate, aircraft, furthest aircraft, positions, signal, noise, strong signals, dropped samples, the computer's CPU and OpenRadome's share, temperature. Kept in memory for up to 24 hours, from the decoder's start |
| `system` | none | the computer: CPU (last 10 s), load average, memory and swap, temperature, Raspberry Pi under-voltage, disk use, Wi-Fi signal, uptime, how many OpenRadome processes run and their CPU together (Linux only) |
| `connections` | none | every open connection: clients on the decoder's ports (`beast out :30005`, `beast in :30104`, ...), its own outgoing connectors, how long each has been connected, counts per port |
| `aircraft_query` | `query`, plus the options below | the matching aircraft in readsb's API format: `hex`, `flight` (callsign), `alt_baro`, `gs`, `track`, `lat`, `lon`, `squawk`, `category`, `rssi`, `seen`, and `dst` (distance in nm from the query's centre) |
| `range_outline` | none | the furthest position heard in each direction over the outline period (24 h by default): the station's coverage. Needs the receiver position and `write-json` |

`aircraft_query` searches:

| `query` | Options | Finds |
|---|---|---|
| `closest` | `radius_nm` (default 25), `lat`/`lon` (default: the receiver) | the nearest aircraft with a position within the radius |
| `within` | `radius_nm` (default 25), `lat`/`lon` (default: the receiver) | every aircraft within the radius, each with its distance (`dst`) |
| `box` | `south`, `north`, `west`, `east` (degrees) | every aircraft inside the box |
| `find_hex` | `value`: ICAO addresses, comma-separated (`43c5e9,40666c`) | those aircraft |
| `find_callsign` | `value`: callsigns, comma-separated (`BAW123,RYR5UK`) | those flights |
| `find_squawk` | `value`: squawk codes, comma-separated (`7700,7600,7500`) | aircraft squawking them |
| `all` | `with_position` (default true) | every aircraft heard (with a position unless `with_position` is false) |

Questions that work well, and the tools behind them:

| Ask | Uses |
|---|---|
| "How is the ADS-B station doing?" | `station_status`, `system`, `connections` |
| "Is reception normal for this time of day?" / "How many aircraft are we seeing?" | `station_status`, `history` |
| "When did the message rate drop in the last 6 hours, and did anything else change then?" | `history` (minutes 360) |
| "Has the receiver dropped any samples today?" / "Is the gain too high?" | `station_status`, `history` (dropped samples, strong signals) |
| "How far can the station see? Which direction is weakest?" | `range_outline`, `station_status` |
| "How hot is the Pi, and is the power supply OK?" / "Is the SD card filling up?" | `system` |
| "How much CPU do the decoder and the feeds use?" | `system`, `station_status` |
| "Is the Wi-Fi good enough?" | `system` (signal), `connections` (are the feeds connected) |
| "Which feeds and MLAT clients are connected, and for how long?" | `connections` |
| "What's the closest aircraft right now?" / "What's flying overhead?" | `aircraft_query` closest |
| "List the aircraft within 10 nm, lowest first." | `aircraft_query` within, radius 10 |
| "Any aircraft between London and Brighton?" | `aircraft_query` box |
| "Is anyone squawking 7700, 7600 or 7500?" | `aircraft_query` find_squawk |
| "Is BAW123 in range? How high and how far?" / "Where is 43c5e9?" | `aircraft_query` find_callsign / find_hex |
| "Which aircraft are helicopters (category A7)?" / "Which is the highest?" | `aircraft_query` all |
| "Compare the last hour with the hour before." | `history` (minutes 120) |

What it does not know: anything older than the decoder's start (the history is in memory and starts again at
each restart), registrations and types beyond what the aircraft broadcast (OpenRadome has no aircraft database),
and which program is behind a local connection (`connections` shows `127.0.0.1:<port>`; a station with many
feeders has dozens of them, which is normal).

### HTTPS

With `mcp-tls-cert` and `mcp-tls-key` (PEM files; the certificate chain leaf first) the server speaks HTTPS only,
TLS 1.2 and 1.3 (rustls). The service user must be able to read both files. The files are looked at again at
most every 10 s: a renewed certificate is used without a restart, and one that fails to load is reported while
the previous stays in use.

To switch TLS off, put a `#` in front of **both** lines and restart: the server then speaks plain HTTP on the same
port, and clients must change `https://` to `http://` (in Claude Code: remove and add it again). Only one of the
two set is an error, and OpenRadome does not start. Without TLS the token crosses the network in the clear: keep
it to a network you trust.

### Token

`install.sh --mcp-port` makes it: 32 random bytes from `/dev/urandom` as 64 hex characters (256 bits), in
`/etc/openradome/mcp-token`, owner `root:openradome`, mode 0640, made only if the file does not exist. OpenRadome reads
it when it starts: at least 32 characters, no spaces, and a warning if every user can read the file.

To rotate it (if it may have leaked):

```sh
sudo sh -c 'umask 077; openssl rand -hex 32 > /etc/openradome/mcp-token.new'
sudo chown root:openradome /etc/openradome/mcp-token.new && sudo chmod 0640 /etc/openradome/mcp-token.new
sudo mv /etc/openradome/mcp-token.new /etc/openradome/mcp-token
sudo systemctl restart openradome        # from now on the old token gets 401
```

then remove and add the server again in each client.

### Security

Every request needs `Authorization: Bearer <token>`; the token is compared in constant time, and a refused
request is logged (at most once a minute, without the token). Requests that carry an `Origin` header (made by web
pages) are refused, so a page open in a browser on the LAN cannot use it. No tool changes anything. Callsigns
and other aircraft fields come from radio messages anyone can transmit: control characters are removed, and the
server tells the assistant to treat them as data, never as instructions.

### How it works

MCP's Streamable HTTP transport: JSON-RPC 2.0 POSTed to `/mcp`, each answered with one JSON reply (no sessions,
no event stream); protocol versions 2025-11-25, 2025-06-18, 2025-03-26 and 2024-11-05. Each tool returns its
answer as JSON text and as structured content. The dashboard's figures are collected whenever the MCP server or
the dashboard is on; the aircraft list is rebuilt only while someone is asking, so an idle server costs nothing.

## Architecture

A Cargo workspace with two crates:

```
crates/
  openradome-core/   library: decoding and tracking. No I/O, no threads, no async.
  openradome/        binary: dongle, files, sockets, JSON files, command line.
```

`openradome-core` takes bytes in and returns messages and aircraft state, so all of the decoding and tracking is
testable with vectors and recorded files.

```
 RTL-SDR ──(rtlsdr thread)──┐
 IQ file ───────────────────┤  raw IQ
                            ▼
          IqPipeline: convert → magnitudes (64 k-sample blocks, 326-sample overlap)
                      → Demodulator → Decoder (CRC, ICAO filter, Mode S / ADS-B / Comm-B)
                            │  one batch of messages per block
 Beast/raw TCP in ─(tokio)─►│
                            ▼
               Engine (main thread): track the whole batch, then forward it
                            │
                            ▼
               Tracker: aircraft, per-field validity, CPR, reliability, range outline
                            │
        ┌───────────────────┼──────────────────────────┐
        ▼                   ▼                          ▼
   Hub: raw / Beast /    JsonWriter                  stdout
   SBS / BeastReduce     aircraft, receiver,         (--raw, --stats)
   (tokio clients)       stats, outline .json
```

**openradome-core**

| Module | Upstream | Does |
|---|---|---|
| `convert` | `convert.c` | IQ to magnitude, block level and power |
| `demod` | `demod_2400.c` | preamble scan, phase slicing, scoring, timestamps |
| `pipeline` | `sdr_ifile.c`, upstream's main file | blocks, overlap, sample clock, dropped-sample gaps |
| `crc`, `icao` | `crc.c`, `icao_filter.c` | CRC-24 and repair, address filter |
| `mode_s`, `comm_b`, `mode_ac` | `mode_s.c`, `comm_b.c`, `mode_ac.c` | message decoding |
| `cpr`, `geo`, `geomag` | `cpr.c`, `geomag.c` | position decoding, distances, WMM2025 |
| `track` | `track.c`, `aircraft.c` | aircraft state, validity, CPR pairing, reduce pacing, outline |
| `json` | `json_out.c`, `stats.c` | the JSON documents and statistics windows |
| `beast`, `output`, `sbs` | `net_io.c` | wire formats in and out |
| `synth` | | test signal generator (`examples/synth_iq.rs`) |

**OpenRadome (binary)**

| Module | Does |
|---|---|
| `main` | builds the pipeline from the command line, runs the input loop, final statistics |
| `cli` | clap definitions with upstream's flag names |
| `sdr` | RTL-SDR reader thread: never blocks on its queue, 30 s silence watchdog, bounded shutdown |
| `engine` | per-batch tracking and upstream's forwarding rules |
| `net` | tokio listeners and connectors, output batching, UUID and ping handling, loss accounting |
| `jsonout` | periodic, atomic JSON writes |

At run time: the main thread (decoding, tracking, JSON), the `rtlsdr` reader thread, two tokio workers and the USB
event thread, joined by bounded channels.

## Verification against upstream

Upstream is built in Docker at a pinned commit (`tools/upstream/Dockerfile`) and, for live checks, run on the
Pi next to OpenRadome. The same input goes through both programs and the outputs are compared.

| Input | Compared | Result |
|---|---|---|
| Synthetic IQ (UC8, SC16, SC16Q11; `--fix`, `--no-fix`, `--aggressive`, `--no-fix-df`, `--modeac`, thresholds) | messages and statistics | identical |
| 60 s of real RF recorded from the Pi's antenna | 8 771 messages, statistics | byte-identical |
| 90 s recording of the live Beast feed | `aircraft.json`, 681 fields incl. Comm-B, wind, temperature | identical |
| 130 s capture of live Beast and 12 mlat-clients' results | `aircraft.json`, with and without ADS-B (686 and 102 fields) | identical |
| same capture, as aggregator feed clients | `beast_reduce_plus` streams | 99.86 % of messages identical; two copies of upstream agree 99.84 % (pacing-timer boundaries) |
| same capture | `outline.json` | byte-identical |
| same capture, `--write-state`; each decoder restarted from the other's `rangeDirs.gz` | `outline.json` (240 points) | identical in both directions |

`cargo test` repeats the decoding comparison on every run, on three synthetic captures from `examples/synth_iq.rs`:
`testdata/synth_uc8.bin` (random frames), `testdata/flights_uc8.bin` (3 s of 12 simulated flights with bit
errors, overlaps and bad positions) and `testdata/squawk_uc8.bin` (10 flights that also send squawks, emergency
codes among them). Each is compared with upstream's output for the same file: the decoded messages, the
statistics, and for the two flight captures `aircraft.binCraft` field by field. `tools/regen-golden.sh` makes the
upstream files. The recordings in the table above were made at the author's station and are not in the
repository.

The tests also fuzz every decoder that takes outside input (`crates/openradome-core/tests/fuzz.rs`): radio frames
with valid CRCs, mutated real frames, Comm-B registers, IQ samples, Beast and raw streams and API queries, through
the tracker and every output format, checking that nothing panics, the JSON stays valid and memory stays bounded.
`cargo test` runs a short round; `make fuzz FUZZ_ITERS=50000000` a long one. `make coverage` reports the line
coverage (about 90 %).

## Performance

On a Raspberry Pi 2 (900 MHz Cortex-A7):

- Decoding 60 s of recorded RF takes 8.8 s of CPU (upstream: 8.1 s).
- The live decoder (2.4 MS/s, 30 output clients) uses about 45 % of one core and 9 MB. Whole-Pi CPU at the same
  message rate is the same as with upstream, to within measurement noise.
- A feed client uses about 2 % of one core and 7 MB.
- No dropped samples in normal operation.

## Install

On Raspberry Pi OS, Debian, Ubuntu or any Linux with systemd (x86_64, aarch64, armv7, armv6; glibc 2.31 or
newer), with an RTL-SDR dongle plugged in:

```sh
curl -fsSL https://raw.githubusercontent.com/phillip-bailey/openradome/main/install.sh | sudo bash -s -- --lat 51.5 --lon -0.1
```

The script picks the build for the CPU, downloads it with the release's `SHA256SUMS` and refuses a mismatch,
installs `/usr/local/bin/openradome`, creates the `openradome` system user (in `plugdev`), adds a udev rule for
the dongle and keeps the DVB-T TV driver off it, then writes the options file `/etc/openradome/openradome.toml`
(see [Configuration file](#configuration-file)) and `openradome.service` and starts it. JSON goes to
`/run/openradome`, Beast to port 30005.

- Re-run it to upgrade; your `openradome.toml` is kept, the previous binary saved as `openradome.prev`, and
  other services still running the old binary (feed clients using `openradome --net-only`) are restarted.
  `--rollback` puts the previous binary back.
- `--version vX.Y.Z` pins a release; `--archive FILE` installs a downloaded archive (offline).
- If dump1090 is running it stops and asks: `--replace` disables it (it holds the dongle).
- `--json-dir /run/<name>` changes where the JSON goes; `--bind 127.0.0.1` keeps the ports local.
- `sudo ./install.sh --uninstall` removes it (`--purge` also the options file and user).
- **Moving from rustadsb** (OpenRadome's name before 0.6): running the installer on a rustadsb install moves it
  over once. The options file is converted and checked with the new binary first; then the service, user and group
  (renamed in place, so file owners are kept), `/etc/rustadsb`, the saved state, the MCP token and TLS files become
  OpenRadome's. The old paths stay as links: `/usr/local/bin/rustadsb` (feed clients that start it keep working),
  `/etc/rustadsb`, `/run/rustadsb` (re-created at boot), and the service also answers to `rustadsb.service`.
  `--rollback` afterwards puts the last rustadsb binary back under the new name.
- rustadsb installs from before 0.4 kept their options as shell variables in `/etc/default/rustadsb`; the move
  converts them to `openradome.toml` too (the old file is kept as `/etc/default/rustadsb.migrated-<date>`).

### Configuration file

`openradome --config <file.toml>` reads options from a TOML file. [`openradome.example.toml`](openradome.example.toml) is a
complete, commented example (it is also in every release archive, and `install.sh` writes it, with your choices
filled in, as `/etc/openradome/openradome.toml` on a new install). Each key is a long option's name as
`openradome --help` lists it, without the dashes; `true` turns an on/off option on; an option that can be given
more than once takes a list. Options on the command line override the file. A mistyped key is an error, not
ignored.

```toml
device-type = "rtlsdr"
gain = 49.6
lat = 51.5
lon = -0.1
fix = true
net = true
net-bind-address = "127.0.0.1"
net-bo-port = 30005
net-bi-port = "30004,30104"
write-json = "/run/openradome"
net-connector = ["feed.adsb.lol,30004,beast_reduce_plus_out"]
```

`--print-config` prints the options it is given (and those from `--config`) as such a file, so a command line
becomes a config file with `openradome <your options> --print-config > openradome.toml`.

macOS and Windows: download the archive from the release page (see below), unpack it and run `openradome`
directly.

### Which file to download

Each release has one archive per platform, named `openradome-<version>-<platform>`. On Linux, `install.sh` picks the
right one for you; to download one yourself:

| Your device | Download |
|---|---|
| Raspberry Pi 5, 4, 3 or Zero 2 W on **64-bit** Raspberry Pi OS | `openradome-<version>-linux-arm64.tar.gz` |
| Raspberry Pi 5, 4, 3, 2 or Zero 2 W on **32-bit** Raspberry Pi OS | `openradome-<version>-linux-armv7.tar.gz` |
| Raspberry Pi 1, Zero or Zero W | `openradome-<version>-linux-armv6.tar.gz` |
| Other 64-bit ARM Linux boards (Orange Pi, Rock Pi, Odroid, ARM servers) | `openradome-<version>-linux-arm64.tar.gz` |
| Linux PC, server, mini PC or VM (Intel or AMD) | `openradome-<version>-linux-x86_64.tar.gz` |
| Mac with Apple silicon (M1 or later) | `openradome-<version>-macos-arm64.tar.gz` |
| Mac with an Intel processor | `openradome-<version>-macos-x86_64.tar.gz` |
| Windows PC (Intel or AMD) | `openradome-<version>-windows-x86_64.zip` |
| Windows on ARM (Snapdragon laptops, Surface Pro X) | `openradome-<version>-windows-arm64.zip` |

Not sure which Linux build? Run `uname -m` and `getconf LONG_BIT`:

| `uname -m` | `getconf LONG_BIT` | Download |
|---|---|---|
| `aarch64` | `64` | `linux-arm64` |
| `aarch64` | `32` | `linux-armv7` (64-bit kernel, 32-bit system) |
| `armv7l` | `32` | `linux-armv7` |
| `armv6l` | `32` | `linux-armv6` |
| `x86_64` | `64` | `linux-x86_64` |

On a Mac, Apple menu > About This Mac shows "Chip: Apple M…" (Apple silicon) or "Processor: Intel". On
Windows, Settings > System > About shows "x64-based processor" or "ARM-based processor".

Linux builds need glibc 2.31 or newer: Raspberry Pi OS Bullseye (11) or later, Debian 11, Ubuntu 20.04 or
later. Check a download against the release's `SHA256SUMS` with `sha256sum -c --ignore-missing SHA256SUMS`
(Linux) or `shasum -a 256 -c --ignore-missing SHA256SUMS` (macOS).

## Releases

Releases are built by GitHub Actions (`.github/workflows/build.yml`). Every push and pull request runs the
tests on Linux and macOS and builds all eight archives (Linux and Windows cross-compiled with cargo-zigbuild,
macOS on a macOS runner); the archives are kept as workflow artifacts for 14 days. Pushing a tag `vX.Y.Z`
also publishes them as a GitHub release with `SHA256SUMS` (a tag with a `-`, like `v1.0.0-rc1`, is a
pre-release):

```sh
git tag v0.3.0 && git push origin v0.3.0
```

| Archive | Rust target it is built for |
|---|---|
| `linux-x86_64` | `x86_64-unknown-linux-gnu` |
| `linux-arm64` | `aarch64-unknown-linux-gnu` |
| `linux-armv7` | `armv7-unknown-linux-gnueabihf` |
| `linux-armv6` | `arm-unknown-linux-gnueabihf` |
| `windows-x86_64` | `x86_64-pc-windows-gnu` |
| `windows-arm64` | `aarch64-pc-windows-gnullvm` |
| `macos-arm64` | `aarch64-apple-darwin` |
| `macos-x86_64` | `x86_64-apple-darwin` |

Releases before 0.6 were published under the project's earlier name, rustadsb (`rustadsb-v0.5.0-linux-armv7.tar.gz`,
program `rustadsb`); `install.sh` still finds them, and moves a rustadsb install over to OpenRadome (see
Install). Releases before the plain names named the archives after the Rust target
(`rustadsb-v0.2.0-armv7-unknown-linux-gnueabihf.tar.gz`); `install.sh` tries the plain name first and falls
back to that. Linux binaries need glibc 2.31 or newer.
Tested on real hardware so far: armv7 (Pi 2) and macOS Apple silicon. The other targets build but have not been run.

## Building and deploying

`make` targets (see the `Makefile` for the variables):

```sh
make test    # cargo fmt --check, clippy -D warnings, all tests
make pi      # cross-compile for armv7 (Raspberry Pi 2/3/4 32-bit) with cargo-zigbuild
make push    # make pi, then rsync to $(PI):$(PI_DIR)/openradome
```

Cross-compiling from macOS needs `brew install rustup zig`, `cargo install cargo-zigbuild`,
`rustup target add armv7-unknown-linux-gnueabihf`, and the rustup Homebrew directory on `PATH`. The binary is built
against glibc 2.31 (`GLIBC=`), so it runs on older and newer Raspberry Pi OS releases.

To install a build as a service, see [Install](#install): `install.sh --archive <file>` takes a local archive.

## Tools

| Tool | Does |
|---|---|
| `tools/compare.sh capture.bin [UC8\|SC16\|SC16Q11]` | run one IQ file through upstream (Docker) and OpenRadome, diff messages and statistics |
| `tools/regen-golden.sh` | regenerate the golden files in `testdata/` from upstream |
| `examples/synth_iq.rs` | synthetic captures: random frames, flights, noise, bit errors, overlaps |
| `tools/replay-beast.py capture.bin PORT` | replay a recorded Beast stream at its original pace |
| `tools/replay-pcap.py capture.pcap MLAT_PORT BEAST_PORT` | replay a `tcpdump` capture of mlat-client results and a Beast flow, per connection, at capture pace |
| `tools/compare-json.py DIR` | compare two `aircraft.json` files field by field |
| `tools/soak.sh DIR` / `tools/soak-report.py` | log a running service once a minute and summarise the log |
| `tools/state-compare.sh` | on a Pi with upstream installed as `/usr/local/bin/readsb`, `./openradome-state` and `mlat.pcap` in `~/cmp`: both decoders save state from the replay, then each restarts from the other's `rangeDirs.gz` |

```sh
docker build -t openradome-upstream tools/upstream
cargo build --release
tools/compare.sh capture.bin
cargo run --release --example synth_iq -p openradome-core -- out.bin --random 300 --count 4000 --flip 20 --overlap 10
```

## Development

- Every change must pass `make test`: formatting, clippy with warnings as errors, unit tests with known vectors
  (CRC, CPR, Mode A/C, Comm-B, WMM), network tests, and the golden tests against upstream.
- `unsafe` is forbidden in the workspace; it would only be considered with a benchmark that proves the need.
- Networking uses tokio, the command line clap.

## Changes

What changed in each release: [CHANGELOG.md](CHANGELOG.md).

## Security

Found a security problem? Please report it privately, not in a public issue: see [SECURITY.md](SECURITY.md).

## Licence

GPL-3.0-or-later (see `COPYING`). The decoding, tracking and output logic is ported from
[wiedehopf/readsb](https://github.com/wiedehopf/readsb) (GPL v3 or later), which incorporates dump1090-fa and
dump1090-mutability (GPL v2 or later) and the original dump1090 (BSD-style). Their authors are credited in
[`NOTICE`](NOTICE): Matthias Wirth, Michael Wolf, Oliver Jowett, FlightAware LLC and Salvatore Sanfilippo, with
dump1090's BSD licence text. The World Magnetic Model data is NOAA's (public domain).
