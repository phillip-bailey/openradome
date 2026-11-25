# rustadsb

## Executive summary

**ADS-B** (Automatic Dependent Surveillance–Broadcast) is how aircraft tell the world where they are. An equipped
aircraft works out its own position by satellite navigation (GPS) and broadcasts it by radio on 1090 MHz, about
twice a second, together with its identity, callsign, altitude, speed and heading. Nothing has to ask for it: the
report is *automatic*, it *depends* on the aircraft's own navigation rather than on a radar measuring it, and it is
*broadcast* to anyone listening.

**Why it is used**

- **Air traffic control sees more, more precisely.** A radar sweeps every 4 to 12 seconds and grows less accurate
  with distance; ADS-B gives a GPS-accurate position twice a second. A receiving station costs a small fraction of a
  radar, so coverage reaches places radar does not: remote regions, mountains, and (through satellite receivers)
  the oceans.
- **Aircraft see each other.** Other aircraft receive the same broadcasts, so pilots and on-board traffic displays
  know what is nearby, not only what the ground tells them.
- **It is required.** Since 2020, most aircraft in United States controlled airspace and larger aircraft in Europe
  must broadcast ADS-B, and many other countries have similar rules. Nearly every airliner transmits it.
- **Anyone can receive it.** The signal is not encrypted. A USB radio costing about 20 € and an antenna on a
  Raspberry Pi pick up aircraft 200 to 400 km away. Tens of thousands of volunteer stations built this way feed
  flight-tracking services (Flightradar24, FlightAware, ADS-B Exchange, adsb.lol, adsb.fi, airplanes.live and
  others), whose maps are used by the public, journalists, researchers studying emissions and noise, airports and
  accident investigators.

**What rustadsb is.** The software at the heart of such a receiving station. It turns the raw radio signal from
the USB radio into a live list of aircraft with their positions, feeds the station's own map, and forwards the
data to the flight-tracking networks. It is a memory-safe rewrite, in Rust, of readsb, the decoder most stations
run today. It takes the same options and produces the same outputs, so it can replace readsb without changing the
rest of a station. It runs in service on a Raspberry Pi, and its output is checked against readsb's on recorded
and live data.

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
twice a second without being asked (ADS-B). Anyone can receive this: a 20 € RTL-SDR USB dongle and an antenna on a
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

Upstream is the standard program for this chain and the one most feeder setups run, in C. rustadsb does
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
  own format, so rustadsb and upstream each read the other's file. Time slices that passed while nothing ran are
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
./target/release/rustadsb --help
```

Decode an IQ capture and print the messages:

```sh
rustadsb --ifile capture.bin --iformat UC8 --raw
rustadsb --ifile capture.bin --stats            # receiver statistics, as upstream prints them
```

Run a receiver with an RTL-SDR dongle, network outputs and JSON for tar1090 (the usual ports):

```sh
rustadsb --device-type rtlsdr --gain 49.6 --fix --lat 51.5 --lon -0.1 \
    --net --net-ro-port 30002 --net-sbs-port 30003 --net-bi-port 30004,30104 --net-bo-port 30005 \
    --write-json /run/rustadsb
```

Feed an aggregator from a running decoder (what each network's feed client does):

```sh
rustadsb --net --net-only --quiet --lat 51.5 --lon -0.1 \
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

JSON files are written to a temporary file and renamed, so readers never see a partial file.

## Architecture

A Cargo workspace with two crates:

```
crates/
  rustadsb-core/   library: decoding and tracking. No I/O, no threads, no async.
  rustadsb/        binary: dongle, files, sockets, JSON files, command line.
```

`rustadsb-core` takes bytes in and returns messages and aircraft state, so all of the decoding and tracking is
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

**rustadsb-core**

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

**rustadsb (binary)**

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
Pi next to rustadsb. The same input goes through both programs and the outputs are compared.

| Input | Compared | Result |
|---|---|---|
| Synthetic IQ (UC8, SC16, SC16Q11; `--fix`, `--no-fix`, `--aggressive`, `--no-fix-df`, `--modeac`, thresholds) | messages and statistics | identical |
| 60 s of real RF recorded from the Pi's antenna | 8 771 messages, statistics | byte-identical |
| 90 s recording of the live Beast feed | `aircraft.json`, 681 fields incl. Comm-B, wind, temperature | identical |
| 130 s capture of live Beast and 12 mlat-clients' results | `aircraft.json`, with and without ADS-B (686 and 102 fields) | identical |
| same capture, as aggregator feed clients | `beast_reduce_plus` streams | 99.86 % of messages identical; two copies of upstream agree 99.84 % (pacing-timer boundaries) |
| same capture | `outline.json` | byte-identical |
| same capture, `--write-state`; each decoder restarted from the other's `rangeDirs.gz` | `outline.json` (240 points) | identical in both directions |

