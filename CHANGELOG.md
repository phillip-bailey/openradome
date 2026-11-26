# Changelog

All notable changes to this project. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project uses [semantic versioning](https://semver.org/) (0.x: anything may still change).
Releases, with downloads for every platform and their `SHA256SUMS`, are on the
[releases page](https://github.com/phillip-bailey/openradome/releases).

## [Unreleased]

### Changed
- Website: the AI section is now "Talk to your receiver."

## [0.6.0] - 2026-11-26

The project is now **OpenRadome**: one name for the project, the repository, the program and its files.

### Changed
- Renamed from rustadsb: the program is `openradome`, the crates `openradome` and `openradome-core`, the
  service `openradome.service`, the options file `/etc/openradome/openradome.toml`, the JSON directory
  `/run/openradome`, the saved state `/var/lib/openradome`, the user and group `openradome`, and the release
  archives `openradome-vX.Y.Z-<platform>`. The dashboard and the MCP server introduce themselves as OpenRadome.
- `install.sh` moves an existing rustadsb install over, once: the options file is converted and checked with
  the new binary first; then the service, the user and group (renamed in place, so file owners are kept),
  `/etc/rustadsb` (options, MCP token, TLS files), the saved state and the udev and DVB-T files become
  OpenRadome's. The old paths stay as links: `/usr/local/bin/rustadsb` (feed clients that start it keep
  working), `/etc/rustadsb`, `/run/rustadsb` (re-created at every boot), and the service answers to
  `rustadsb.service` too. `--rollback` puts the last rustadsb binary back under the new name.
- `install.sh` finds releases under the old name (`rustadsb-vX.Y.Z-...`) as well; its environment variables
  are `OPENRADOME_REPO_URL`, `OPENRADOME_VERSION` and `OPENRADOME_TOKEN` (the `RUSTADSB_` ones still work).
- The dashboard counts processes named `openradome` or `rustadsb` (feed clients started through the old link).
- README: the MCP section rewritten: turning it on, adding it to Claude Code (checking, permissions,
  troubleshooting, removal), every tool and query with example questions, HTTPS on and off, the token's
  generation and rotation.

### Added
- `openradome.example.toml`: a complete, commented configuration. It goes into every release archive, and
  `install.sh` writes it as `/etc/openradome/openradome.toml` on a new install, with the install's gain,
  position, bind address, JSON directory, dashboard and MCP filled in (and their LAN bind lines when
  `--bind 127.0.0.1` keeps the data ports local).
- `docs/`: the OpenRadome website (GitHub Pages).
- `CHANGELOG.md` (this file).

## [0.5.0] - 2026-11-25

### Added
- **MCP server** (`--mcp-port`, off unless given): a Model Context Protocol server at `/mcp`, so AI assistants
  such as Claude Code can ask the station how it is doing. Streamable HTTP (JSON-RPC 2.0, one JSON reply per
  request), protocol versions 2025-11-25 back to 2024-11-05. Six read-only tools: `station_status`, `history`,
  `system`, `connections`, `aircraft_query` (closest, within, box, find_hex, find_callsign, find_squawk, all)
  and `range_outline`.
- MCP security: a Bearer token from `--mcp-token-file` (at least 32 characters, compared in constant time,
  warned about if every user can read it); requests from web pages (`Origin` header) refused; refusals logged
  at most once a minute, without the token; control characters stripped from aircraft strings, which the server
  tells the assistant to treat as data.
- `--mcp-tls-cert` / `--mcp-tls-key`: HTTPS only (rustls with ring, TLS 1.2 and 1.3); a renewed certificate
  is picked up within 10 s without a restart, and one that fails to load is reported while the previous stays
  in use.
- `--mcp-bind-address`: the MCP server on the LAN while the data ports stay on 127.0.0.1.
- `install.sh --mcp-port`: makes a random token (`/etc/rustadsb/mcp-token`, root:rustadsb 0640) and writes the
  lines into a new config.

### Changed
- The dashboard's figures are collected whenever the dashboard or the MCP server is on; the aircraft list for
  MCP is rebuilt only while someone is asking.
- NOTICE names the new dependencies (serde_json, toml, rustls, tokio-rustls, ring) and their licences.

## [0.4.0] - 2026-11-25

### Added
- **TOML configuration file** (`--config <file.toml>`): one key per long option, `true` for on/off options,
  lists for repeatable ones; the command line overrides the file. Unknown keys, sections and wrong value types
  are errors naming the key.
- `--print-config`: prints the options it is given (and those from `--config`) as such a file.
- `install.sh --migrate-config`: converts an older `/etc/default/rustadsb` into `/etc/rustadsb/rustadsb.toml`
  (rustadsb itself does the conversion, and the result must read back before anything changes); the old file
  is kept as `/etc/default/rustadsb.migrated-<date>`.

### Changed
- `install.sh` writes `/etc/rustadsb/rustadsb.toml` on a new install and runs `rustadsb --config`. Installs
  from before 0.4 keep working from `/etc/default/rustadsb` until migrated.

## [0.3.1] - 2026-11-25

### Added
- `--dashboard-bind-address`: the dashboard's own listening address, so a station that keeps its data ports
  on 127.0.0.1 can still show the page on the LAN.

## [0.3.0] - 2026-11-25

### Added
- **Status dashboard** (`--dashboard-port`, off unless given): one page with live figures (messages per
  second, aircraft, range, positions, signal and noise, strong signals, dropped samples, the process's CPU and
  memory), graphs of the last 24 hours at one point a minute (the time axis grows with the history after a
  restart), and every open connection. Drawn in Rust, HTML and SVG, no JavaScript, nothing loaded from
  elsewhere; read-only.
- System figures on Linux: CPU and load, the CPU of all rustadsb processes together, memory and swap,
  temperature, the Raspberry Pi under-voltage alarm, disk use, Wi-Fi signal, uptime; with CPU and temperature
  graphs.
- `install.sh --dashboard-port`.
- README: "What is ADS-B?", "What rustadsb does" and "Which file to download" (a table by device).

### Changed
- Release archives are named by platform (`linux-armv7`, `macos-arm64`, `windows-x86_64`, ...) instead of by
  Rust target; `install.sh` asks for the plain name first and falls back to the old one.
- GitHub is the project's home: `install.sh` downloads from it, and the Gitea pipeline and
  `tools/release-mac.sh` are gone (GitHub Actions builds macOS too).

### Fixed
- Dashboard: a browser's spare, idle connection was answered with "400 Bad Request", which the browser then
  showed on reload; idle connections are now closed without a reply.
- The network tests could fail on macOS when two of them took the same port; they now run one at a time.

## [0.2.0] - 2026-11-24

First release on GitHub.

### Added
- GitHub Actions: tests on Linux and macOS, then eight builds (Linux x86_64, arm64, armv7 and armv6; Windows
  x86_64 and ARM64; macOS Apple silicon and Intel), and a release with `SHA256SUMS` for a `v*` tag (a tag with
  a `-` is a pre-release).
- The parts of readsb a full station uses beyond 0.1.0: the globe index and traces for tar1090,
  `aircraft.binCraft`, the HTTP API (`--net-api-port`) and interactive mode.
- `install.sh`: installs and upgrades the decoder as a systemd service (its own user, udev rule, DVB-T driver
  kept off the dongle), checks downloads against `SHA256SUMS`, restarts other services still running the old
  binary (feed clients), and has `--rollback` and `--uninstall`.
- NOTICE, crediting readsb, dump1090 and their authors.

## 0.1.0

First release, not published on GitHub: the decoder (demodulator, Mode S, ADS-B, Comm-B and Mode A/C
decoding, CPR positions), RTL-SDR, IQ-file, Beast and raw input, Beast, raw, SBS and `beast_reduce` output,
aircraft tracking, and the JSON files tar1090 reads, all checked against readsb's output.

[Unreleased]: https://github.com/phillip-bailey/openradome/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/phillip-bailey/openradome/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/phillip-bailey/openradome/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/phillip-bailey/openradome/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/phillip-bailey/openradome/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/phillip-bailey/openradome/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/phillip-bailey/openradome/releases/tag/v0.2.0