`cargo test` repeats the decoding comparison on every run, on two synthetic captures from `examples/synth_iq.rs`:
`testdata/synth_uc8.bin` (random frames) and `testdata/flights_uc8.bin` (3 s of 12 simulated flights with bit
errors, overlaps and bad positions), each compared with upstream's output for the same file. The recordings in the
table above were made at the author's station and are not in the repository.

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
curl -fsSL https://raw.githubusercontent.com/p0bailey/rustadbtest/main/install.sh | sudo bash -s -- --lat 51.5 --lon -0.1
```

The script picks the build for the CPU, downloads it with the release's `SHA256SUMS` and refuses a mismatch,
installs `/usr/local/bin/rustadsb`, creates the `rustadsb` system user (in `plugdev`), adds a udev rule for
the dongle and keeps the DVB-T TV driver off it, then writes `/etc/default/rustadsb` (options) and
`rustadsb.service` and starts it. JSON goes to `/run/rustadsb`, Beast to port 30005.

- Re-run it to upgrade; your `/etc/default/rustadsb` is kept, the previous binary saved as `rustadsb.prev`, and
  other services still running the old binary (feed clients using `rustadsb --net-only`) are restarted.
  `--rollback` puts the previous binary back.
- `--version vX.Y.Z` pins a release; `--archive FILE` installs a downloaded archive (offline).
- If dump1090 is running it stops and asks: `--replace` disables it (it holds the dongle).
- `--json-dir /run/<name>` changes where the JSON goes; `--bind 127.0.0.1` keeps the ports local.
- `sudo ./install.sh --uninstall` removes it (`--purge` also the options file and user).

macOS and Windows: download the archive from the release page (see below), unpack it and run `rustadsb`
directly.

### Which file to download

Each release has one archive per platform, named `rustadsb-<version>-<platform>`. On Linux, `install.sh` picks the
right one for you; to download one yourself:

| Your device | Download |
|---|---|
| Raspberry Pi 5, 4, 3 or Zero 2 W on **64-bit** Raspberry Pi OS | `rustadsb-<version>-linux-arm64.tar.gz` |
| Raspberry Pi 5, 4, 3, 2 or Zero 2 W on **32-bit** Raspberry Pi OS | `rustadsb-<version>-linux-armv7.tar.gz` |
| Raspberry Pi 1, Zero or Zero W | `rustadsb-<version>-linux-armv6.tar.gz` |
| Other 64-bit ARM Linux boards (Orange Pi, Rock Pi, Odroid, ARM servers) | `rustadsb-<version>-linux-arm64.tar.gz` |
| Linux PC, server, mini PC or VM (Intel or AMD) | `rustadsb-<version>-linux-x86_64.tar.gz` |
| Mac with Apple silicon (M1 or later) | `rustadsb-<version>-macos-arm64.tar.gz` |
| Mac with an Intel processor | `rustadsb-<version>-macos-x86_64.tar.gz` |
| Windows PC (Intel or AMD) | `rustadsb-<version>-windows-x86_64.zip` |
| Windows on ARM (Snapdragon laptops, Surface Pro X) | `rustadsb-<version>-windows-arm64.zip` |

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

Releases before the plain names named the archives after the Rust target
(`rustadsb-v0.2.0-armv7-unknown-linux-gnueabihf.tar.gz`); `install.sh` tries the plain name first and falls
back to that. Linux binaries need glibc 2.31 or newer.
Tested on real hardware so far: armv7 (Pi 2) and macOS Apple silicon. The other targets build but have not been run.

## Building and deploying

`make` targets (see the `Makefile` for the variables):

```sh
make test    # cargo fmt --check, clippy -D warnings, all tests
make pi      # cross-compile for armv7 (Raspberry Pi 2/3/4 32-bit) with cargo-zigbuild
make push    # make pi, then rsync to $(PI):$(PI_DIR)/rustadsb
```

Cross-compiling from macOS needs `brew install rustup zig`, `cargo install cargo-zigbuild`,
`rustup target add armv7-unknown-linux-gnueabihf`, and the rustup Homebrew directory on `PATH`. The binary is built
against glibc 2.31 (`GLIBC=`), so it runs on older and newer Raspberry Pi OS releases.

To install a build as a service, see [Install](#install): `install.sh --archive <file>` takes a local archive.

## Tools

| Tool | Does |
|---|---|
| `tools/compare.sh capture.bin [UC8\|SC16\|SC16Q11]` | run one IQ file through upstream (Docker) and rustadsb, diff messages and statistics |
| `tools/regen-golden.sh` | regenerate the golden files in `testdata/` from upstream |
| `examples/synth_iq.rs` | synthetic captures: random frames, flights, noise, bit errors, overlaps |
| `tools/replay-beast.py capture.bin PORT` | replay a recorded Beast stream at its original pace |
| `tools/replay-pcap.py capture.pcap MLAT_PORT BEAST_PORT` | replay a `tcpdump` capture of mlat-client results and a Beast flow, per connection, at capture pace |
| `tools/compare-json.py DIR` | compare two `aircraft.json` files field by field |
| `tools/soak.sh DIR` / `tools/soak-report.py` | log a running service once a minute and summarise the log |
| `tools/state-compare.sh` | on a Pi with upstream installed as `/usr/local/bin/readsb`, `./rustadsb-state` and `mlat.pcap` in `~/cmp`: both decoders save state from the replay, then each restarts from the other's `rangeDirs.gz` |

```sh
docker build -t rustadsb-upstream tools/upstream
cargo build --release
tools/compare.sh capture.bin
cargo run --release --example synth_iq -p rustadsb-core -- out.bin --random 300 --count 4000 --flip 20 --overlap 10
```

## Development

- Every change must pass `make test`: formatting, clippy with warnings as errors, unit tests with known vectors
  (CRC, CPR, Mode A/C, Comm-B, WMM), network tests, and the golden tests against upstream.
- `unsafe` is forbidden in the workspace; it would only be considered with a benchmark that proves the need.
- Networking uses tokio, the command line clap.

## Licence

GPL-3.0-or-later (see `COPYING`). The decoding, tracking and output logic is ported from
[wiedehopf/readsb](https://github.com/wiedehopf/readsb) (GPL v3 or later), which incorporates dump1090-fa and
dump1090-mutability (GPL v2 or later) and the original dump1090 (BSD-style). Their authors are credited in
[`NOTICE`](NOTICE): Matthias Wirth, Michael Wolf, Oliver Jowett, FlightAware LLC and Salvatore Sanfilippo, with
dump1090's BSD licence text. The World Magnetic Model data is NOAA's (public domain).
